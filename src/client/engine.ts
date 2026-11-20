import {
  accessSync,
  chmodSync,
  constants,
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  renameSync,
  rmSync,
  statSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { open } from "node:fs/promises";
import { createHash, randomUUID } from "node:crypto";
import { homedir } from "node:os";
import { dirname, join, posix, win32 } from "node:path";
import { isAbsolutePath } from "../config.js";

/** Engine installation layout version. Must match package.json. */
export const RUNTIME_VERSION = "0.1.1-pr.53.8";
/** Fixed release location. Never taken from options or the environment. */
const RELEASE_BASE = "https://github.com/mortezaPRK/opencode-converse/releases/download";
export const PACKAGE_NAME = "@mortz.prk/opencode-converse";
const STATE_NAME = "opencode-converse";
const COMPLETE = ".complete";
const EXECUTABLE = "opencode-converse";

const SEMVER =
  /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*)?$/;
/** Full semver including prerelease identifiers, without build metadata. */
export const isVersion = (value: unknown): value is string =>
  typeof value === "string" && value.length <= 64 && SEMVER.test(value);
const TARGETS = ["darwin-arm64", "linux-x64", "win32-x64"];
const MAX_ENGINE_BYTES = 256 * 1024 * 1024;
const NOTICES = "THIRD_PARTY.md";

export interface EngineOptions {
  env?: NodeJS.ProcessEnv;
  platform?: NodeJS.Platform;
  arch?: string;
  home?: string;
  packageRoot?: string;
  /** Throttled, human-readable install progress. */
  onProgress?: (text: string) => void;
  signal?: AbortSignal;
  /** Internal test seams. The runtime never sets them; no option or env reaches them. */
  fetch?: typeof fetch;
  releaseBase?: string;
  connectTimeoutMs?: number;
  idleTimeoutMs?: number;
}

const platformPath = (platform: string) => (platform === "win32" ? win32 : posix);
const executableName = (platform: string) =>
  platform === "win32" ? `${EXECUTABLE}.exe` : EXECUTABLE;

/** Release asset name for one target. */
export const engineAssetName = (platform: string, arch: string, version = RUNTIME_VERSION) =>
  `opencode-converse-${version}-${platform}-${arch}${platform === "win32" ? ".exe" : ""}`;

/** Fixed app state folder for the engine, models, downloads, and the engine lock. */
export function stateDir(options: EngineOptions = {}): string {
  const env = options.env ?? process.env;
  const platform = options.platform ?? process.platform;
  const home = options.home ?? homedir();
  const { join } = platformPath(platform);
  if (!isAbsolutePath(home, platform)) throw new Error("Converse home directory must be absolute");
  const base =
    platform === "win32"
      ? env.LOCALAPPDATA && isAbsolutePath(env.LOCALAPPDATA, "win32")
        ? env.LOCALAPPDATA
        : join(home, "AppData", "Local")
      : platform === "darwin"
        ? join(home, "Library", "Application Support")
        : env.XDG_DATA_HOME && isAbsolutePath(env.XDG_DATA_HOME, "linux")
          ? env.XDG_DATA_HOME
          : join(home, ".local", "share");
  return join(base, STATE_NAME);
}

/** Deterministic engine location inside the state folder. */
export function engineExecutable(options: EngineOptions = {}): string {
  const platform = options.platform ?? process.platform;
  const { join } = platformPath(platform);
  return join(stateDir(options), "bin", RUNTIME_VERSION, executableName(platform));
}

function findPackageRoot(start: string): string | undefined {
  let current = start;
  for (let depth = 0; depth < 8; depth++) {
    const candidate = join(current, "package.json");
    if (existsSync(candidate)) {
      try {
        const value = JSON.parse(readFileSync(candidate, "utf8")) as { name?: unknown };
        if (value.name === PACKAGE_NAME) return current;
      } catch {
        // Keep walking; a broken parent package.json is not our package.
      }
    }
    const parent = dirname(current);
    if (parent === current) break;
    current = parent;
  }
  return undefined;
}

function packageRoot(): string {
  const starts: string[] = [];
  if (typeof __dirname === "string") starts.push(__dirname);
  starts.push(process.cwd());
  for (const start of starts) {
    const found = findPackageRoot(start);
    if (found) return found;
  }
  throw new Error(`Cannot locate the ${PACKAGE_NAME} package root to install its engine`);
}

