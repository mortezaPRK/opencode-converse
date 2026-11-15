import { Rpc } from "@opencode/plugin/rpc";

export const WORKER_UPDATE_MARKER = "converse.worker-update";

export interface ModelRef {
  readonly providerID: string;
  readonly id: string;
  readonly variant?: string;
}

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

export function parseModelRef(value: unknown): ModelRef {
  const input = object(value);
  keys(input, ["providerID", "id", "variant"]);
  return {
    providerID: nonempty(input.providerID),
    id: nonempty(input.id),
    ...(input.variant === undefined ? {} : { variant: nonempty(input.variant) }),
  };
}

/** Small Standard Schema validators keep the public RPC typed without runtime SDK imports. */
function schema<T>(parse: (value: unknown) => T): {
  "~standard": {
    version: 1;
    vendor: string;
    types?: { input: T; output: T };
    validate(value: unknown): { value: T } | { issues: { message: string }[] };
  };
} {
  return {
    "~standard": {
      version: 1 as const,
      vendor: "converse",
      validate(value: unknown) {
        try {
          return { value: parse(value) };
        } catch (error) {
          return {
            issues: [{ message: error instanceof Error ? error.message : "Invalid input" }],
          };
        }
      },
    },
  };
}

export const Converse = Rpc.define({
  id: "converse",
  events: {},
  methods: {
    hello: {
      input: schema<void>((value) => {
        if (value !== undefined && value !== null) throw new Error("hello takes no input");
      }),
      output: schema<{ protocol: 1 }>((value) => {
        const input = object(value);
        keys(input, ["protocol"]);
        if (input.protocol !== 1) throw new Error("Unsupported Converse protocol");
        return { protocol: 1 };
      }),
    },
    start: {
      input: schema<{ model?: ModelRef }>((value) => {
        const input = object(value);
        keys(input, ["model"]);
        return input.model === undefined ? {} : { model: parseModelRef(input.model) };
      }),
      output: schema<{ sessionID: string }>((value) => {
        const input = object(value);
        keys(input, ["sessionID"]);
        return { sessionID: nonempty(input.sessionID) };
      }),
    },
    stop: {
      output: schema<void>((value) => {
        if (value !== undefined && value !== null) throw new Error("stop returns no value");
      }),
      input: schema<{ sessionID: string }>((value) => {
        const input = object(value);
        keys(input, ["sessionID"]);
        return { sessionID: nonempty(input.sessionID) };
      }),
    },
  },
});
