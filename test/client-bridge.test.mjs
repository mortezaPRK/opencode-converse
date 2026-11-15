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

test("C0 controls cannot expand a clipped reply beyond the JSON frame limit", async () => {
  const s = setup();
  await s.bridge.start();
  await begin(s);
  s.emit("session.text.ended", {
    assistantMessageID: "assistant-1",
    ordinal: 0,
    text: "a\t\nb\r" + "\u0001\u0002\u0000\u001f".repeat(80_000),
  });
  await tick();
  const reply = s.frames.find((frame) => frame.type === "reply_end");
  assert.equal(reply.text.length, 262_144);
  assert.ok(reply.text === "a\t\nb " + " ".repeat(262_144 - 5));
  assert.ok(Buffer.byteLength(JSON.stringify(reply) + "\n") < 1_048_576);
  assert.deepEqual(s.errors, []);
  await s.bridge.stop();
});

test("runtime waits for the authenticated live stream before it can produce a prompt", async () => {
  const s = setup({ connected: false });
  const starting = s.bridge.start();
  await tick();
  assert.equal(s.runtimeStarted(), false);
  s.emit("server.connected", {});
  await starting;
  assert.equal(s.runtimeStarted(), true);
  await s.bridge.stop();
});

test("stream EOF before connection never starts a subprocess or replays work", async () => {
  const s = setup({ connected: false });
  const starting = s.bridge.start();
  await tick();
  s.streamClose();
  await assert.rejects(starting, /event stream closed/);
  assert.equal(s.runtimeStarted(), false);
  assert.equal(s.calls.filter(([type]) => type === "prompt").length, 0);
  assert.equal(s.calls.filter(([type]) => type === "subscribe").length, 1);
  await s.bridge.stop();
});

test("idle worker summary uses only live recognized synthetic delivery and complete text", async () => {
  const s = setup();
  await s.bridge.start();
  background(s, "worker");
  s.emit("session.reasoning.delta", { assistantMessageID: "worker", ordinal: 0, delta: "private" });
  s.emit("session.text.delta", { assistantMessageID: "worker", ordinal: 0, delta: "partial" });
  s.emit("session.text.ended", {
    assistantMessageID: "worker",
    ordinal: 0,
    text: "Worker completed.",
  });
  s.emit("session.text.ended", {
    assistantMessageID: "worker",
    ordinal: 0,
    text: "Worker completed.",
  });
  s.emit("session.execution.succeeded", {});
  await tick();
  assert.deepEqual(s.frames, [
    { type: "announcement", reply: "worker:0", text: "Worker completed." },
  ]);
  assert.equal(s.calls.filter(([type]) => type === "prompt").length, 0);
  // Repeated semantic delivery with new event IDs cannot resurrect a consumed inbox.
  enqueue(s, "worker");
  s.emit("session.inbox.delivered", { inboxID: "worker" });
  s.emit("session.step.started", { assistantMessageID: "resurrected" });
  s.emit("session.text.ended", {
    assistantMessageID: "resurrected",
    ordinal: 0,
    text: "duplicate",
  });
  await tick();
  assert.equal(s.frames.length, 1);
  await s.bridge.stop();
});

