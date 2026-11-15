import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { createBridge, clipUtf8 } from "../dist/client/bridge.js";
import { WORKER_UPDATE_MARKER } from "../dist/rpc.js";
import { startRuntime } from "../dist/client/runtime.js";
import { buildFixture } from "./client-cargo-fixture.mjs";
import { fixtureTrace } from "./client-fixture-trace.mjs";
import { removeAfterClose } from "./client-process-cleanup.mjs";
import { fixtureEnvironment } from "./client-fixture-environment.mjs";

const tick = () => new Promise((resolve) => setImmediate(resolve));
function setup({
  prompt,
  start,
  hello,
  list,
  runtime,
  interrupt,
  cancel,
  stop,
  onChange,
  connected = true,
  directory = "/chosen",
} = {}) {
  let runtimeOptions;
  let nextEvent;
  const events = connected
    ? [{ value: { id: "connected", created: 0, type: "server.connected", data: {} }, done: false }]
    : [];
  const calls = [];
  const frames = [];
  const errors = [];
  const snapshots = [];
  let sequence = 0;
  let inbox = 0;
  const stream = {
    [Symbol.asyncIterator]() {
      return this;
    },
    next() {
      return events.length
        ? Promise.resolve(events.shift())
        : new Promise((resolve) => {
            nextEvent = resolve;
          });
    },
  };
  const emit = (type, data, extra = {}) => {
    const n = ++sequence;
    const value = {
      id: `event-${n}`,
      created: n,
      type,
      location: { directory },
      ...(type !== "session.text.delta" && !type.includes("reasoning")
        ? { durable: { aggregateID: "session", seq: n, version: 1 } }
        : {}),
      data: { sessionID: "session", ...data },
      ...extra,
    };
    const result = { value, done: false };
    if (nextEvent) {
      const resolve = nextEvent;
      nextEvent = undefined;
      resolve(result);
    } else events.push(result);
    return value;
  };
  const client = {
    plugin: {
      list: async (...args) => {
        calls.push(["plugin.list", ...args]);
        return list
          ? list(...args)
          : {
              location: { directory },
              data: [
                {
                  id: "converse",
                  source: { type: "sdk" },
                  features: { server: true, rpc: true },
                  state: { status: "active" },
                },
              ],
            };
      },
    },
    rpc(contract) {
      assert.equal(contract.id, "converse");
      return {
        hello: async (...args) => {
          calls.push(["hello", ...args]);
          return typeof hello === "function" ? hello(...args) : (hello ?? { protocol: 1 });
        },
        start: async (...args) => {
          calls.push(["start", ...args]);
          return start ? start(...args) : { sessionID: "session" };
        },
        stop: async (...args) => {
          calls.push(["stop", ...args]);
          return stop?.(...args);
        },
      };
    },
    event: {
      subscribe(options) {
        calls.push(["subscribe", options]);
        options.signal.addEventListener("abort", () => {
          if (nextEvent) {
            nextEvent({ done: true });
            nextEvent = undefined;
          }
        });
        return stream;
      },
    },
    session: {
      prompt: async (...args) => {
        calls.push(["prompt", ...args]);
        return prompt ? prompt(...args) : { id: `inbox-${++inbox}` };
      },
      interrupt: async (...args) => {
        calls.push(["interrupt", ...args]);
        return interrupt?.(...args);
      },
      inbox: {
        cancel: async (...args) => {
          calls.push(["cancel", ...args]);
          return cancel?.(...args);
        },
      },
    },
  };
  const target = {
    serverID: "remote-server",
    directory,
    model: { providerID: "provider", id: "model", variant: "high" },
  };
  let stopped = 0;
  const bridge = createBridge({
    client,
    target,
    settings: { inputDevice: "mic" },
    onChange: (value) => {
      snapshots.push(value);
      onChange?.(value);
    },
    onError: (error) => errors.push(error),
    runtime(options) {
      runtimeOptions = options;
      if (runtime) {
        const transport = runtime(options);
        return {
          ...transport,
          send: (frame) => {
            frames.push(frame);
            transport.send(frame);
          },
        };
      }
      return {
        ready: Promise.resolve(),
        send: (frame) => frames.push(frame),
        stop: async () => {
          stopped++;
        },
      };
    },
  });
  return {
    bridge,
    target,
    client,
    calls,
    emit,
    frames,
    errors,
    snapshots,
    engine: (message) => runtimeOptions.onMessage(message),
    runtimeError: (error) => runtimeOptions.onError(error),
    runtimeStarted: () => runtimeOptions !== undefined,
    runtimeOptions: () => runtimeOptions,
    streamClose() {
      if (nextEvent) nextEvent({ done: true });
    },
    stopped: () => stopped,
  };
}
async function begin(s, turn = 1) {
  s.engine({ type: "prompt", turn, text: "hello" });
  await tick();
  s.emit("session.execution.started", {});
  s.emit("session.inbox.delivered", { inboxID: `inbox-${turn}` });
  s.emit("session.step.started", { assistantMessageID: `assistant-${turn}` });
  await tick();
}