function installed(directory: string, executable: string, platform: string): boolean {
  if (!existsSync(join(directory, COMPLETE))) return false;
  try {
    if (!statSync(executable).isFile()) return false;
    if (platform !== "win32") accessSync(executable, constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

export interface EngineEntry {
  file: string;
  bytes: number;
  sha256: string;
}

/** Read and strictly validate the pinned manifest shipped in the package. */
export function readEngineManifest(root: string): Record<string, EngineEntry> {
  const path = join(root, "assets", "engines.json");
  let value: any;
  try {
    value = JSON.parse(readFileSync(path, "utf8"));
  } catch (error) {
    throw new Error(
      `Cannot read the pinned engine manifest ${path}: ${error instanceof Error ? error.message : String(error)}`,
    );
  }
  const bad = (why: string): never => {
    throw new Error(`Invalid engine manifest ${path}: ${why}`);
  };
  const plain = (v: unknown): v is Record<string, unknown> =>
    typeof v === "object" && v !== null && !Array.isArray(v);
  if (!plain(value) || Object.keys(value).sort().join() !== "engines,version")
    return bad("expected exactly version and engines");
  if (value.version !== RUNTIME_VERSION)
    return bad(`version ${String(value.version)} does not match ${RUNTIME_VERSION}`);
  if (!plain(value.engines)) return bad("engines must be an object");
  const engines: Record<string, EngineEntry> = {};
  for (const [key, entry] of Object.entries(value.engines)) {
    if (!TARGETS.includes(key)) return bad(`unknown target ${key}`);
    if (!plain(entry) || Object.keys(entry).sort().join() !== "bytes,file,sha256")
      return bad(`${key} must have exactly file, bytes, sha256`);
    const { file, bytes, sha256 } = entry;
    const [platform, arch] = key.split("-") as [string, string];
    if (file !== engineAssetName(platform, arch)) return bad(`${key} has an unexpected file name`);
    if (
      !Number.isSafeInteger(bytes) ||
      (bytes as number) <= 0 ||
      (bytes as number) > MAX_ENGINE_BYTES
    )
      return bad(`${key} has an invalid size`);
    if (typeof sha256 !== "string" || !/^[a-f0-9]{64}$/.test(sha256))
      return bad(`${key} has an invalid sha256`);
    engines[key] = { file, bytes: bytes as number, sha256 };
  }
  return engines;
}

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));
const message = (error: unknown) => (error instanceof Error ? error.message : String(error));

function writeNotices(directory: string, root: string | undefined) {
  if (!root) return;
  const source = join(root, "native", NOTICES);
  if (existsSync(source)) copyFileSync(source, join(directory, NOTICES));
}

/** Rename a finished staging directory into place, replacing an incomplete one. */
function publish(staging: string, directory: string, executable: string, platform: string) {
  rmSync(directory, { recursive: true, force: true });
  renameSync(staging, directory);
  if (!installed(directory, executable, platform))
    throw new Error(`Engine installation at ${directory} is incomplete`);
}

async function download(
  options: EngineOptions,
  entry: EngineEntry,
  url: string,
  target: string,
): Promise<void> {
  const connectMs = options.connectTimeoutMs ?? 30_000;
  const idleMs = options.idleTimeoutMs ?? 30_000;
  const abort = new AbortController();
  const onAbort = () => abort.abort(options.signal?.reason);
  options.signal?.addEventListener("abort", onAbort, { once: true });
  if (options.signal?.aborted) onAbort();
  let timer: ReturnType<typeof setTimeout> | undefined;
  let timedOut = "";
  const arm = (ms: number, what: string) => {
    clearTimeout(timer);
    timer = setTimeout(() => {
      timedOut = what;
      abort.abort(new Error(what));
    }, ms);
  };
  const file = await open(target, "w", 0o700);
  try {
    arm(connectMs, `no response within ${connectMs} ms`);
    const response = await (options.fetch ?? fetch)(url, {
      redirect: "follow",
      signal: abort.signal,
    });
    if (!response.ok || !response.body) throw new Error(`HTTP ${response.status}`);
    const declared = response.headers.get("content-length");
    if (declared !== null && Number(declared) !== entry.bytes)
      throw new Error(`size ${declared} does not match the pinned ${entry.bytes} bytes`);
    const hash = createHash("sha256");
    const reader = response.body.getReader();
    const total = (entry.bytes / 1048576).toFixed(0);
    let received = 0;
    let reported = 0;
    for (;;) {
      arm(idleMs, `no data for ${idleMs} ms`);
      const { done, value } = await reader.read();
      if (done) break;
      received += value.byteLength;
      if (received > entry.bytes) {
        await reader.cancel().catch(() => {});
        throw new Error(`more than the pinned ${entry.bytes} bytes`);
      }
      hash.update(value);
      await file.write(value);
      const now = Date.now();
      if (now - reported >= 250) {
        reported = now;
        options.onProgress?.(
          `Downloading voice engine ${(received / 1048576).toFixed(0)}/${total} MB`,
        );
      }
    }
    if (received !== entry.bytes)
      throw new Error(`incomplete download: ${received} of ${entry.bytes} bytes`);
    if (hash.digest("hex") !== entry.sha256)
      throw new Error("SHA-256 does not match the pinned value");
  } catch (error) {
    throw new Error(
      `Failed to download the voice engine from ${url}: ${timedOut || message(error)}`,
      { cause: error },
    );
  } finally {
    clearTimeout(timer);
    options.signal?.removeEventListener("abort", onAbort);
    await file.close().catch(() => {});
  }
}

/** Exclusive cross-process install lock; stale holders (no heartbeat) are taken over. */
async function withInstallLock<T>(
  lock: string,
  options: EngineOptions,
  ready: () => boolean,
  run: () => Promise<T>,
): Promise<T | undefined> {
  const STALE_MS = 60_000;
  for (;;) {
    options.signal?.throwIfAborted();
    try {
      (await open(lock, "wx")).close();
      break;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
      if (ready()) return undefined;
      try {
        if (Date.now() - statSync(lock).mtimeMs > STALE_MS) rmSync(lock, { force: true });
      } catch {
        // Released between checks.
      }
      await sleep(100);
    }
  }
  const beat = setInterval(() => {
    try {
      const now = new Date();
      utimesSync(lock, now, now);
    } catch {
      // Lock removed; the install result is still validated before use.
    }
  }, 5_000);
  try {
    if (ready()) return undefined;
    return await run();
  } finally {
    clearInterval(beat);
    rmSync(lock, { force: true });
  }
}

/**
 * Reuse the fixed engine when complete; otherwise download this version's pinned
 * engine from its GitHub release, verify it, and install it atomically.
 */
export async function ensureEngine(options: EngineOptions = {}): Promise<string> {
  const platform = options.platform ?? process.platform;
  const arch = options.arch ?? process.arch;
  const executable = engineExecutable(options);
  const directory = dirname(executable);
  const ok = () => installed(directory, executable, platform);
  if (ok()) return executable;
  if (!isVersion(RUNTIME_VERSION)) throw new Error(`Invalid engine version ${RUNTIME_VERSION}`);
  const root = options.packageRoot ?? packageRoot();
  const entry = readEngineManifest(root)[`${platform}-${arch}`];
  if (!entry)
    throw new Error(
      `No voice engine is published for ${platform}-${arch} in version ${RUNTIME_VERSION}. ` +
        `Developers can build one locally with npm run engine:build && npm run engine:stage.`,
    );
  const url = `${options.releaseBase ?? RELEASE_BASE}/v${RUNTIME_VERSION}/${entry.file}`;
  const parent = dirname(directory);
  mkdirSync(parent, { recursive: true });
  options.onProgress?.("Downloading voice engine");
  await withInstallLock(join(parent, `.install-${RUNTIME_VERSION}.lock`), options, ok, async () => {
    const staging = join(parent, `.stage-${process.pid}-${randomUUID()}`);
    try {
      mkdirSync(staging);
      const staged = join(staging, executableName(platform));
      await download(options, entry, url, staged);
      if (platform !== "win32") chmodSync(staged, 0o755);
      writeNotices(staging, root);
      writeFileSync(join(staging, COMPLETE), RUNTIME_VERSION);
      publish(staging, directory, executable, platform);
    } finally {
      rmSync(staging, { recursive: true, force: true });
    }
  });
  if (!ok()) throw new Error(`Engine installation at ${directory} is incomplete`);
  return executable;
}

/** Developer flow: install a locally built engine file into the fixed state folder. */
export function installLocalEngine(source: string, options: EngineOptions = {}): string {
  const platform = options.platform ?? process.platform;
  const executable = engineExecutable(options);
  const directory = dirname(executable);
  const parent = dirname(directory);
  mkdirSync(parent, { recursive: true });
  const staging = join(parent, `.stage-${process.pid}-${randomUUID()}`);
  try {
    mkdirSync(staging);
    copyFileSync(source, join(staging, executableName(platform)));
    if (platform !== "win32") chmodSync(join(staging, executableName(platform)), 0o755);
    writeNotices(staging, options.packageRoot ?? packageRoot());
    writeFileSync(join(staging, COMPLETE), RUNTIME_VERSION);
    publish(staging, directory, executable, platform);
    return executable;
  } finally {
    rmSync(staging, { recursive: true, force: true });
  }
}