test("unrecognized, foreign, undelivered and stale background events never become speech", async () => {
  const s = setup();
  await s.bridge.start();
  for (const [id, extra] of [
    ["foreign-session", { sessionID: "other" }],
    ["ordinary-user", { item: { type: "user", delivery: "queue", payload: { text: "hello" } } }],
    [
      "other-synthetic",
      {
        item: {
          type: "synthetic",
          delivery: "queue",
          payload: { text: "data", description: "Other" },
        },
      },
    ],
    [
      "steer",
      {
        item: {
          type: "synthetic",
          delivery: "steer",
          payload: { text: "data", metadata: { [WORKER_UPDATE_MARKER]: true } },
        },
      },
    ],
  ]) {
    enqueue(s, id, extra);
    s.emit("session.inbox.delivered", { inboxID: id });
    s.emit("session.step.started", { assistantMessageID: id });
    s.emit("session.text.ended", { assistantMessageID: id, ordinal: 0, text: "ignore" });
  }
  enqueue(s, "undelivered");
  s.emit("session.step.started", { assistantMessageID: "undelivered" });
  s.emit("session.text.ended", { assistantMessageID: "undelivered", ordinal: 0, text: "ignore" });
  const stale = enqueue(s, "old");
  background(s, "new");
  s.emit("session.text.ended", { assistantMessageID: "new", ordinal: 0, text: "new summary" });
  s.emit("session.execution.succeeded", {});
  s.emit("session.inbox.delivered", { inboxID: "old" }, { durable: stale.durable });
  s.emit("session.step.started", { assistantMessageID: "fresh-stale-delivery" });
  s.emit("session.text.ended", {
    assistantMessageID: "fresh-stale-delivery",
    ordinal: 0,
    text: "stale delivery must not speak",
  });
  s.emit(
    "session.inbox.enqueued",
    {
      inboxID: "wrong-location",
      item: {
        type: "synthetic",
        delivery: "queue",
        payload: { text: "data", metadata: { [WORKER_UPDATE_MARKER]: true } },
      },
    },
    { location: { directory: "/elsewhere" } },
  );
  s.emit("session.inbox.delivered", { inboxID: "wrong-location" });
  s.emit("session.step.started", { assistantMessageID: "wrong-location" });
  s.emit("session.text.ended", {
    assistantMessageID: "wrong-location",
    ordinal: 0,
    text: "must not be spoken",
  });
  await tick();
  assert.deepEqual(s.frames, [{ type: "announcement", reply: "new:0", text: "new summary" }]);
  assert.match(s.errors[0].message, /moved.*start.*new conversation/i);
  assert.equal(s.bridge.snapshot().status, "off");
  s.engine({ type: "prompt", turn: 1, text: "must not retarget" });
  await tick();
  assert.equal(s.calls.filter(([type]) => type === "prompt").length, 0);
  await s.bridge.stop();
  assert.deepEqual(s.calls.find(([type]) => type === "stop")[2].location, { directory: "/chosen" });
});

test("new user prompt fences background text and its completion cannot close an undelivered user turn", async () => {
  const s = setup();
  await s.bridge.start();
  background(s, "old-background");
  await tick();
  s.engine({ type: "prompt", turn: 1, text: "new request" });
  await tick();
  s.emit("session.text.ended", {
    assistantMessageID: "old-background",
    ordinal: 0,
    text: "stale summary",
  });
  s.emit("session.execution.interrupted", {});
  await tick();
  assert.deepEqual(s.frames, []);
  s.emit("session.execution.started", {});
  s.emit("session.inbox.delivered", { inboxID: "inbox-1" });
  s.emit("session.step.started", { assistantMessageID: "user" });
  s.emit("session.text.ended", { assistantMessageID: "user", ordinal: 0, text: "fresh answer" });
  s.emit("session.execution.succeeded", {});
  await tick();
  assert.deepEqual(s.frames, [
    { type: "reply_end", turn: 1, reply: "user:0", text: "fresh answer" },
    { type: "turn_end", turn: 1, reason: "completed" },
  ]);
  await s.bridge.stop();
});

test("old background terminal replay after new user delivery cannot finish the new execution", async () => {
  const s = setup();
  await s.bridge.start();
  background(s, "background");
  const oldEnd = s.emit("session.execution.succeeded", {});
  await tick();
  await begin(s);
  s.emit(oldEnd.type, oldEnd.data, { durable: oldEnd.durable });
  s.emit("session.text.ended", { assistantMessageID: "background", ordinal: 0, text: "stale" });
  s.emit("session.text.ended", { assistantMessageID: "assistant-1", ordinal: 0, text: "fresh" });
  await tick();
  assert.deepEqual(s.frames, [
    { type: "reply_end", turn: 1, reply: "assistant-1:0", text: "fresh" },
  ]);
  s.emit("session.execution.succeeded", {});
  await tick();
  assert.deepEqual(s.frames.at(-1), { type: "turn_end", turn: 1, reason: "completed" });
  await s.bridge.stop();
});

