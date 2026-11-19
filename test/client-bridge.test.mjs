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
        attach: async (...args) => {
          calls.push(["attach", ...args]);
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
  const target = { serverID: "remote-server", directory };
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

test("completed interrupted executions never interrupt a newer worker during prompt replay or inbox cancellation", async () => {
  for (const race of ["prompt", "cancel-success", "cancel-conflict"]) {
    let accept;
    let settleCancel;
    const s = setup({
      ...(race === "prompt"
        ? {
            prompt: () =>
              new Promise((resolve) => {
                accept = resolve;
              }),
          }
        : {}),
      cancel: () =>
        new Promise((resolve, reject) => {
          settleCancel = () =>
            race === "cancel-conflict" ? reject(new Error("delivered")) : resolve();
        }),
    });
    await s.bridge.start();
    s.engine({ type: "prompt", turn: 1, text: "first" });
    await tick();
    s.engine({ type: "interrupt", turn: 1 });
    await tick();
    s.emit("session.execution.started", {});
    s.emit("session.inbox.delivered", { inboxID: "inbox-1" });
    s.emit("session.execution.interrupted", {});
    background(s, "new-worker");
    await tick();
    if (race === "prompt") accept({ id: "inbox-1" });
    else settleCancel();
    await tick();
    assert.equal(s.calls.filter(([type]) => type === "interrupt").length, 0);
    assert.deepEqual(s.errors, []);
    s.emit("session.text.ended", {
      assistantMessageID: "new-worker",
      ordinal: 0,
      text: "still running",
    });
    await tick();
    assert.deepEqual(s.frames.at(-1), {
      type: "announcement",
      reply: "new-worker:0",
      text: "still running",
    });
    await s.bridge.stop();
  }
});

test("genuine inbox cancellation failure cannot release a replacement prompt", async () => {
  const s = setup({
    cancel: async () => {
      throw new Error("transport failed");
    },
  });
  await s.bridge.start();
  s.engine({ type: "prompt", turn: 1, text: "first" });
  await tick();
  s.engine({ type: "interrupt", turn: 1 });
  s.engine({ type: "prompt", turn: 2, text: "replacement" });
  await tick();
  assert.equal(s.calls.filter(([type]) => type === "prompt").length, 1);
  assert.equal(s.bridge.snapshot().status, "off");
  assert.match(s.errors[0].message, /transport failed/);
  await s.bridge.stop();
});

test("unconfirmed prompt admission fails closed even after Rust cancels before the inbox response", async () => {
  for (const cancelled of [true, false])
    for (const failure of [
      new Error("HTTP response timed out after admission"),
      new DOMException("response aborted", "AbortError"),
      { _tag: "ServiceUnavailableError", message: "server returned 503 after uncertain admission" },
    ]) {
      let reject;
      const s = setup({
        prompt: () =>
          new Promise((_, fail) => {
            reject = fail;
          }),
      });
      await s.bridge.start();
      s.engine({ type: "prompt", turn: 1, text: "possibly accepted" });
      await tick();
      if (cancelled) {
        s.engine({ type: "interrupt", turn: 1 });
        s.engine({ type: "prompt", turn: 2, text: "must not submit" });
      }
      reject(failure);
      await tick();
      assert.equal(s.bridge.snapshot().status, "off");
      assert.match(s.errors[0].message, /unconfirmed.*admission|admission.*unconfirmed/i);
      assert.equal(s.calls.filter(([type]) => type === "prompt").length, 1);
      assert.equal(s.calls.filter(([type]) => type === "stop").length, 1);
      await s.bridge.stop();
    }
});

test("explicit pre-admission request rejection does not manufacture an uncertain accepted turn", async () => {
  for (const _tag of [
    "InvalidRequestError",
    "ConflictError",
    "UnauthorizedError",
    "ForbiddenError",
    "SessionNotFoundError",
    "LocationNotFoundError",
    "ModelNotFoundError",
    "AgentNotFoundError",
  ]) {
    let reject;
    let count = 0;
    const s = setup({
      prompt: () =>
        ++count === 1
          ? new Promise((_, fail) => {
              reject = fail;
            })
          : { id: "second" },
    });
    await s.bridge.start();
    s.engine({ type: "prompt", turn: 1, text: "invalid request" });
    await tick();
    s.engine({ type: "interrupt", turn: 1 });
    s.engine({ type: "prompt", turn: 2, text: "valid replacement" });
    reject({
      _tag,
      message: "Definitively rejected prompt request",
      sessionID: "session",
      providerID: "fixture",
      modelID: "model",
      agentID: "agent",
      directory: "/chosen",
    });
    await tick();
    assert.deepEqual(s.errors, []);
    assert.equal(count, 2);
    await s.bridge.stop();
  }
});

test("UTF-8 clipping only normalizes and encodes the retained source prefix", (t) => {
  const giant = "😀".repeat(2_000_000);
  const originalFrom = Buffer.from;
  const originalReplace = String.prototype.replace;
  let fullInputProcessed = false;
  const from = t.mock.method(Buffer, "from", function (input, ...args) {
    if (typeof input === "string" && input.length > 10) fullInputProcessed = true;
    return originalFrom.call(Buffer, input, ...args);
  });
  const replace = t.mock.method(String.prototype, "replace", function (...args) {
    if (this.length > 10) fullInputProcessed = true;
    return originalReplace.apply(this, args);
  });
  const result = clipUtf8(giant, 8);
  from.mock.restore();
  replace.mock.restore();
  assert.equal(result, "😀😀");
  assert.equal(fullInputProcessed, false, "discarded input must not be normalized or buffered");
});

test("bounded clipping preserves replacement, control, euro and emoji byte semantics", () => {
  for (const text of ["\ud800X", "X\udc00", "😀€X", "\u0000\t\r\n€😀", "\ud800\ud800\udc00"]) {
    for (let limit = 0; limit <= 14; limit++) {
      const clean = text.replace(/\p{Cc}/gu, (char) =>
        char.charCodeAt(0) >= 32 || char === "\n" || char === "\t" ? char : " ",
      );
      const bytes = Buffer.from(clean);
      let end = Math.min(limit, bytes.length);
      while (end > 0 && end < bytes.length && (bytes[end] & 0xc0) === 0x80) end--;
      assert.equal(clipUtf8(text, limit), bytes.subarray(0, end).toString("utf8"));
    }
  }
});

test("delivery conflict before SSE waits for confirmation without cancelling newer work", async () => {
  for (const alreadyTerminal of [false, true]) {
    const s = setup({
      cancel: async () => {
        throw { _tag: "InvalidRequestError", message: "Inbox item already delivered" };
      },
    });
    await s.bridge.start();
    s.engine({ type: "prompt", turn: 1, text: "first" });
    await tick();
    s.engine({ type: "interrupt", turn: 1 });
    s.engine({ type: "prompt", turn: 2, text: "replacement" });
    await tick();
    assert.deepEqual(s.errors, []);
    assert.equal(s.calls.filter(([type]) => type === "prompt").length, 1);
    s.emit("session.execution.started", {});
    s.emit("session.inbox.delivered", { inboxID: "inbox-1" });
    if (alreadyTerminal) {
      s.emit("session.execution.interrupted", {});
      background(s, "new-worker");
    }
    await tick();
    assert.equal(s.calls.filter(([type]) => type === "interrupt").length, alreadyTerminal ? 0 : 1);
    if (!alreadyTerminal) {
      assert.equal(s.calls.filter(([type]) => type === "prompt").length, 1);
      s.emit("session.execution.interrupted", {});
      await tick();
    }
    assert.equal(s.calls.filter(([type]) => type === "prompt").length, 2);
    assert.deepEqual(s.errors, []);
    await s.bridge.stop();
  }
});

test("delivery conflict without SSE confirmation times out closed", async (t) => {
  const s = setup({
    cancel: async () => {
      throw { _tag: "InvalidRequestError", message: "Inbox item already delivered" };
    },
  });
  await s.bridge.start();
  s.engine({ type: "prompt", turn: 1, text: "first" });
  await tick();
  t.mock.timers.enable({ apis: ["setTimeout"] });
  s.engine({ type: "interrupt", turn: 1 });
  s.engine({ type: "prompt", turn: 2, text: "replacement" });
  await tick();
  assert.deepEqual(s.errors, []);
  t.mock.timers.tick(10_000);
  await tick();
  assert.equal(s.calls.filter(([type]) => type === "prompt").length, 1);
  assert.equal(s.bridge.snapshot().status, "off");
  assert.match(s.errors[0].message, /delivery confirmation timed out/);
  await s.bridge.stop();
});

test("interrupt acknowledgement without terminal confirmation times out closed", async (t) => {
  const s = setup();
  await s.bridge.start();
  await begin(s);
  t.mock.timers.enable({ apis: ["setTimeout"] });
  s.engine({ type: "interrupt", turn: 1 });
  s.engine({ type: "prompt", turn: 2, text: "replacement" });
  await tick();
  assert.equal(s.calls.filter(([type]) => type === "prompt").length, 1);
  t.mock.timers.tick(10_000);
  await tick();
  assert.equal(s.calls.filter(([type]) => type === "prompt").length, 1);
  assert.equal(s.bridge.snapshot().status, "off");
  assert.match(s.errors[0].message, /completion timed out/);
  await s.bridge.stop();
});

test("long text overtaking the prompt response is bounded without losing lifecycle correlation", async () => {
  for (const deltas of [false, true]) {
    let accept;
    const s = setup({
      prompt: () =>
        new Promise((resolve) => {
          accept = resolve;
        }),
    });
    await s.bridge.start();
    s.engine({ type: "prompt", turn: 1, text: "first" });
    await tick();
    s.emit("session.execution.started", {});
    s.emit("session.inbox.delivered", { inboxID: "ours" });
    s.emit("session.step.started", { assistantMessageID: "ours" });
    if (deltas)
      for (let i = 0; i < 6000; i++) {
        s.emit("session.text.delta", {
          assistantMessageID: "ours",
          ordinal: 0,
          delta: "😀".repeat(100),
        });
      }
    s.emit("session.text.ended", {
      assistantMessageID: "ours",
      ordinal: 0,
      text: "😀".repeat(400_000),
    });
    s.emit("session.execution.succeeded", {});
    await tick();
    assert.deepEqual(s.errors, []);
    accept({ id: "ours" });
    await tick();
    const end = s.frames.find((frame) => frame.type === "reply_end");
    assert.equal(Buffer.byteLength(end.text), 262_144);
    assert.equal(end.text.includes("�"), false);
    assert.deepEqual(s.frames.at(-1), { type: "turn_end", turn: 1, reason: "completed" });
    assert.deepEqual(s.errors, []);
    await s.bridge.stop();
  }
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

test("native stop rejection still attempts original-location server cleanup and remains a replacement barrier", async () => {
  for (const serverFails of [false, true]) {
    const nativeError = new Error("native termination unconfirmed");
    const s = setup({
      runtime: () => ({
        ready: Promise.resolve(),
        send() {},
        stop: async () => {
          throw nativeError;
        },
      }),
      stop: async () => {
        if (serverFails) throw new Error("original-location cleanup denied");
      },
    });
    await s.bridge.start();
    const stopping = s.bridge.stop();
    assert.equal(s.bridge.stop(), stopping);
    await assert.rejects(stopping, (error) => error === nativeError);
    assert.equal(s.calls.filter(([type]) => type === "stop").length, 1);
    assert.equal(s.errors.length, serverFails ? 1 : 0);
    assert.deepEqual(s.calls.find(([type]) => type === "stop")[2].location, {
      directory: "/chosen",
    });
    assert.equal(s.bridge.snapshot().status, "off");
    s.engine({ type: "prompt", turn: 1, text: "must stay closed" });
    await tick();
    assert.equal(s.calls.filter(([type]) => type === "prompt").length, 0);
  }
});

test("confirmed native stop resolves and reports refused server cleanup without changing location", async () => {
  const s = setup({
    stop: async () => {
      throw new Error("original-location cleanup denied");
    },
  });
  await s.bridge.start();
  await s.bridge.stop();
  assert.equal(s.errors.length, 1);
  assert.match(s.errors[0].message, /cleanup denied/);
  assert.deepEqual(s.calls.find(([type]) => type === "stop")[2].location, { directory: "/chosen" });
  assert.equal(s.stopped(), 1);
});

test("server cleanup allows its ten-second drain plus fixed network grace", async (t) => {
  const s = setup({
    stop: async (_input, options) =>
      new Promise((resolve, reject) => {
        setTimeout(resolve, 10_000);
        options.signal.addEventListener("abort", () => reject(new Error("cleanup deadline")), {
          once: true,
        });
      }),
  });
  await s.bridge.start();
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const budgets = [];
  t.mock.method(AbortSignal, "timeout", (milliseconds) => {
    budgets.push(milliseconds);
    const controller = new AbortController();
    setTimeout(() => controller.abort(), milliseconds);
    return controller.signal;
  });
  const stopping = s.bridge.stop();
  await tick();
  t.mock.timers.tick(10_000);
  await stopping;
  assert.deepEqual(s.errors, []);
  assert.deepEqual(budgets, [15_000]);
  assert.deepEqual(s.calls.find(([type]) => type === "stop")[2].location, { directory: "/chosen" });
});

test("stop publishes its pending promise before off notification and retries native confirmation only", async () => {
  let confirmed = false;
  let observed = false;
  let reentrant;
  const nativeError = new Error("native termination unconfirmed");
  const s = setup({
    runtime: () => ({
      ready: Promise.resolve(),
      send() {},
      stop: async () => {
        if (!confirmed) throw nativeError;
      },
    }),
    onChange(snapshot) {
      if (snapshot.status === "off" && !observed) {
        observed = true;
        reentrant = s.bridge.stop();
        void reentrant.catch(() => {});
      }
    },
  });
  await s.bridge.start();
  const stopping = s.bridge.stop();
  void stopping.catch(() => {});
  assert.equal(reentrant, stopping);
  await assert.rejects(stopping, (error) => error === nativeError);
  const offCount = s.snapshots.filter((snapshot) => snapshot.status === "off").length;
  await assert.rejects(s.bridge.stop(), (error) => error === nativeError);
  confirmed = true;
  await s.bridge.stop();
  assert.equal(s.snapshots.filter((snapshot) => snapshot.status === "off").length, offCount);
});

test("terminal-frame synchronous runtime failure cannot reenter an active turn or strand cleanup", async () => {
  for (const terminal of ["stop", "completion"]) {
    let options;
    let stops = 0;
    let terminalFrames = 0;
    const s = setup({
      runtime: (value) => {
        options = value;
        return {
          ready: Promise.resolve(),
          send(frame) {
            if (frame.type === "turn_end") {
              terminalFrames++;
              if (terminalFrames === 1) options.onError(new Error("outgoing queue overflow"));
            }
          },
          stop: async () => {
            stops++;
          },
        };
      },
    });
    await s.bridge.start();
    await begin(s);
    if (terminal === "completion") {
      s.emit("session.execution.succeeded", {});
      await tick();
    }
    const stopping = s.bridge.stop();
    let timer;
    try {
      await Promise.race([
        stopping,
        new Promise((_, reject) => {
          timer = setTimeout(() => reject(new Error("cleanup remained unsettled")), 100);
        }),
      ]);
    } finally {
      clearTimeout(timer);
    }
    assert.equal(terminalFrames, 1);
    assert.equal(stops, 1);
    assert.equal(s.calls.filter(([type]) => type === "stop").length, 1);
    assert.equal(s.bridge.snapshot().status, "off");
  }
});

test("synchronous terminal send exception still closes and settles the published cleanup barrier", async () => {
  let stops = 0;
  const s = setup({
    runtime: () => ({
      ready: Promise.resolve(),
      send(frame) {
        if (frame.type === "turn_end") throw new Error("send threw");
      },
      stop: async () => {
        stops++;
      },
    }),
  });
  await s.bridge.start();
  await begin(s);
  assert.throws(() => s.bridge.stop(), /send threw/);
  await s.bridge.stop();
  assert.equal(stops, 1);
  assert.equal(s.calls.filter(([type]) => type === "stop").length, 1);
  assert.equal(s.bridge.snapshot().status, "off");
});

test("buffered and live deltas remain a prefix after an omitted buffered delta", async () => {
  let accept;
  const s = setup({
    prompt: () =>
      new Promise((resolve) => {
        accept = resolve;
      }),
  });
  await s.bridge.start();
  s.engine({ type: "prompt", turn: 1, text: "first" });
  await tick();
  s.emit("session.execution.started", {});
  s.emit("session.inbox.delivered", { inboxID: "ours" });
  s.emit("session.step.started", { assistantMessageID: "ours" });
  s.emit("session.text.delta", { assistantMessageID: "ours", ordinal: 0, delta: "prefix" });
  s.emit("session.text.delta", {
    assistantMessageID: "ours",
    ordinal: 0,
    delta: "a".repeat(400_000),
  });
  s.emit("session.text.delta", { assistantMessageID: "ours", ordinal: 0, delta: "buffered-gap" });
  await tick();
  accept({ id: "ours" });
  await tick();
  s.emit("session.text.delta", { assistantMessageID: "ours", ordinal: 0, delta: "live-gap" });
  s.emit("session.text.ended", {
    assistantMessageID: "ours",
    ordinal: 0,
    text: "prefix" + "a".repeat(400_000) + "buffered-gaplive-gap",
  });
  await tick();
  assert.equal(
    s.frames
      .filter((frame) => frame.type === "reply_delta")
      .map((frame) => frame.text)
      .join(""),
    "prefix",
  );
  assert.equal(
    s.frames.find((frame) => frame.type === "reply_end").text,
    "prefix" + "a".repeat(262_144 - 6),
  );
  await s.bridge.stop();
});

test("buffer eviction preserves full completion without replaying a retained suffix", async () => {
  let accept;
  const s = setup({
    prompt: () =>
      new Promise((resolve) => {
        accept = resolve;
      }),
  });
  await s.bridge.start();
  s.engine({ type: "prompt", turn: 1, text: "first" });
  await tick();
  s.emit("session.execution.started", {});
  s.emit("session.inbox.delivered", { inboxID: "ours" });
  s.emit("session.step.started", { assistantMessageID: "ours" });
  s.emit("session.text.delta", { assistantMessageID: "ours", ordinal: 0, delta: "old-prefix" });
  s.emit("session.text.ended", {
    assistantMessageID: "noise",
    ordinal: 0,
    text: "\\".repeat(262_144),
  });
  s.emit("session.text.delta", {
    assistantMessageID: "ours",
    ordinal: 0,
    delta: "buffered-suffix",
  });
  await tick();
  accept({ id: "ours" });
  await tick();
  s.emit("session.text.delta", { assistantMessageID: "ours", ordinal: 0, delta: "live-suffix" });
  s.emit("session.text.ended", {
    assistantMessageID: "ours",
    ordinal: 0,
    text: "old-prefixbuffered-suffixlive-suffix",
  });
  await tick();
  assert.deepEqual(s.errors, []);
  assert.equal(s.frames.filter((frame) => frame.type === "reply_delta").length, 0);
  assert.equal(
    s.frames.find((frame) => frame.type === "reply_end").text,
    "old-prefixbuffered-suffixlive-suffix",
  );
  await s.bridge.stop();
});

test("a multibyte delta that cannot fit closes the prefix before a later ASCII delta", async () => {
  const s = setup();
  await s.bridge.start();
  await begin(s);
  s.emit("session.text.delta", {
    assistantMessageID: "assistant-1",
    ordinal: 0,
    delta: "a".repeat(262_143),
  });
  s.emit("session.text.delta", { assistantMessageID: "assistant-1", ordinal: 0, delta: "😀" });
  s.emit("session.text.delta", { assistantMessageID: "assistant-1", ordinal: 0, delta: "X" });
  s.emit("session.text.ended", {
    assistantMessageID: "assistant-1",
    ordinal: 0,
    text: "a".repeat(262_143) + "😀X",
  });
  await tick();
  assert.ok(
    s.frames
      .filter((frame) => frame.type === "reply_delta")
      .map((frame) => frame.text)
      .join("") === "a".repeat(262_143),
    "stream must retain the exact prefix without the later X",
  );
  assert.ok(s.frames.find((frame) => frame.type === "reply_end").text === "a".repeat(262_143));
  await s.bridge.stop();
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

test("duplicate consumed background delivery cannot erase newer user step ownership", async () => {
  const s = setup();
  await s.bridge.start();
  background(s, "worker");
  s.emit("session.execution.succeeded", {});
  await tick();
  await begin(s);
  enqueue(s, "worker");
  s.emit("session.inbox.delivered", { inboxID: "worker" });
  s.emit("session.step.started", { assistantMessageID: "next-user-step" });
  s.emit("session.text.ended", {
    assistantMessageID: "next-user-step",
    ordinal: 0,
    text: "answer",
  });
  await tick();
  assert.deepEqual(s.frames, [
    { type: "reply_end", turn: 1, reply: "next-user-step:0", text: "answer" },
  ]);
  await s.bridge.stop();
});

test(
  "real Rust fixture speaks a live idle worker summary once through the client transport",
  { timeout: 180_000 },
  async (t) => {
    const root = fileURLToPath(new URL("../", import.meta.url));
    const executable = buildFixture(root);
    const base = join(tmpdir(), "opencode");
    await mkdir(base, { recursive: true });
    const dir = await mkdtemp(join(base, "converse-client-"));
    let proc;
    let closed;
    let trace;
    let transport;
    const s = setup({
      runtime: (options) =>
        (transport = startRuntime({
          ...options,
          spawn: () => {
            proc = spawn(executable, ["run", "--test-fixture"], {
              cwd: root,
              env: fixtureEnvironment(dir),
              stdio: "pipe",
            });
            trace = fixtureTrace(proc, s.errors);
            closed = new Promise((resolve) =>
              proc.once("close", (code, signal) => resolve({ code, signal })),
            );
            return proc;
          },
        })),
    });
    t.after(async () => {
      try {
        await s.bridge.stop();
      } finally {
        try {
          if (proc?.exitCode === null && proc.signalCode === null) proc.kill("SIGKILL");
        } finally {
          await removeAfterClose(dir, { child: proc, closed });
        }
      }
    });
    const wait = async (predicate) => {
      const deadline = Date.now() + 10_000;
      while (!predicate()) {
        assert.equal(s.errors.length, 0, s.errors[0]?.message);
        assert.ok(
          Date.now() < deadline,
          JSON.stringify({
            snapshots: s.snapshots,
            frames: s.frames.map((frame) => ({
              ...frame,
              text: frame.text ? `${Buffer.byteLength(frame.text)} bytes` : undefined,
            })),
          }),
        );
        await new Promise((resolve) => setTimeout(resolve, 5));
      }
    };
    await s.bridge.start();
    await wait(() => s.calls.some(([type]) => type === "prompt"));
    s.emit("session.execution.started", {});
    s.emit("session.inbox.delivered", { inboxID: "inbox-1" });
    s.emit("session.step.started", { assistantMessageID: "user" });
    s.emit("session.text.ended", {
      assistantMessageID: "user",
      ordinal: 0,
      text: "Your worker is running. " + "A short sentence. ".repeat(20_000),
    });
    s.emit("session.execution.succeeded", {});
    await wait(
      () =>
        s.snapshots.some((state) => state.status === "speaking") &&
        s.bridge.snapshot().status === "listening",
    );
    const drain = async (reply, text) => {
      const baseline = s.snapshots.length;
      transport.send({ type: "announcement", reply, text });
      await wait(() => {
        const states = s.snapshots.slice(baseline);
        const speaking = states.findIndex((snapshot) => snapshot.status === "speaking");
        return (
          trace.played.includes(text) &&
          speaking >= 0 &&
          states.slice(speaking + 1).some((snapshot) => snapshot.status === "listening")
        );
      });
    };
    await drain("initial-fixture-drain:0", "Initial fixture drain barrier.");
    const playedBaseline = trace.played.length;
    const before = s.snapshots.filter((state) => state.status === "speaking").length;
    background(s, "worker");
    s.emit("session.text.ended", {
      assistantMessageID: "worker",
      ordinal: 0,
      text: "Your worker has completed. " + "€".repeat(3100),
    });
    s.emit("session.text.ended", {
      assistantMessageID: "worker",
      ordinal: 0,
      text: "Your worker has completed. " + "€".repeat(3100),
    });
    s.emit("session.execution.succeeded", {});
    await wait(
      () =>
        s.snapshots.filter((state) => state.status === "speaking").length > before &&
        s.bridge.snapshot().status === "listening",
    );
    assert.equal(s.snapshots.filter((state) => state.status === "speaking").length, before + 1);
    assert.equal(s.calls.filter(([type]) => type === "prompt").length, 1);
    // Replayed delivery uses a fresh assistant so identity dedupe cannot hide a
    // bridge replay. A consumed FIFO sentinel fences this without cancellation.
    background(s, "worker");
    s.emit("session.step.started", { assistantMessageID: "replayed-worker" });
    s.emit("session.text.ended", {
      assistantMessageID: "replayed-worker",
      ordinal: 0,
      text: "Your worker has completed. " + "€".repeat(3100),
    });
    s.emit("session.execution.succeeded", {});
    await tick();
    const sentinel = "Worker fixture drain barrier.";
    await drain("worker-fixture-drain:0", sentinel);
    // The Rust speech consumer has a 320-byte UTF-8-safe chunk bound. Pin the
    // complete retained summary sequence, not just its scheduling status.
    const euros = Math.floor((8192 - Buffer.byteLength("Your worker has completed. ")) / 3);
    const expectedSummary = [
      "Your worker has completed.",
      ...Array(Math.floor(euros / 106)).fill("€".repeat(106)),
      "€".repeat(euros % 106),
      "The rest is on screen.",
    ];
    assert.deepEqual(trace.played.slice(playedBaseline), [...expectedSummary, sentinel]);
    assert.equal(s.frames.filter((frame) => frame.type === "announcement").length, 1);
    await s.bridge.stop();
    assert.deepEqual(await closed, { code: 0, signal: null });
    await trace.closed;
    assert.deepEqual(trace.played.slice(playedBaseline), [...expectedSummary, sentinel]);
    assert.equal(s.errors.length, 0);
  },
);

test("bridge snapshots authenticated client and location; runtime receives no endpoint", async () => {
  const s = setup();
  s.target.directory = "/changed";
  await s.bridge.start();
  assert.deepEqual(Object.keys(s.runtimeOptions()).sort(), ["onError", "onMessage", "settings"]);
  assert.deepEqual(s.runtimeOptions().settings, { inputDevice: "mic" });
  assert.equal(typeof s.runtimeOptions().onError, "function");
  assert.equal(typeof s.runtimeOptions().onMessage, "function");
  const call = s.calls.find(([type]) => type === "start");
  assert.equal(call[1], undefined);
  assert.deepEqual(call[2].location, { directory: "/chosen" });
  assert.deepEqual(s.bridge.snapshot(), {
    serverID: "remote-server",
    sessionID: "session",
    status: "loading",
    paused: false,
    interrupted: null,
    transcript: [],
  });
  await begin(s);
  assert.deepEqual(s.calls.find(([type]) => type === "prompt")[1], {
    sessionID: "session",
    text: "hello",
  });
  await s.bridge.stop();
  assert.equal(s.stopped(), 1);
  assert.deepEqual(s.calls.find(([type]) => type === "stop")[2].location, { directory: "/chosen" });
});

test("pause is a local spoken-output gate, not a teardown", async () => {
  const s = setup();
  await s.bridge.start();
  s.engine({ type: "status", state: "speaking" });
  s.bridge.pause(true);
  assert.deepEqual(s.frames.at(-1), { type: "pause", on: true });
  assert.equal(s.bridge.snapshot().status, "speaking");
  assert.equal(s.bridge.snapshot().paused, true);
  assert.equal(s.stopped(), 0, "pause must not stop the runtime");

  s.bridge.pause(false);
  assert.deepEqual(s.frames.at(-1), { type: "pause", on: false });
  assert.equal(s.bridge.snapshot().paused, false);
  assert.equal(s.stopped(), 0);

  await s.bridge.stop();
  assert.equal(s.bridge.snapshot().paused, false, "stop clears the pause overlay");
});

test("an interrupt records what was heard for display and clears on the next prompt", async () => {
  const s = setup();
  await s.bridge.start();
  await begin(s);
  s.engine({ type: "interrupt", turn: 1, played_ms: 500, generated_ms: 2000, heard: "Hello" });
  await tick();
  assert.deepEqual(s.bridge.snapshot().interrupted, { heard: "Hello", playedPercent: 25 });
  const sent = JSON.stringify(s.calls);
  assert.ok(!sent.includes('Hello"'), "heard text never reaches the server");
  s.engine({ type: "prompt", turn: 2, text: "next" });
  await tick();
  assert.equal(s.bridge.snapshot().interrupted, null);
});

test("reply correlation ignores reasoning, duplicates, foreign sessions and stale assistants", async () => {
  const s = setup();
  await s.bridge.start();
  await begin(s);
  s.emit("session.reasoning.delta", {
    assistantMessageID: "assistant-1",
    ordinal: 0,
    delta: "private",
  });
  s.emit("session.text.delta", { assistantMessageID: "unknown", ordinal: 0, delta: "stale" });
  s.emit("session.text.delta", {
    sessionID: "other",
    assistantMessageID: "assistant-1",
    ordinal: 0,
    delta: "other",
  });
  const event = s.emit("session.text.delta", {
    assistantMessageID: "assistant-1",
    ordinal: 0,
    delta: "Hi",
  });
  s.emit(event.type, event.data, { id: event.id });
  s.emit("session.text.ended", { assistantMessageID: "assistant-1", ordinal: 0, text: "Hi there" });
  s.emit("session.text.ended", { assistantMessageID: "assistant-1", ordinal: 0, text: "Hi there" });
  s.emit("session.execution.succeeded", {});
  await tick();
  assert.deepEqual(s.frames, [
    { type: "reply_delta", turn: 1, reply: "assistant-1:0", text: "Hi" },
    { type: "reply_end", turn: 1, reply: "assistant-1:0", text: "Hi there" },
    { type: "turn_end", turn: 1, reason: "completed" },
  ]);
  const secondTurn = s.frames.length;
  await begin(s, 2);
  s.emit("session.text.delta", { assistantMessageID: "assistant-1", ordinal: 0, delta: "old" });
  s.emit("session.text.ended", {
    assistantMessageID: "assistant-2",
    ordinal: 0,
    text: "No deltas needed",
  });
  await tick();
  assert.deepEqual(s.frames.slice(secondTurn), [
    {
      type: "reply_end",
      turn: 2,
      reply: "assistant-2:0",
      text: "No deltas needed",
    },
  ]);
  await s.bridge.stop();
});

test("events overtaking prompt response wait for its matching inbox delivery", async () => {
  let accept;
  const s = setup({
    prompt: () =>
      new Promise((resolve) => {
        accept = resolve;
      }),
  });
  await s.bridge.start();
  s.engine({ type: "prompt", turn: 1, text: "hello" });
  s.emit("session.inbox.delivered", { inboxID: "foreign" });
  s.emit("session.step.started", { assistantMessageID: "foreign" });
  s.emit("session.inbox.delivered", { inboxID: "ours" });
  s.emit("session.step.started", { assistantMessageID: "ours" });
  s.emit("session.text.ended", { assistantMessageID: "ours", ordinal: 0, text: "complete answer" });
  await tick();
  assert.deepEqual(s.frames, []);
  accept({ id: "ours" });
  await tick();
  assert.deepEqual(s.frames, [
    { type: "reply_end", turn: 1, reply: "ours:0", text: "complete answer" },
  ]);
  await s.bridge.stop();
});

test("interrupt is scoped to supervisor; stale prompt callbacks cannot revive a turn", async () => {
  let accept;
  const s = setup({
    prompt: () =>
      new Promise((resolve) => {
        accept = resolve;
      }),
  });
  await s.bridge.start();
  s.engine({ type: "prompt", turn: 1, text: "hello" });
  await tick();
  s.engine({ type: "interrupt", turn: 999 });
  assert.equal(s.calls.filter(([type]) => type === "interrupt").length, 0);
  s.engine({ type: "interrupt", turn: 1 });
  await tick();
  assert.equal(s.calls.filter(([type]) => type === "interrupt").length, 0);
  accept({ id: "late" });
  await tick();
  assert.deepEqual(s.calls.find(([type]) => type === "cancel")[1], {
    sessionID: "session",
    inboxID: "late",
  });
  s.emit("session.step.started", { assistantMessageID: "late" });
  s.emit("session.text.ended", { assistantMessageID: "late", ordinal: 0, text: "late" });
  await tick();
  assert.deepEqual(s.frames, [{ type: "turn_end", turn: 1, reason: "interrupted" }]);
  await s.bridge.stop();
});

test("prompt failure and stream EOF never replay a prompt", async () => {
  const failed = setup({
    prompt: async () => {
      throw new Error("offline");
    },
  });
  await failed.bridge.start();
  failed.engine({ type: "prompt", turn: 1, text: "hello" });
  await tick();
  assert.deepEqual(failed.frames, [{ type: "turn_end", turn: 1, reason: "disconnected" }]);
  assert.equal(failed.bridge.snapshot().status, "off");
  assert.match(failed.errors[0].message, /admission.*unconfirmed/);
  await failed.bridge.stop();
  const s = setup();
  await s.bridge.start();
  await begin(s);
  s.streamClose();
  await tick();
  assert.deepEqual(s.frames.at(-1), { type: "turn_end", turn: 1, reason: "disconnected" });
  assert.equal(s.bridge.snapshot().status, "off");
  assert.equal(s.calls.filter(([type]) => type === "prompt").length, 1);
  assert.equal(s.calls.filter(([type]) => type === "subscribe").length, 1);
});

test("stopping during server start cleans up the late session without spawning runtime", async () => {
  let accept;
  const s = setup({
    start: () =>
      new Promise((resolve) => {
        accept = resolve;
      }),
  });
  const starting = s.bridge.start();
  await tick();
  await s.bridge.stop();
  accept({ sessionID: "session" });
  await starting;
  assert.equal(s.calls.filter(([type]) => type === "stop").length, 1);
  assert.equal(s.stopped(), 0);
  assert.equal(s.runtimeStarted(), false);
  assert.equal(s.bridge.snapshot().status, "off");
});

test("server-returned inference tuning reaches the local runtime", async () => {
  const runtime = { asrThreads: 2, ttsThreads: 3 };
  const s = setup({ start: async () => ({ sessionID: "session", runtime }) });
  await s.bridge.start();
  assert.deepEqual(s.runtimeOptions().runtime, runtime);
  await s.bridge.stop();
});

test("snapshot carries a bounded live transcript of prompts, replies and worker updates", async () => {
  const s = setup();
  await s.bridge.start();
  await begin(s);
  assert.deepEqual(s.bridge.snapshot().transcript, [{ role: "user", text: "hello" }]);
  s.emit("session.text.ended", { assistantMessageID: "assistant-1", ordinal: 0, text: "Hi there" });
  await tick();
  assert.deepEqual(s.bridge.snapshot().transcript, [
    { role: "user", text: "hello" },
    { role: "assistant", text: "Hi there" },
  ]);
  assert.deepEqual(s.snapshots.at(-1).transcript.at(-1), { role: "assistant", text: "Hi there" });
  const published = s.snapshots.at(-1).transcript;
  published.push({ role: "user", text: "tamper" });
  assert.equal(s.bridge.snapshot().transcript.length, 2, "published snapshots are copies");
  s.emit("session.execution.succeeded", {});
  await tick();
  background(s, "update-1");
  s.emit("session.text.ended", {
    assistantMessageID: "update-1",
    ordinal: 0,
    text: "Worker finished",
  });
  await tick();
  assert.deepEqual(s.bridge.snapshot().transcript.at(-1), {
    role: "assistant",
    text: "Worker finished",
  });
  await s.bridge.stop();
});

test("transcript rolls at its entry and text limits", async () => {
  const s = setup();
  await s.bridge.start();
  for (let turn = 1; turn <= 60; turn++) {
    s.engine({ type: "prompt", turn, text: turn === 60 ? "é".repeat(5000) : `turn ${turn}` });
    await tick();
    s.emit("session.execution.started", {});
    s.emit("session.inbox.delivered", { inboxID: `inbox-${turn}` });
    s.emit("session.step.started", { assistantMessageID: `assistant-${turn}` });
    s.emit("session.execution.succeeded", {});
    await tick();
  }
  const { transcript } = s.bridge.snapshot();
  assert.equal(transcript.length, 50);
  assert.equal(transcript[0].text, "turn 11");
  const last = transcript.at(-1).text;
  assert.ok(Buffer.byteLength(last) <= 2003);
  assert.ok(last.endsWith("…"));
  await s.bridge.stop();
});

test("attach targets this conversation's supervisor at its selected location", async () => {
  const s = setup();
  await assert.rejects(s.bridge.attach("existing"), /not running/);
  await s.bridge.start();
  await s.bridge.attach("existing");
  const call = s.calls.find(([type]) => type === "attach");
  assert.deepEqual(call[1], { supervisorID: "session", sessionID: "existing" });
  assert.deepEqual(call[2].location, { directory: "/chosen" });
  await s.bridge.stop();
  await assert.rejects(s.bridge.attach("existing"), /not running/);
});
