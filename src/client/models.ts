import { spawn } from "node:child_process";
import { runtimeEnvironment } from "./runtime.js";

const MAX_LINE_BYTES = 16 * 1024;
const MAX_STDERR_BYTES = 2048;

export interface ModelsOptions {
  /** Installed engine executable. */
  executable: string;
  signal?: AbortSignal;
  /** Throttled, human-readable progress, e.g. "Downloading speech models 120/210 MB". */
  onProgress?: (text: string) => void;
  /** Kill the installer when it prints nothing for this long. */
  idleTimeoutMs?: number;
  /** Kill the installer after this long overall (includes waiting for another installer). */
  totalTimeoutMs?: number;
  statusTimeoutMs?: number;
}

const mb = (bytes: number) => Math.round(bytes / 1048576);
const manual = (executable: string) => `"${executable}" models install`;

interface Run {
  events: Record<string, unknown>[];
  code: number | null;
  stderr: string;
}

/** Run one engine subcommand and parse its bounded JSON-lines stdout. */
function run(
  executable: string,
  args: string[],
  options: ModelsOptions,
  limits: { idleMs: number; totalMs: number },
  onEvent: (event: Record<string, unknown>) => void,
): Promise<Run> {
  return new Promise((resolve, reject) => {
    if (options.signal?.aborted) return reject(options.signal.reason ?? new Error("Aborted"));
    const child = spawn(executable, args, {
      stdio: ["ignore", "pipe", "pipe"],
      windowsHide: true,
      env: runtimeEnvironment(),
    });
    const events: Record<string, unknown>[] = [];
    let pending = Buffer.alloc(0);
    let stderr = "";
    let failure: Error | undefined;
    let closed = false;
    let idle: ReturnType<typeof setTimeout> | undefined;
    const kill = (error: Error) => {
      failure ??= error;
      if (!closed) child.kill("SIGKILL");
    };
    const arm = () => {
      clearTimeout(idle);
      idle = setTimeout(
        () => kill(new Error(`no output for ${Math.round(limits.idleMs / 1000)} s`)),
        limits.idleMs,
      );
    };
    const total = setTimeout(
      () => kill(new Error(`did not finish within ${Math.round(limits.totalMs / 60000)} min`)),
      limits.totalMs,
    );
    const onAbort = () => kill(options.signal?.reason ?? new Error("Aborted"));
    options.signal?.addEventListener("abort", onAbort, { once: true });
    arm();
    const line = (bytes: Buffer) => {
      try {
        const value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
        if (!value || typeof value !== "object" || Array.isArray(value))
          throw new Error("not an object");
        events.push(value);
        if (events.length > 4096) events.shift();
        onEvent(value);
      } catch {
        kill(new Error("the engine printed malformed progress output"));
      }
    };
    child.stdout.on("data", (chunk: Buffer) => {
      if (failure) return;
      arm();
      let start = 0;
      for (let i = 0; i < chunk.length; i++) {
        if (chunk[i] !== 10) continue;
        if (pending.length + i - start > MAX_LINE_BYTES)
          return kill(new Error("the engine printed an oversized output line"));
        const bytes = Buffer.concat([pending, chunk.subarray(start, i)]);
        pending = Buffer.alloc(0);
        start = i + 1;
        if (bytes.length) line(bytes);
        if (failure) return;
      }
      if (pending.length + chunk.length - start > MAX_LINE_BYTES)
        return kill(new Error("the engine printed an oversized output line"));
      pending = Buffer.concat([pending, chunk.subarray(start)]);
    });
    child.stderr.on("data", (chunk: Buffer) => {
      stderr = (stderr + chunk.toString("utf8")).slice(-MAX_STDERR_BYTES);
    });
    child.stdout.on("error", () => {});
    child.stderr.on("error", () => {});
    child.on("error", (error) => {
      failure ??= new Error(`could not start the engine: ${error.message}`);
    });
    child.on("close", (code) => {
      closed = true;
      clearTimeout(idle);
      clearTimeout(total);
      options.signal?.removeEventListener("abort", onAbort);
      if (failure) reject(failure);
      else resolve({ events, code, stderr: stderr.trim() });
    });
  });
}

const fallback = (executable: string) => ` To install them manually, run: ${manual(executable)}`;

/** Offline check through the engine: are all pinned speech models installed? */
async function modelsInstalled(options: ModelsOptions): Promise<boolean> {
  const ms = options.statusTimeoutMs ?? 30_000;
  let result: Run;
  try {
    result = await run(
      options.executable,
      ["models", "status", "--json"],
      options,
      { idleMs: ms, totalMs: ms },
      () => {},
    );
  } catch (error) {
    if (options.signal?.aborted) throw error;
    throw new Error(
      `Could not check speech models: ${error instanceof Error ? error.message : String(error)}.${fallback(options.executable)}`,
      { cause: error },
    );
  }
  const status = result.events.find((event) => event.type === "status");
  if (result.code !== 0 || typeof status?.installed !== "boolean")
    throw new Error(
      `Could not check speech models${result.stderr ? `: ${result.stderr}` : ""}.${fallback(options.executable)}`,
    );
  return status.installed;
}

/**
 * Make sure the pinned speech models are installed, running the engine's own
 * installer (verified downloads, atomic install, cross-process locking) when needed.
 */
export async function ensureModels(options: ModelsOptions): Promise<void> {
  if (await modelsInstalled(options)) return;
  options.onProgress?.("Downloading speech models");
  let failedAsset: string | undefined;
  let reason: string | undefined;
  let reported = "";
  const say = (text: string) => {
    if (text === reported) return;
    reported = text;
    options.onProgress?.(text);
  };
  let result: Run;
  try {
    result = await run(
      options.executable,
      ["models", "install", "--json"],
      options,
      { idleMs: options.idleTimeoutMs ?? 120_000, totalMs: options.totalTimeoutMs ?? 45 * 60_000 },
      (event) => {
        if (
          event.type === "progress" &&
          Number.isFinite(event.done) &&
          Number.isFinite(event.total)
        )
          say(
            `Downloading speech models ${mb(event.done as number)}/${mb(event.total as number)} MB`,
          );
        else if (event.type === "waiting")
          say("Waiting for another window to finish installing speech models");
        else if (event.type === "error") {
          if (typeof event.message === "string") reason = event.message.slice(0, 2000);
          if (typeof event.asset === "string") failedAsset = event.asset;
        }
      },
    );
  } catch (error) {
    if (options.signal?.aborted) throw error;
    const detail = error instanceof Error ? error.message : String(error);
    throw new Error(
      `Speech model installation failed${failedAsset ? ` (${failedAsset})` : ""}: ${reason ?? detail}.${fallback(options.executable)}`,
      { cause: error },
    );
  }
  if (result.code !== 0 || !result.events.some((event) => event.type === "done")) {
    const why = reason ?? (result.stderr || `the installer exited with code ${result.code}`);
    throw new Error(
      `Speech model installation failed${failedAsset ? ` (${failedAsset})` : ""}: ${why}.${fallback(options.executable)}`,
    );
  }
}