test("new background execution supersedes only its older delivered user turn", async () => {
  const s = setup();
  await s.bridge.start();
  await begin(s);
  background(s, "worker");
  s.emit("session.text.ended", {
    assistantMessageID: "assistant-1",
    ordinal: 0,
    text: "old user answer",
  });
  s.emit("session.text.ended", {
    assistantMessageID: "worker",
    ordinal: 0,
    text: "worker summary",
  });
  s.emit("session.execution.succeeded", {});
  await tick();
  assert.deepEqual(s.frames, [
    { type: "turn_end", turn: 1, reason: "interrupted" },
    { type: "announcement", reply: "worker:0", text: "worker summary" },
  ]);
  await begin(s, 2);
  s.emit("session.text.ended", {
    assistantMessageID: "assistant-1",
    ordinal: 0,
    text: "still stale",
  });
  s.emit("session.text.ended", { assistantMessageID: "assistant-2", ordinal: 0, text: "current" });
  s.emit("session.execution.succeeded", {});
  await tick();
  assert.deepEqual(s.frames.slice(-2), [
    { type: "reply_end", turn: 2, reply: "assistant-2:0", text: "current" },
    { type: "turn_end", turn: 2, reason: "completed" },
  ]);
  await s.bridge.stop();
});

test("queued but undelivered background survives new user turn; delivered background does not", async () => {
  const s = setup();
  await s.bridge.start();
  background(s, "already-delivered");
  enqueue(s, "queued");
  await tick();
  await begin(s);
  s.emit("session.text.ended", {
    assistantMessageID: "already-delivered",
    ordinal: 0,
    text: "stale",
  });
  s.emit("session.text.ended", {
    assistantMessageID: "assistant-1",
    ordinal: 0,
    text: "user answer",
  });
  s.emit("session.execution.succeeded", {});
  s.emit("session.execution.started", {});
  s.emit("session.inbox.delivered", { inboxID: "queued" });
  s.emit("session.step.started", { assistantMessageID: "queued-reply" });
  s.emit("session.text.ended", {
    assistantMessageID: "queued-reply",
    ordinal: 0,
    text: "fresh worker update",
  });
  s.emit("session.execution.succeeded", {});
  await tick();
  assert.deepEqual(s.frames, [
    { type: "reply_end", turn: 1, reply: "assistant-1:0", text: "user answer" },
    { type: "turn_end", turn: 1, reason: "completed" },
    { type: "announcement", reply: "queued-reply:0", text: "fresh worker update" },
  ]);
  await s.bridge.stop();
});

test("co-delivered background text stays separate while completion owns the delivered user turn", async () => {
  const s = setup();
  await s.bridge.start();
  await begin(s);
  enqueue(s, "co-delivered");
  s.emit("session.inbox.delivered", { inboxID: "co-delivered" });
  s.emit("session.step.started", { assistantMessageID: "summary" });
  s.emit("session.text.ended", { assistantMessageID: "summary", ordinal: 0, text: "summary" });
  s.emit("session.execution.succeeded", {});
  await tick();
  assert.deepEqual(s.frames, [
    { type: "announcement", reply: "summary:0", text: "summary" },
    { type: "turn_end", turn: 1, reason: "completed" },
  ]);
  await s.bridge.stop();
});

test("interrupted user assistants never become idle announcements", async () => {
  const s = setup();
  await s.bridge.start();
  await begin(s);
  s.engine({ type: "interrupt", turn: 1 });
  s.emit("session.execution.interrupted", {});
  background(s, "worker");
  s.emit("session.step.started", { assistantMessageID: "assistant-1" });
  s.emit("session.text.ended", {
    assistantMessageID: "assistant-1",
    ordinal: 0,
    text: "stale user",
  });
  s.emit("session.text.ended", { assistantMessageID: "worker", ordinal: 0, text: "worker" });
  await tick();
  assert.deepEqual(s.frames, [
    { type: "turn_end", turn: 1, reason: "interrupted" },
    { type: "announcement", reply: "worker:0", text: "worker" },
  ]);
  await s.bridge.stop();
});

