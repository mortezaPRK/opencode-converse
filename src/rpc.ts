import { Rpc } from "@opencode/plugin/rpc";
import {
  keys,
  nonempty,
  object,
  parseModelRef,
  parseRuntimeConfig,
  type ModelRef,
  type RuntimeConfig,
} from "./config.js";

export { keys, nonempty, object, parseModelRef };
export type { ModelRef, RuntimeConfig };

export const WORKER_UPDATE_MARKER = "converse.worker-update";

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
      version: 1,
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
      output: schema<{ protocol: 3; packageRoot?: string }>((value) => {
        const input = object(value);
        keys(input, ["protocol", "packageRoot"]);
        if (input.protocol !== 3) throw new Error("Unsupported Converse protocol");
        return {
          protocol: 3,
          ...(input.packageRoot === undefined ? {} : { packageRoot: nonempty(input.packageRoot) }),
        };
      }),
    },
    start: {
      input: schema<{ participant?: string } | undefined>((value) => {
        if (value === undefined || value === null) return undefined;
        const input = object(value);
        keys(input, ["participant"]);
        return input.participant === undefined
          ? undefined
          : { participant: nonempty(input.participant) };
      }),
      output: schema<{ sessionID: string; runtime?: RuntimeConfig }>((value) => {
        const input = object(value);
        keys(input, ["sessionID", "runtime"]);
        return {
          sessionID: nonempty(input.sessionID),
          ...(input.runtime === undefined ? {} : { runtime: parseRuntimeConfig(input.runtime) }),
        };
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
    attach: {
      output: schema<void>((value) => {
        if (value !== undefined && value !== null) throw new Error("attach returns no value");
      }),
      input: schema<{ supervisorID: string; sessionID: string }>((value) => {
        const input = object(value);
        keys(input, ["supervisorID", "sessionID"]);
        return {
          supervisorID: nonempty(input.supervisorID),
          sessionID: nonempty(input.sessionID),
        };
      }),
    },
    turn: {
      output: schema<void>((value) => {
        if (value !== undefined && value !== null) throw new Error("turn returns no value");
      }),
      input: schema<{ supervisorID: string; origin: "user" | "agent" | "none" }>((value) => {
        const input = object(value);
        keys(input, ["supervisorID", "origin"]);
        if (input.origin !== "user" && input.origin !== "agent" && input.origin !== "none")
          throw new Error('origin must be "user", "agent" or "none"');
        return { supervisorID: nonempty(input.supervisorID), origin: input.origin };
      }),
    },
    detach: {
      output: schema<void>((value) => {
        if (value !== undefined && value !== null) throw new Error("detach returns no value");
      }),
      input: schema<{ supervisorID: string; sessionID: string }>((value) => {
        const input = object(value);
        keys(input, ["supervisorID", "sessionID"]);
        return {
          supervisorID: nonempty(input.supervisorID),
          sessionID: nonempty(input.sessionID),
        };
      }),
    },
  },
});
