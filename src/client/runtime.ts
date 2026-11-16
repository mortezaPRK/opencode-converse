import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { homedir } from "node:os";
import { posix, win32 } from "node:path";
import {
  MAX_FRAME_BYTES,
  PROTOCOL_VERSION,
  type EngineMessage,
  type HostMessage,
  type LocalSettings,
} from "../protocol.js";

// Installation layout is versioned independently of the server or project.
export const RUNTIME_VERSION = "0.1.0";
/** Do not inherit host/server tokens, provider keys, or arbitrary loader variables. */
export function runtimeEnvironment(env: NodeJS.ProcessEnv = process.env): NodeJS.ProcessEnv {
  const result: NodeJS.ProcessEnv = {};
  for (const key of [
    "HOME",
    "USERPROFILE",
    "LOCALAPPDATA",
    "XDG_DATA_HOME",
    // Local Linux audio-session routing (also used by WSLg), not server auth.
    "XDG_RUNTIME_DIR",
    "PULSE_SERVER",
    "PIPEWIRE_REMOTE",
    "PIPEWIRE_RUNTIME_DIR",
    "TMPDIR",
    "TEMP",
    "TMP",
    "SystemRoot",
    "LANG",
    "LC_ALL",
  ])
    if (env[key] !== undefined) result[key] = env[key];
  return result;
}
export function runtimePath(
  env: NodeJS.ProcessEnv = process.env,
  platform = process.platform,
  home = homedir(),
): string {
  const { join } = platform === "win32" ? win32 : posix;
  const isAbsolute =
    platform === "win32"
      ? (value: string) => win32.isAbsolute(value) && win32.parse(value).root.length > 1
      : posix.isAbsolute;
  if (env.CONVERSE_BIN) {
    if (!isAbsolute(env.CONVERSE_BIN))
      throw new Error("CONVERSE_BIN must be an absolute local development executable path");
    return env.CONVERSE_BIN;
  }
  const fromHome = (...parts: string[]) => {
    if (!isAbsolute(home)) throw new Error("Converse runtime home must be absolute");
    return join(home, ...parts);
  };
  const data =
    platform === "darwin"
      ? fromHome("Library", "Application Support")
      : platform === "win32"
        ? env.LOCALAPPDATA && isAbsolute(env.LOCALAPPDATA)
          ? env.LOCALAPPDATA
          : fromHome("AppData", "Local")
        : env.XDG_DATA_HOME && isAbsolute(env.XDG_DATA_HOME)
          ? env.XDG_DATA_HOME
          : fromHome(".local", "share");
  return join(
    data,
    "opencode-converse",
    RUNTIME_VERSION,
    platform === "win32" ? "opencode-converse.exe" : "opencode-converse",
  );
}

export function localSettings(value: unknown): LocalSettings {
  if (!value || typeof value !== "object" || Array.isArray(value)) return {};
  const result: LocalSettings = {};
  for (const key of ["inputDevice", "outputDevice"] as const) {
    const item = (value as Record<string, unknown>)[key];
    if (
      typeof item === "string" &&
      item.length > 0 &&
      item.length <= 1024 &&
      Buffer.byteLength(item) <= 1024 &&
      !/\p{Cc}/u.test(item)
    )
      result[key] = item;
  }
  return result;
}

