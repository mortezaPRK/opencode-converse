import { posix, win32 } from "node:path";

/** OpenCode model reference accepted by plugin options and RPC payloads. */
export interface ModelRef {
  readonly providerID: string;
  readonly id: string;
  readonly variant?: string;
}

/**
 * Local inference tuning. Engine and speech-model locations are fixed under the
 * app state folder and are not configurable.
 */
export interface RuntimeConfig {
  readonly asrThreads?: number;
  readonly ttsThreads?: number;
  readonly useGpu?: boolean;
}

export const PLUGIN_OPTION_KEYS = [
  "$schema",
  "supervisorModel",
  "workerModel",
  "asrThreads",
  "ttsThreads",
  "useGpu",
] as const;

const MAX_THREADS = 256;

export function object(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("Expected an object");
  }
  return value as Record<string, unknown>;
}

export function keys(value: Record<string, unknown>, allowed: readonly string[]): void {
  for (const key of Object.keys(value)) {
    if (!allowed.includes(key)) throw new Error(`Unknown option: ${key}`);
  }
}

export function nonempty(value: unknown): string {
  if (typeof value !== "string" || !value.trim()) throw new Error("Expected a nonempty string");
  return value;
}

export function isAbsolutePath(value: string, platform: string): boolean {
  return platform === "win32"
    ? win32.isAbsolute(value) && win32.parse(value).root.length > 1
    : posix.isAbsolute(value);
}

function threadOption(value: unknown, name: string): number | undefined {
  if (value === undefined || value === null) return undefined;
  if (!Number.isSafeInteger(value) || (value as number) < 1 || (value as number) > MAX_THREADS)
    throw new Error(`${name} must be an integer between 1 and ${MAX_THREADS}`);
  return value as number;
}

function booleanOption(value: unknown, name: string): boolean | undefined {
  if (value === undefined || value === null) return undefined;
  if (typeof value !== "boolean") throw new Error(`${name} must be a boolean`);
  return value;
}

/** Structural parser shared by the server half and the local UI half. */
export function parseRuntimeConfig(value: unknown): RuntimeConfig {
  const input = value === undefined || value === null ? {} : object(value);
  keys(input, PLUGIN_OPTION_KEYS);
  const asrThreads = threadOption(input.asrThreads, "asrThreads");
  const ttsThreads = threadOption(input.ttsThreads, "ttsThreads");
  const useGpu = booleanOption(input.useGpu, "useGpu");
  return {
    ...(asrThreads === undefined ? {} : { asrThreads }),
    ...(ttsThreads === undefined ? {} : { ttsThreads }),
    ...(useGpu === undefined ? {} : { useGpu }),
  };
}

/** Accept `provider/model#variant` and the object form used by existing configs. */
export function parseModelRef(value: unknown): ModelRef {
  if (typeof value === "string") {
    const input = value.trim();
    const providerEnd = input.indexOf("/");
    if (providerEnd <= 0) throw new Error('Expected a model reference like "provider/model"');
    const variantStart = input.indexOf("#", providerEnd + 1);
    const providerID = input.slice(0, providerEnd);
    const id = input.slice(providerEnd + 1, variantStart === -1 ? undefined : variantStart);
    const variant = variantStart === -1 ? undefined : input.slice(variantStart + 1);
    if (
      !id ||
      providerID.includes("#") ||
      (variant !== undefined && (!variant || variant.includes("#")))
    )
      throw new Error(`Invalid model reference: ${input}`);
    return { providerID, id, ...(variant ? { variant } : {}) };
  }
  const input = object(value);
  keys(input, ["providerID", "id", "variant"]);
  return {
    providerID: nonempty(input.providerID),
    id: nonempty(input.id),
    ...(input.variant === undefined ? {} : { variant: nonempty(input.variant) }),
  };
}
