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
  readonly agents?: readonly AgentConfig[];
  readonly asrThreads?: number;
  readonly ttsThreads?: number;
  readonly endpointing?: Endpointing;
  readonly acknowledgement?: Acknowledgement;
  readonly echoCancellation?: EchoCancellation;
}

/** Voices shipped with the engine, in assignment order for agents that name none. */
export const VOICES = ["zoey", "maeve", "easton", "preston"] as const;
export type Voice = (typeof VOICES)[number];
export const MAX_AGENTS = 4;
export const AGENT_PROMPT_LIMIT = 1000;
/** Spoken words that already mean something to the person or to this plugin. */
const RESERVED_NAMES = new Set([
  "all",
  "assistant",
  "attach",
  "detach",
  "everyone",
  "mute",
  "pause",
  "resume",
  "start",
  "stop",
  "system",
  "unmute",
  "user",
]);

/** One configured voice agent. Its OpenCode session is created lazily with `agent` and `model`. */
export interface AgentConfig {
  readonly name: string;
  readonly voice?: Voice;
  /** OpenCode primary agent the agent's own session runs as. */
  readonly agent?: string;
  readonly model?: ModelRef;
  /** Extra instructions for this agent only. */
  readonly prompt?: string;
}

/** A roster entry as the engine sees it. `voice` may be `default` for the implicit agent. */
export interface Participant {
  readonly id: string;
  readonly name: string;
  readonly voice: Voice | "default";
}

export const PLUGIN_OPTION_KEYS = [
  "$schema",
  "supervisorModel",
  "workerModel",
  "asrThreads",
  "ttsThreads",
  "endpointing",
  "acknowledgement",
  "echoCancellation",
  "agents",
] as const;

export type Endpointing = "responsive" | "patient";
export type Acknowledgement = "auto" | "off";
export type EchoCancellation = "on" | "off";

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

function endpointingOption(value: unknown): Endpointing | undefined {
  if (value === undefined || value === null) return undefined;
  if (value !== "responsive" && value !== "patient")
    throw new Error('endpointing must be "responsive" or "patient"');
  return value;
}

function acknowledgementOption(value: unknown): Acknowledgement | undefined {
  if (value === undefined || value === null) return undefined;
  if (value !== "auto" && value !== "off")
    throw new Error('acknowledgement must be "auto" or "off"');
  return value;
}

function echoCancellationOption(value: unknown): EchoCancellation | undefined {
  if (value === undefined || value === null) return undefined;
  if (value !== "on" && value !== "off") throw new Error('echoCancellation must be "on" or "off"');
  return value;
}

const AGENT_KEYS = ["name", "voice", "agent", "model", "prompt"] as const;

function agentOption(value: unknown, index: number): AgentConfig {
  const where = `agents[${index}]`;
  let input: Record<string, unknown>;
  try {
    input = object(value);
    keys(input, AGENT_KEYS);
  } catch (error) {
    throw new Error(`${where}: ${(error as Error).message}`);
  }
  const name = input.name;
  if (typeof name !== "string" || !/^[A-Za-z]{2,16}$/.test(name))
    throw new Error(`${where}.name must be 2 to 16 ASCII letters`);
  if (RESERVED_NAMES.has(name.toLowerCase()))
    throw new Error(`${where}.name "${name}" is reserved; choose another spoken name`);
  if (input.voice !== undefined && !VOICES.includes(input.voice as Voice))
    throw new Error(`${where}.voice must be one of ${VOICES.join(", ")}`);
  if (
    input.agent !== undefined &&
    (typeof input.agent !== "string" || !input.agent.trim() || input.agent.length > 128)
  )
    throw new Error(`${where}.agent must be a nonempty OpenCode agent name`);
  if (
    input.prompt !== undefined &&
    (typeof input.prompt !== "string" || input.prompt.length > AGENT_PROMPT_LIMIT)
  )
    throw new Error(`${where}.prompt must be a string of at most ${AGENT_PROMPT_LIMIT} characters`);
  let model: ModelRef | undefined;
  if (input.model !== undefined) {
    try {
      model = parseModelRef(input.model);
    } catch (error) {
      throw new Error(`${where}.model: ${(error as Error).message}`);
    }
  }
  return {
    name,
    ...(input.voice === undefined ? {} : { voice: input.voice as Voice }),
    ...(input.agent === undefined ? {} : { agent: input.agent as string }),
    ...(model ? { model } : {}),
    ...(input.prompt === undefined ? {} : { prompt: input.prompt as string }),
  };
}

function agentsOption(value: unknown): readonly AgentConfig[] | undefined {
  if (value === undefined || value === null) return undefined;
  if (!Array.isArray(value) || value.length < 1 || value.length > MAX_AGENTS)
    throw new Error(`agents must be an array of 1 to ${MAX_AGENTS} agents`);
  const agents = value.map(agentOption);
  const names = new Set<string>();
  const voices = new Set<string>();
  for (const agent of agents) {
    const key = agent.name.toLowerCase();
    if (names.has(key)) throw new Error(`agents: duplicate name "${agent.name}"`);
    names.add(key);
    if (agent.voice) {
      if (voices.has(agent.voice)) throw new Error(`agents: voice "${agent.voice}" is used twice`);
      voices.add(agent.voice);
    }
  }
  return agents;
}

/**
 * The fixed roster for a conversation: ids `a1..a4` in configured order, and a voice for every
 * agent (the first unused shipped voice when none is named). No configuration means one agent
 * speaking the default voice.
 */
export function resolveRoster(agents: readonly AgentConfig[] | undefined): Participant[] {
  if (!agents?.length) return [{ id: "a1", name: "Converse", voice: "default" }];
  const taken = new Set(agents.flatMap((agent) => (agent.voice ? [agent.voice] : [])));
  const free = VOICES.filter((voice) => !taken.has(voice));
  return agents.map((agent, index) => ({
    id: `a${index + 1}`,
    name: agent.name,
    voice: agent.voice ?? free.shift()!,
  }));
}

/** Structural parser shared by the server half and the local UI half. */
export function parseRuntimeConfig(value: unknown): RuntimeConfig {
  const input = value === undefined || value === null ? {} : object(value);
  keys(input, PLUGIN_OPTION_KEYS);
  const asrThreads = threadOption(input.asrThreads, "asrThreads");
  const ttsThreads = threadOption(input.ttsThreads, "ttsThreads");
  const endpointing = endpointingOption(input.endpointing);
  const acknowledgement = acknowledgementOption(input.acknowledgement);
  const echoCancellation = echoCancellationOption(input.echoCancellation);
  const agents = agentsOption(input.agents);
  return {
    ...(agents === undefined ? {} : { agents }),
    ...(asrThreads === undefined ? {} : { asrThreads }),
    ...(ttsThreads === undefined ? {} : { ttsThreads }),
    ...(endpointing === undefined ? {} : { endpointing }),
    ...(acknowledgement === undefined ? {} : { acknowledgement }),
    ...(echoCancellation === undefined ? {} : { echoCancellation }),
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