function exact(value: Record<string, unknown>, fields: string[]) {
  return (
    Object.keys(value).length === fields.length && fields.every((key) => Object.hasOwn(value, key))
  );
}
const turn = (value: unknown) => Number.isSafeInteger(value) && (value as number) > 0;
export function parseEngineMessage(value: unknown): EngineMessage {
  if (!value || typeof value !== "object" || Array.isArray(value))
    throw new Error("Invalid Converse frame");
  const v = value as Record<string, unknown>;
  const text = (x: unknown) => typeof x === "string";
  if (v.type === "hello" && exact(v, ["type", "protocol"]) && v.protocol === PROTOCOL_VERSION)
    return v as EngineMessage;
  if (
    v.type === "status" &&
    exact(v, ["type", "state"]) &&
    typeof v.state === "string" &&
    ["loading", "listening", "hearing", "thinking", "speaking", "muted"].includes(v.state)
  )
    return v as EngineMessage;
  if (
    v.type === "prompt" &&
    exact(v, ["type", "turn", "text"]) &&
    turn(v.turn) &&
    text(v.text) &&
    (v.text as string).trim()
  )
    return v as EngineMessage;
  if (v.type === "interrupt" && exact(v, ["type", "turn"]) && turn(v.turn))
    return v as EngineMessage;
  if (
    v.type === "error" &&
    exact(v, ["type", "code", "message", "fatal"]) &&
    text(v.code) &&
    text(v.message) &&
    typeof v.fatal === "boolean"
  )
    return v as EngineMessage;
  throw new Error(
    v.type === "hello"
      ? "Converse protocol version mismatch (expected 1)"
      : "Invalid Converse frame",
  );
}

export interface Runtime {
  ready: Promise<void>;
  send(message: HostMessage): void;
  stop(): Promise<void>;
}
export interface RuntimeOptions {
  settings: LocalSettings;
  onMessage(message: EngineMessage): void;
  onError(error: Error): void;
  spawn?: () => ChildProcessWithoutNullStreams;
  helloTimeoutMs?: number;
  shutdownTimeoutMs?: number;
  killTimeoutMs?: number;
}