test("hearing and mute fence already-delivered background replies before another prompt exists", async () => {
  for (const activity of ["hearing", "mute"]) {
    const s = setup();
    await s.bridge.start();
    background(s, "worker");
    await tick();
    if (activity === "mute") s.bridge.mute();
    else s.engine({ type: "status", state: "hearing" });
    s.emit("session.text.ended", { assistantMessageID: "worker", ordinal: 0, text: "stale" });
    await tick();
    assert.equal(s.frames.filter((frame) => frame.type === "announcement").length, 0);
    await s.bridge.stop();
  }
});

test("finished inbox history rolls without ending a long conversation or replaying old durable events", async () => {
  const s = setup();
  await s.bridge.start();
  const old = enqueue(s, "old");
  s.emit("session.inbox.delivered", { inboxID: "old" });
  for (let i = 0; i <= 4100; i++) {
    enqueue(s, `inbox-${i}`);
    s.emit("session.inbox.delivered", { inboxID: `inbox-${i}` });
  }
  await tick();
  assert.deepEqual(s.errors, []);
  assert.notEqual(s.bridge.snapshot().status, "off");
  s.emit(old.type, old.data, { durable: old.durable });
  s.emit("session.inbox.delivered", { inboxID: "old" });
  s.emit("session.step.started", { assistantMessageID: "old-replay" });
  s.emit("session.text.ended", { assistantMessageID: "old-replay", ordinal: 0, text: "stale" });
  await tick();
  assert.deepEqual(s.frames, []);
  await s.bridge.stop();
});

test("interruption finishes before a newer prompt is submitted and unconfirmed failure closes the bridge", async () => {
  let release;
  const s = setup({
    interrupt: () =>
      new Promise((resolve) => {
        release = resolve;
      }),
  });
  await s.bridge.start();
  await begin(s);
  s.engine({ type: "interrupt", turn: 1 });
  s.engine({ type: "prompt", turn: 2, text: "next" });
  await tick();
  assert.equal(s.calls.filter(([type]) => type === "prompt").length, 1);
  release();
  await tick();
  assert.equal(s.calls.filter(([type]) => type === "prompt").length, 1);
  s.emit("session.execution.interrupted", {});
  await tick();
  assert.equal(s.calls.filter(([type]) => type === "prompt").length, 2);
  s.emit("session.execution.started", {});
  s.emit("session.inbox.delivered", { inboxID: "inbox-2" });
  s.emit("session.step.started", { assistantMessageID: "new" });
  s.emit("session.text.ended", { assistantMessageID: "new", ordinal: 0, text: "next answer" });
  await tick();
  assert.deepEqual(s.frames.at(-1), {
    type: "reply_end",
    turn: 2,
    reply: "new:0",
    text: "next answer",
  });
  await s.bridge.stop();

  const failed = setup({
    interrupt: async () => {
      throw new Error("transient interrupt");
    },
  });
  await failed.bridge.start();
  await begin(failed);
  failed.engine({ type: "interrupt", turn: 1 });
  failed.engine({ type: "prompt", turn: 2, text: "continue" });
  await tick();
  assert.equal(failed.calls.filter(([type]) => type === "prompt").length, 1);
  assert.equal(failed.bridge.snapshot().status, "off");
  assert.equal(failed.stopped(), 1);
  await failed.bridge.stop();
});

