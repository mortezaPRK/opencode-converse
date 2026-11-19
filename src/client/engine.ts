import {
  accessSync,
  chmodSync,
  constants,
  cpSync,
  existsSync,
  mkdirSync,
  readFileSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { randomUUID } from "node:crypto";
import { homedir } from "node:os";
import { dirname, join, posix, win32 } from "node:path";
import { isAbsolutePath } from "../config.js";

/** Engine installation layout version. Must match package.json. */
export const RUNTIME_VERSION = "0.3.0";
export const PACKAGE_NAME = "@mortz.prk/opencode-converse";
const STATE_NAME = "opencode-converse";
const COMPLETE = ".complete";
const EXECUTABLE = "opencode-converse";

export interface EngineOptions {
  env?: NodeJS.ProcessEnv;
  platform?: NodeJS.Platform;
  arch?: string;
  home?: string;
  packageRoot?: string;
}

const platformPath = (platform: string) => (platform === "win32" ? win32 : posix);
const executableName = (platform: string) =>
  platform === "win32" ? `${EXECUTABLE}.exe` : EXECUTABLE;

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

/** Directory shipped inside the npm package for this platform/architecture. */
export function packagedEngineDirectory(options: EngineOptions = {}): string {
  const platform = options.platform ?? process.platform;
  const arch = options.arch ?? process.arch;
  return join(options.packageRoot ?? packageRoot(), "assets", "bin", `${platform}-${arch}`);
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

/**
 * Reuse the fixed engine when complete, otherwise copy the packaged engine into
 * the state folder once. Staging plus rename keeps concurrent starts safe.
 */
export function ensureEngine(options: EngineOptions = {}): string {
  const platform = options.platform ?? process.platform;
  const executable = engineExecutable(options);
  const directory = dirname(executable);
  if (installed(directory, executable, platform)) return executable;
  const source = packagedEngineDirectory(options);
  const sourceExecutable = join(source, executableName(platform));
  if (!existsSync(sourceExecutable))
    throw new Error(
      `No packaged Converse engine for ${platform}-${options.arch ?? process.arch} at ${source}. ` +
        `This installation does not include assets/bin/${platform}-${options.arch ?? process.arch}. ` +
        `Reinstall a package built for this platform, or maintainers can run npm run engine:build and npm run engine:stage before packing.`,
    );
  const parent = dirname(directory);
  mkdirSync(parent, { recursive: true });
  const staging = join(parent, `.stage-${process.pid}-${randomUUID()}`);
  try {
    cpSync(source, staging, { recursive: true, dereference: true });
    const staged = join(staging, executableName(platform));
    if (!existsSync(staged))
      throw new Error(`Packaged engine directory ${source} has no ${executableName(platform)}`);
    if (platform !== "win32") chmodSync(staged, 0o755);
    writeFileSync(join(staging, COMPLETE), RUNTIME_VERSION);
    try {
      renameSync(staging, directory);
    } catch {
      if (installed(directory, executable, platform)) {
        rmSync(staging, { recursive: true, force: true });
        return executable;
      }
      // An incomplete target cannot belong to a running conversation. Replace it.
      rmSync(directory, { recursive: true, force: true });
      try {
        renameSync(staging, directory);
      } catch (retry) {
        if (installed(directory, executable, platform)) {
          rmSync(staging, { recursive: true, force: true });
          return executable;
        }
        throw retry;
      }
    }
    if (!installed(directory, executable, platform))
      throw new Error(`Engine installation at ${directory} is incomplete`);
    return executable;
  } catch (error) {
    rmSync(staging, { recursive: true, force: true });
    throw new Error(
      `Failed to install the Converse engine into ${directory}: ${
        error instanceof Error ? error.message : String(error)
      }`,
      { cause: error },
    );
  }
}