export function startRuntime(options: RuntimeOptions): Runtime {
  const child = options.spawn
    ? options.spawn()
    : spawn(runtimePath(), ["run"], {
        stdio: "pipe",
        windowsHide: true,
        env: runtimeEnvironment(),
      });
  let pending: Buffer[] = [];
  let pendingBytes = 0;
  let hello = false;
  let stopping = false;
  let finished = false;
  let failed = false;
  let resolveReady!: () => void;
  let rejectReady!: (error: Error) => void;
  let resolveExit!: () => void;
  const exited = new Promise<void>((resolve) => {
    resolveExit = resolve;
  });
  const ready = new Promise<void>((resolve, reject) => {
    resolveReady = resolve;
    rejectReady = reject;
  });
  // Callers can attach after the child has already reported an error.
  void ready.catch(() => {});
  let stopPromise: Promise<void> | undefined;
  const send = (message: HostMessage) => {
    if (stopping || finished || failed) return;
    // Scan only the fixed protocol's string fields, before allocating serialized
    // JSON or a UTF-8 buffer. Length rejects giant strings without scanning them.
    let bytes = 0;
    const values =
      message.type === "init"
        ? [...Object.values(message), ...Object.values(message.settings)]
        : Object.values(message);
    for (const value of values) {
      if (typeof value !== "string") continue;
      if (value.length > MAX_FRAME_BYTES || (bytes += Buffer.byteLength(value)) > MAX_FRAME_BYTES)
        return fail(new Error("Converse outgoing frame/queue exceeds limit"));
    }
    // Control substitutions preserve character positions across deltas and full
    // completions, and prevent six-byte JSON escapes from inflating speech text.
    const safe =
      "text" in message
        ? {
            ...message,
            text: message.text.replace(/\p{Cc}/gu, (char) =>
              char === "\n" || char === "\t" ? char : " ",
            ),
          }
        : message;
    const frame = Buffer.from(JSON.stringify(safe) + "\n");
    if (
      frame.byteLength > MAX_FRAME_BYTES ||
      child.stdin.writableLength + frame.byteLength > MAX_FRAME_BYTES * 2
    )
      return fail(new Error("Converse outgoing frame/queue exceeds limit"));
    try {
      child.stdin.write(frame);
    } catch (error) {
      fail(error instanceof Error ? error : new Error(String(error)));
    }
  };
  const stop = (): Promise<void> => {
    if (finished) return exited;
    if (stopPromise) return stopPromise;
    stopping = true;
    pending = [];
    pendingBytes = 0;
    clearTimeout(timer);
    if (!hello) rejectReady(new Error("Converse stopped before handshake"));
    if (!finished) {
      // End stdin even if the runtime is not reading; never wait for a drain.
      child.stdin.end(JSON.stringify({ type: "shutdown" }) + "\n");
    }
    stopPromise = (async () => {
      const wait = async (milliseconds: number) => {
        let deadline: ReturnType<typeof setTimeout> | undefined;
        try {
          await Promise.race([
            exited,
            new Promise<void>((resolve) => {
              deadline = setTimeout(resolve, milliseconds);
            }),
          ]);
        } finally {
          clearTimeout(deadline);
        }
      };
      await wait(options.shutdownTimeoutMs ?? 1500);
      if (finished) return;
      let signalError: unknown;
      try {
        if (!child.kill("SIGKILL")) signalError = new Error("SIGKILL was not sent");
      } catch (error) {
        signalError = error;
      }
      // A signal request (even a successful one) does not release audio devices.
      await wait(options.killTimeoutMs ?? 1500);
      if (!finished)
        throw new Error("Converse runtime termination is unconfirmed", { cause: signalError });
    })();
    return stopPromise;
  };
  const fail = (error: Error) => {
    if (failed || stopping) return;
    failed = true;
    clearTimeout(timer);
    rejectReady(error);
    options.onError(error);
    void stop().catch((failure) => options.onError(failure));
  };
  const timer = setTimeout(
    () => fail(new Error("Converse runtime handshake timed out")),
    options.helloTimeoutMs ?? 10_000,
  );
  child.stdout.on("data", (chunk: Buffer) => {
    if (stopping || failed) return;
    // Split before accumulating so even a single enormous chunk stays bounded.
    let start = 0;
    for (let i = 0; i < chunk.length; i++) {
      if (chunk[i] !== 10) continue;
      if (pendingBytes + i - start > MAX_FRAME_BYTES)
        return fail(new Error("Converse frame exceeds limit"));
      if (i > start) pending.push(chunk.subarray(start, i));
      pendingBytes += i - start;
      const frame = Buffer.concat(pending, pendingBytes);
      pending = [];
      pendingBytes = 0;
      start = i + 1;
      try {
        const message = parseEngineMessage(
          JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(frame)),
        );
        if (!hello) {
          if (message.type !== "hello") throw new Error("Converse must send hello first");
          hello = true;
          clearTimeout(timer);
          send({
            type: "init",
            protocol: PROTOCOL_VERSION,
            settings: localSettings(options.settings),
          });
          resolveReady();
        } else {
          if (message.type === "hello") throw new Error("Duplicate Converse hello");
          options.onMessage(message);
          if (message.type === "error" && message.fatal) fail(new Error(message.message));
        }
      } catch (error) {
        return fail(error instanceof Error ? error : new Error(String(error)));
      }
      if (stopping || failed) return;
    }
    if (pendingBytes + chunk.length - start > MAX_FRAME_BYTES)
      return fail(new Error("Converse frame exceeds limit"));
    if (start < chunk.length) {
      // Own only the bounded unfinished fragment, not a potentially large read
      // buffer containing already-consumed frames. No retained-prefix recopying.
      pending.push(Buffer.from(chunk.subarray(start)));
      pendingBytes += chunk.length - start;
    }
  });
  child.stdout.on("end", () => {
    if (!stopping) fail(new Error("Converse runtime stdout closed unexpectedly"));
  });
  child.stdout.on("error", (error) => fail(error));
  // Drain stderr, but do not expose model output or accidental secrets in logs.
  child.stderr.on("data", () => {});
  child.stderr.on("error", (error) => fail(error));
  child.stdin.on("error", (error) => {
    if (!stopping) fail(error);
  });
  child.on("error", (error) =>
    fail(
      new Error(
        `Converse runtime unavailable: ${error.message}. Build from source with npm run engine:build, then set CONVERSE_BIN to the absolute native/target/release/opencode-converse executable path for local development. No prebuilt runtime is installed automatically.`,
      ),
    ),
  );
  child.on("close", () => {
    finished = true;
    clearTimeout(timer);
    resolveExit();
    if (!stopping) fail(new Error("Converse runtime exited unexpectedly"));
  });
  return { ready, send, stop };
}