test("a canceled prompt still awaiting its inbox ID is canceled before the next submission", async () => {
  let accept;
  let release;
  let count = 0;
  const s = setup({
    prompt: async () =>
      ++count === 1
        ? new Promise((resolve) => {
            accept = resolve;
          })
        : { id: "second" },
    cancel: () =>
      new Promise((resolve) => {
        release = resolve;
      }),
  });
  await s.bridge.start();
  s.engine({ type: "prompt", turn: 1, text: "queued" });
  await tick();
  s.engine({ type: "interrupt", turn: 1 });
  s.engine({ type: "prompt", turn: 2, text: "next" });
  await tick();
  assert.equal(count, 1);
  accept({ id: "queued-first" });
  await tick();
  assert.deepEqual(s.calls.find(([type]) => type === "cancel")[1], {
    sessionID: "session",
    inboxID: "queued-first",
  });
  assert.equal(count, 1);
  release();
  await tick();
  assert.equal(count, 2);
  assert.equal(s.calls.filter(([type]) => type === "interrupt").length, 0);
  await s.bridge.stop();
});

test("delivery winning queued cancellation is interrupted before submitting the next user prompt", async () => {
  let rejectCancel;
  let finishInterrupt;
  const s = setup({
    cancel: () =>
      new Promise((_resolve, reject) => {
        rejectCancel = reject;
      }),
    interrupt: () =>
      new Promise((resolve) => {
        finishInterrupt = resolve;
      }),
  });
  await s.bridge.start();
  s.engine({ type: "prompt", turn: 1, text: "first" });
  await tick();
  s.engine({ type: "interrupt", turn: 1 });
  s.engine({ type: "prompt", turn: 2, text: "second" });
  await tick();
  s.emit("session.execution.started", {});
  s.emit("session.inbox.delivered", { inboxID: "inbox-1" });
  s.emit("session.step.started", { assistantMessageID: "old" });
  await tick();
  rejectCancel(new Error("inbox already delivered"));
  await tick();
  assert.deepEqual(s.calls.find(([type]) => type === "interrupt")[1], {
    sessionID: "session",
  });
  assert.equal(s.calls.filter(([type]) => type === "prompt").length, 1);
  s.emit("session.execution.interrupted", {});
  finishInterrupt();
  await tick();
  assert.equal(s.calls.filter(([type]) => type === "prompt").length, 2);
  s.emit("session.execution.started", {});
  s.emit("session.inbox.delivered", { inboxID: "inbox-2" });
  s.emit("session.step.started", { assistantMessageID: "new" });
  s.emit("session.text.ended", { assistantMessageID: "old", ordinal: 0, text: "stale" });
  s.emit("session.text.ended", { assistantMessageID: "new", ordinal: 0, text: "fresh" });
  await tick();
  assert.deepEqual(s.frames, [
    { type: "turn_end", turn: 1, reason: "interrupted" },
    { type: "reply_end", turn: 2, reply: "new:0", text: "fresh" },
  ]);
  await s.bridge.stop();
});

test("forty thousand deltas and thousands of completed steps keep running without stale replay", async () => {
  const s = setup();
  await s.bridge.start();
  await begin(s);
  const oldStep = s.emit("session.step.started", { assistantMessageID: "assistant-1" });
  const old = s.emit("session.text.delta", {
    assistantMessageID: "assistant-1",
    ordinal: 0,
    delta: "a",
  });
  for (let i = 0; i < 40_010; i++)
    s.emit("session.text.delta", { assistantMessageID: "assistant-1", ordinal: 0, delta: "b" });
  await tick();
  assert.deepEqual(s.errors, []);
  const before = s.frames.length;
  s.emit(old.type, old.data, { id: old.id });
  await tick();
  assert.equal(s.frames.length, before);
  s.emit("session.text.ended", {
    assistantMessageID: "assistant-1",
    ordinal: 0,
    text: "a" + "b".repeat(40_010),
  });
  await tick();
  assert.deepEqual(
    s.frames.filter((frame) => frame.type === "reply_end" && frame.reply === "assistant-1:0"),
    [{ type: "reply_end", turn: 1, reply: "assistant-1:0", text: "a" + "b".repeat(40_010) }],
  );
  for (let i = 0; i < 4100; i++) {
    s.emit("session.step.started", { assistantMessageID: `step-${i}` });
    s.emit("session.text.ended", { assistantMessageID: `step-${i}`, ordinal: 0, text: "reply" });
  }
  await tick();
  assert.deepEqual(s.errors, []);
  assert.notEqual(s.bridge.snapshot().status, "off");
  assert.equal(s.frames.at(-1).reply, "step-4099:0");
  const after = s.frames.length;
  s.emit(oldStep.type, oldStep.data, { durable: oldStep.durable });
  s.emit("session.text.delta", { assistantMessageID: "assistant-1", ordinal: 0, delta: "stale" });
  await tick();
  assert.equal(s.frames.length, after);
  await s.bridge.stop();
});