function enqueue(s, inboxID, extra = {}) {
  return s.emit("session.inbox.enqueued", {
    inboxID,
    item: {
      type: "synthetic",
      delivery: "queue",
      payload: { text: "Untrusted worker data", metadata: { [WORKER_UPDATE_MARKER]: true } },
    },
    ...extra,
  });
}

function background(s, id) {
  enqueue(s, id);
  s.emit("session.execution.started", {});
  s.emit("session.inbox.delivered", { inboxID: id });
  s.emit("session.step.started", { assistantMessageID: id });
}

const activePlugin = {
  id: "converse",
  source: { type: "sdk" },
  features: { server: true, rpc: true },
  state: { status: "active" },
};

test("startup waits for the exact location's lazily loaded Converse server and RPC features", async () => {
  let lists = 0;
  const s = setup({
    list: async (input, options) => {
      assert.deepEqual(input, { location: { directory: "/chosen" } });
      assert.ok(options.signal instanceof AbortSignal);
      lists++;
      return {
        data:
          lists === 1
            ? []
            : lists === 2
              ? [{ ...activePlugin, id: "builtin" }]
              : lists === 3
                ? [{ ...activePlugin, features: { server: true } }]
                : [activePlugin],
      };
    },
  });
  const starting = s.bridge.start();
  await tick();
  assert.equal(s.calls.filter(([type]) => type === "hello").length, 0);
  assert.equal(s.calls.filter(([type]) => type === "start").length, 0);
  assert.equal(s.runtimeStarted(), false);
  await starting;
  assert.equal(lists, 4);
  assert.equal(s.calls.filter(([type]) => type === "hello").length, 1);
  await s.bridge.stop();
});

test("failed Converse plugin produces an actionable error without allocating or starting Rust", async () => {
  const s = setup({
    list: async () => ({
      data: [{ ...activePlugin, state: { status: "failed", error: "Invalid server option" } }],
    }),
  });
  await assert.rejects(s.bridge.start(), /Converse.*failed.*Invalid server option/i);
  assert.equal(s.calls.filter(([type]) => type === "start").length, 0);
  assert.equal(s.runtimeStarted(), false);
});

test(
  "missing Converse is not declared absent until the bounded 15 second deadline",
  { timeout: 20_000 },
  async () => {
    const s = setup({ list: async () => ({ data: [{ ...activePlugin, id: "builtin" }] }) });
    const before = Date.now();
    await assert.rejects(s.bridge.start(), /Converse.*15 seconds.*selected server/i);
    assert.ok(Date.now() - before >= 14_900);
    assert.ok(s.calls.filter(([type]) => type === "plugin.list").length > 2);
    assert.equal(s.calls.filter(([type]) => type === "hello").length, 0);
    assert.equal(s.calls.filter(([type]) => type === "start").length, 0);
    assert.equal(s.runtimeStarted(), false);
  },
);

test("stop aborts lazy-plugin backoff and an outstanding readiness read without allocation", async () => {
  for (const blocked of [false, true]) {
    let readSignal;
    const s = setup({
      list: async (_input, options) => {
        readSignal = options.signal;
        return blocked ? new Promise(() => {}) : { data: [] };
      },
    });
    const starting = s.bridge.start();
    await tick();
    const rejection = assert.rejects(starting, /abort|stopped/i);
    await s.bridge.stop();
    await rejection;
    assert.equal(readSignal.aborted, true);
    assert.equal(s.calls.filter(([type]) => type === "start").length, 0);
    assert.equal(s.runtimeStarted(), false);
  }
});

test("reload rpc.unavailable retries only safe readiness and hello calls before a single allocation", async () => {
  let hellos = 0;
  const s = setup({
    hello: async () => {
      if (++hellos < 3)
        throw { _tag: "RpcError", type: "rpc.unavailable", message: "Plugin is reloading" };
      return { protocol: 1 };
    },
  });
  await s.bridge.start();
  assert.equal(hellos, 3);
  assert.equal(s.calls.filter(([type]) => type === "start").length, 1);
  assert.equal(s.calls.filter(([type]) => type === "prompt").length, 0);
  await s.bridge.stop();
  const rejected = setup({
    hello: async () => {
      throw { _tag: "RpcError", type: "rpc.forbidden", message: "Forbidden" };
    },
  });
  await assert.rejects(rejected.bridge.start());
  assert.equal(rejected.calls.filter(([type]) => type === "hello").length, 1);
  assert.equal(rejected.calls.filter(([type]) => type === "start").length, 0);
});