test("UTF-8 reply and announcement clipping is cumulative, codepoint safe and nonfatal", async () => {
  const s = setup();
  await s.bridge.start();
  await begin(s);
  for (let i = 0; i < 10; i++)
    s.emit("session.text.delta", {
      assistantMessageID: "assistant-1",
      ordinal: 0,
      delta: "😀".repeat(10_000),
    });
  s.emit("session.text.ended", {
    assistantMessageID: "assistant-1",
    ordinal: 0,
    text: "😀".repeat(80_000),
  });
  s.emit("session.execution.succeeded", {});
  background(s, "large");
  s.emit("session.text.ended", { assistantMessageID: "large", ordinal: 0, text: "€".repeat(3100) });
  await tick();
  assert.ok(
    s.frames
      .filter((frame) => frame.type === "reply_delta")
      .reduce((n, frame) => n + Buffer.byteLength(frame.text), 0) <= 262_144,
  );
  assert.ok(
    Buffer.byteLength(s.frames.find((frame) => frame.type === "reply_end").text) <= 262_144,
  );
  const announcement = s.frames.find((frame) => frame.type === "announcement");
  assert.ok(Buffer.byteLength(announcement.text) <= 8192);
  assert.ok(s.frames.every((frame) => !frame.text?.includes("�")));
  assert.ok(
    s.frames
      .filter((frame) => frame.type === "reply_delta")
      .map((frame) => frame.text)
      .join("") === "😀".repeat(65_536),
    "cumulative deltas must retain the exact nonempty prefix",
  );
  assert.ok(
    s.frames.find((frame) => frame.type === "reply_end").text === "😀".repeat(65_536),
    "completion must retain the exact prefix",
  );
  assert.equal(announcement.text, "€".repeat(2730));
  assert.deepEqual(s.errors, []);
  await s.bridge.stop();
});

test("movement and literal alias-location events close immediately, even before prompt response or sequence replay", async () => {
  for (const moved of [false, true])
    for (const pending of [false, true]) {
      let accept;
      const s = setup({
        directory: "/tmp/chosen",
        ...(pending
          ? {
              prompt: () =>
                new Promise((resolve) => {
                  accept = resolve;
                }),
            }
          : {}),
      });
      await s.bridge.start();
      assert.ok(s.calls.find(([type]) => type === "subscribe")[1].types.includes("session.moved"));
      s.engine({ type: "prompt", turn: 1, text: "hello" });
      await tick();
      s.emit("session.execution.started", {});
      s.emit(
        moved ? "session.moved" : "session.text.delta",
        moved
          ? { location: { directory: "/elsewhere" }, projectID: "project" }
          : { assistantMessageID: "alias", ordinal: 0, delta: "must not speak" },
        {
          location: { directory: moved ? "/tmp/chosen" : "/private/tmp/chosen" },
          durable: { aggregateID: "session", seq: 0, version: 1 },
        },
      );
      await tick();
      assert.match(s.errors[0].message, /moved.*start.*new conversation/i);
      assert.equal(s.bridge.snapshot().status, "off");
      assert.equal(
        s.frames.some((frame) => frame.type === "reply_delta" || frame.type === "announcement"),
        false,
      );
      s.engine({ type: "prompt", turn: 2, text: "cannot retarget" });
      if (pending) accept({ id: "inbox-1" });
      await tick();
      assert.equal(s.calls.filter(([type]) => type === "prompt").length, 1);
      assert.deepEqual(s.calls.find(([type]) => type === "stop")[2].location, {
        directory: "/tmp/chosen",
      });
      assert.equal(s.bridge.snapshot().serverID, "remote-server");
      await s.bridge.stop();
    }
});

test("failed prompt preserves buffered fresh worker announcements", async () => {
  let reject;
  const s = setup({
    prompt: () =>
      new Promise((_resolve, fail) => {
        reject = fail;
      }),
  });
  await s.bridge.start();
  s.engine({ type: "prompt", turn: 1, text: "user" });
  await tick();
  background(s, "worker");
  s.emit("session.text.ended", { assistantMessageID: "worker", ordinal: 0, text: "summary" });
  await tick();
  reject({ _tag: "InvalidRequestError", message: "request rejected" });
  await tick();
  assert.deepEqual(s.frames, [
    { type: "prompt_failed", turn: 1, message: "request rejected" },
    { type: "announcement", reply: "worker:0", text: "summary" },
  ]);
  await s.bridge.stop();
});

test("server start allocation is not aborted by local stop and its late session is cleaned", async () => {
  let accept;
  let signal;
  const s = setup({
    start: (_input, options) => {
      signal = options.signal;
      return new Promise((resolve) => {
        accept = resolve;
      });
    },
  });
  const starting = s.bridge.start();
  await tick();
  await s.bridge.stop();
  assert.equal(signal.aborted, false);
  accept({ sessionID: "session" });
  await starting;
  assert.equal(s.calls.filter(([type]) => type === "stop").length, 1);
});

test("preempted background events overtaking the new prompt response retain their old ownership", async () => {
  let accept;
  const s = setup({
    prompt: () =>
      new Promise((resolve) => {
        accept = resolve;
      }),
  });
  await s.bridge.start();
  background(s, "old");
  await tick();
  s.engine({ type: "prompt", turn: 1, text: "new user" });
  s.emit("session.text.ended", { assistantMessageID: "old", ordinal: 0, text: "stale" });
  s.emit("session.execution.interrupted", {});
  s.emit("session.execution.started", {});
  s.emit("session.inbox.delivered", { inboxID: "ours" });
  s.emit("session.step.started", { assistantMessageID: "ours" });
  s.emit("session.text.ended", { assistantMessageID: "ours", ordinal: 0, text: "new answer" });
  s.emit("session.execution.succeeded", {});
  await tick();
  assert.deepEqual(s.frames, []);
  accept({ id: "ours" });
  await tick();
  assert.deepEqual(s.frames, [
    { type: "reply_end", turn: 1, reply: "ours:0", text: "new answer" },
    { type: "turn_end", turn: 1, reason: "completed" },
  ]);
  await s.bridge.stop();
});

test("buffered background delivery keeps its receipt generation across hearing or mute", async () => {
  for (const activity of ["hearing", "mute"]) {
    let accept;
    const s = setup({
      prompt: () =>
        new Promise((resolve) => {
          accept = resolve;
        }),
    });
    await s.bridge.start();
    s.engine({ type: "prompt", turn: 1, text: "user" });
    background(s, "worker");
    s.emit("session.text.ended", { assistantMessageID: "worker", ordinal: 0, text: "old summary" });
    s.emit("session.execution.succeeded", {});
    await tick();
    if (activity === "mute") s.bridge.mute();
    else s.engine({ type: "status", state: "hearing" });
    accept({ id: "ours" });
    await tick();
    assert.deepEqual(
      s.frames.filter((frame) => frame.type === "announcement"),
      [],
    );
    assert.deepEqual(
      s.frames.filter((frame) => frame.type === "turn_end"),
      [],
    );
    await s.bridge.stop();
  }
});
