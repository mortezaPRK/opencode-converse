import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { EventEmitter } from "node:events";
import { constants } from "node:fs";
import { access, mkdtemp } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { createBridge } from "../../dist/client/bridge.js";
import { startRuntime } from "../../dist/client/runtime.js";
import { WORKER_UPDATE_MARKER } from "../../dist/rpc.js";
import { buildFixture } from "../client-cargo-fixture.mjs";
import { removeAfterClose } from "../client-process-cleanup.mjs";
import { fixtureEnvironment } from "../client-fixture-environment.mjs";
import { fixtureTrace } from "../client-fixture-trace.mjs";

const binary = buildFixture(fileURLToPath(new URL("../../", import.meta.url)));
const model = { providerID: "fixture-provider", id: "fixture-model", variant: "fixture" };
const sessionID = "ses_bridge_fixture";

async function deadline(promise, description, milliseconds = 5000) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(`Timed out: ${description}`)), milliseconds);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

/** Only the server is fake. Its inbox, event ordering, and RPC signatures match SDK 2.3.0. */
function server(directory, changed) {
  const calls = [];
  const pending = [];
  const drainWaiters = [];
  let waiting;
  let closed = false;
  let sequence = 0;
  let signal;
  let subscribedTypes;
  let connected = false;
  const publish = (event) => {
    const result = { value: event, done: false };
    if (waiting) {
      const resolve = waiting;
      waiting = undefined;
      resolve(result);
    } else pending.push(result);
  };
  const drained = () => {
    for (const resolve of drainWaiters.splice(0)) resolve();
  };
  const close = () => {
    closed = true;
    pending.length = 0;
    if (waiting) waiting({ done: true });
    waiting = undefined;
    drained();
  };
  const stream = {
    [Symbol.asyncIterator]() {
      return this;
    },
    next() {
      if (closed) return Promise.resolve({ done: true });
      if (pending.length) return Promise.resolve(pending.shift());
      return new Promise((resolve) => {
        waiting = resolve;
        drained();
      });
    },
    return() {
      close();
      return Promise.resolve({ done: true });
    },
  };
  const checkLocation = (options) => {
    assert.deepEqual(options.location, { directory });
    assert.ok(options.signal instanceof AbortSignal);
  };
  const client = {
    plugin: {
      list: async (input, options) => {
        assert.deepEqual(input, { location: { directory } });
        assert.ok(options.signal instanceof AbortSignal);
        calls.push({ method: "plugin.list" });
        return {
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
        hello: async (input, options) => {
          assert.equal(input, undefined);
          checkLocation(options);
          calls.push({ method: "hello" });
          return { protocol: 4 };
        },
        start: async (input, options) => {
          checkLocation(options);
          assert.equal(input, undefined);
          calls.push({ method: "start" });
          return { sessionID };
        },
        stop: async (input, options) => {
          checkLocation(options);
          assert.deepEqual(input, { sessionID });
          calls.push({ method: "stop" });
          changed.emit("change");
        },
        turn: async () => {},
        idle: async (input, options) => {
          checkLocation(options);
          assert.equal(typeof input.idle, "boolean");
        },
      };
    },
    event: {
      subscribe(options) {
        signal = options.signal;
        subscribedTypes = new Set(options.types);
        signal.addEventListener("abort", close, { once: true });
        if (signal.aborted) close();
        // The SDK publishes this handshake even with a filtered subscription.
        // Delay it so starting Rust without awaiting the barrier fails the test.
        setImmediate(() => {
          if (closed) return;
          connected = true;
          publish({
            id: "evt_fixture_connected",
            created: Date.now(),
            type: "server.connected",
            data: {},
          });
        });
        return stream;
      },
    },
    session: {
      prompt: async (input, options) => {
        assert.deepEqual(input, { sessionID, text: "Fixture prompt." });
        assert.ok(options.signal instanceof AbortSignal);
        calls.push({ method: "prompt", input });
        changed.emit("change");
        return {
          id: "msg_fixture_prompt",
          sessionID,
          type: "user",
          time: { created: Date.now() },
          payload: { text: input.text },
          delivery: "steer",
        };
      },
      interrupt: async (input) => {
        assert.deepEqual(input, { sessionID });
        calls.push({ method: "interrupt" });
        return { interrupted: true };
      },
    },
  };
  return {
    client,
    calls,
    close,
    get aborted() {
      return signal?.aborted === true;
    },
    get connected() {
      return connected;
    },
    emit(type, data) {
      assert.ok(!closed, "Cannot publish to a closed fake server stream");
      assert.ok(subscribedTypes.has(type), `Bridge must subscribe to ${type}`);
      const seq = ++sequence;
      const event = {
        id: `evt_fixture_${seq}`,
        created: Date.now(),
        type,
        location: { directory },
        ...(type === "session.text.delta"
          ? {}
          : { durable: { aggregateID: sessionID, seq, version: 1 } }),
        data: { sessionID, ...data },
      };
      publish(event);
    },
    flush() {
      return closed || (waiting && !pending.length)
        ? Promise.resolve()
        : new Promise((resolve) => drainWaiters.push(resolve));
    },
  };
}

async function fixture(t) {
  try {
    await access(binary, constants.X_OK);
  } catch (cause) {
    throw new Error(
      `Missing executable ${binary}. Build the debug fixture with cargo build --manifest-path native/Cargo.toml -p opencode-converse before running this integration test.`,
      { cause },
    );
  }
  const temporary = await mkdtemp(join(resolve(tmpdir()), "converse-bridge-integration-"));
  const dataDirectory = join(temporary, "runtime-data");
  const directory = temporary;
  const changed = new EventEmitter();
  const sdk = server(directory, changed);
  const errors = [];
  const engine = [];
  const snapshots = [];
  let played = [];
  let trace;
  const host = [];
  let transport;
  let child;
  let exit;
  let exited;
  let ready = false;
  let bridge;
  t.after(async () => {
    try {
      if (bridge) await deadline(bridge.stop(), "bridge teardown", 3500);
    } finally {
      sdk.close();
      try {
        if (child && !exit) {
          child.kill("SIGKILL");
          await deadline(exited, "forced Rust subprocess teardown", 1500);
        }
      } finally {
        await removeAfterClose(temporary, { child, closed: exited });
      }
    }
  });
  bridge = createBridge({
    client: sdk.client,
    target: { serverID: "fixture-server", directory },
    settings: {},
    onChange(snapshot) {
      snapshots.push(snapshot);
      changed.emit("change");
    },
    onError(error) {
      errors.push(error);
      changed.emit("change");
    },
    runtime(options) {
      assert.equal(sdk.connected, true, "Rust must not start before server.connected");
      // Observe real Rust frames, but forward every frame to the real bridge.
      // No fake ChildProcess, stdout, stdin, Runtime, or protocol parser is used.
      const runtime = startRuntime({
        ...options,
        helloTimeoutMs: 3000,
        shutdownTimeoutMs: 1500,
        onMessage(message) {
          engine.push(message);
          options.onMessage(message);
          changed.emit("change");
        },
        spawn() {
          child = spawn(binary, ["run", "--test-fixture", "--test-state-dir", dataDirectory], {
            stdio: "pipe",
            windowsHide: true,
            env: fixtureEnvironment(dataDirectory),
          });
          trace = fixtureTrace(child, errors);
          played = trace.played;
          child.stderr.on("data", () => changed.emit("change"));
          child.stderr.on("end", () => changed.emit("change"));
          exited = new Promise((resolve) => {
            child.once("close", (code, signal) => {
              exit = { code, signal };
              changed.emit("change");
              resolve(exit);
            });
          });
          return child;
        },
      });
      void runtime.ready.then(
        () => {
          ready = true;
          changed.emit("change");
        },
        () => {},
      );
      transport = {
        ...runtime,
        send(message) {
          host.push(message);
          runtime.send(message);
        },
      };
      return transport;
    },
  });

  async function waitFor(description, predicate) {
    let check;
    const observed = new Promise((resolve, reject) => {
      check = () => {
        if (errors.length) reject(errors[0]);
        else if (predicate()) resolve();
        else if (exit)
          reject(new Error(`Rust exited before ${description}: ${JSON.stringify(exit)}`));
      };
      changed.on("change", check);
      check();
    });
    try {
      await deadline(observed, description);
    } finally {
      changed.off("change", check);
    }
  }

  async function reply({ finalOnly = false } = {}) {
    const baseline = engine.length;
    const assistantMessageID = "msg_fixture_assistant";
    sdk.emit("session.execution.started", {});
    sdk.emit("session.inbox.delivered", { inboxID: "msg_fixture_prompt" });
    sdk.emit("session.step.started", {
      assistantMessageID,
      agent: "converse",
      model,
      started: Date.now(),
    });
    if (!finalOnly) {
      sdk.emit("session.text.delta", {
        assistantMessageID,
        ordinal: 0,
        delta: "The fixture reply crossed the language boundary. ",
      });
      // Withhold authoritative completion until incremental text has actually
      // crossed the subprocess boundary and started real Rust playback.
      await waitFor("Rust streaming playback before complete text exists", () =>
        engine
          .slice(baseline)
          .some((message) => message.type === "status" && message.state === "speaking"),
      );
    }
    sdk.emit("session.text.ended", {
      assistantMessageID,
      ordinal: 0,
      text: "The fixture reply crossed the language boundary. ",
    });
    sdk.emit("session.execution.succeeded", {});
  }

  async function stop() {
    await deadline(bridge.stop(), "graceful bridge stop");
    assert.deepEqual(await deadline(exited, "actual Rust process exit"), { code: 0, signal: null });
    await deadline(trace.closed, "fixture stderr completion");
    assert.equal(child.killed, false, "Normal shutdown must not need a signal");
    assert.equal(sdk.aborted, true, "Bridge stop must abort the event subscription");
    assert.equal(sdk.calls.filter((call) => call.method === "stop").length, 1);
    assert.equal(bridge.snapshot().status, "off");
    assert.deepEqual(errors, []);
  }

  async function drain() {
    const baseline = engine.length;
    const text = "Fixture playback drain barrier.";
    transport.send({ type: "announcement", speaker: "a1", reply: "fixture-drain:0", text });
    // Unlike mute/shutdown, this FIFO sentinel consumes all preceding jobs
    // without cancelling them. Listening is the real controller drain ack.
    await waitFor("fixture drain sentinel consumed and acknowledged listening", () => {
      const states = engine.slice(baseline).filter((message) => message.type === "status");
      const speaking = states.findIndex((message) => message.state === "speaking");
      return (
        played.includes(text) &&
        speaking >= 0 &&
        states.slice(speaking + 1).some((message) => message.state === "listening") &&
        bridge.snapshot().status === "listening"
      );
    });
    return text;
  }

  await deadline(bridge.start(), "RPC start and real Rust handshake");
  await waitFor("Rust fixture transcript reaching the SDK", () =>
    sdk.calls.some((call) => call.method === "prompt"),
  );
  assert.equal(ready, true, "startRuntime.ready requires Rust hello and TS init");
  assert.deepEqual(
    engine.find((message) => message.type === "prompt"),
    {
      type: "prompt",
      turn: 1,
      speaker: "a1",
      addressed: false,
      text: "Fixture prompt.",
    },
  );
  assert.deepEqual(
    sdk.calls.slice(0, 4).map((call) => call.method),
    ["plugin.list", "hello", "start", "prompt"],
  );
  await access(join(dataDirectory, "engine.lock"));
  return { bridge, sdk, engine, snapshots, played, host, waitFor, reply, drain, stop };
}

for (const finalOnly of [false, true]) {
  test(
    `real Rust and TS bridge round trip (${finalOnly ? "complete text only" : "streaming plus complete text"})`,
    { timeout: 20_000 },
    async (t) => {
      const f = await fixture(t);
      const baseline = f.engine.length;
      await f.reply({ finalOnly });
      await f.waitFor("Rust speaking then listening after the SDK reply", () => {
        const states = f.engine.slice(baseline).filter((message) => message.type === "status");
        const speaking = states.findIndex((message) => message.state === "speaking");
        return (
          speaking >= 0 &&
          states.slice(speaking + 1).some((message) => message.state === "listening")
        );
      });
      assert.equal(f.bridge.snapshot().status, "listening");
      const drainText = await f.drain();
      assert.deepEqual(
        f.played,
        ["The fixture reply crossed the language boundary.", drainText],
        "stream reconciliation and final-only delivery must consume the exact reply once",
      );
      assert.equal(f.sdk.calls.filter((call) => call.method === "prompt").length, 1);
      await f.stop();
    },
  );
}

test(
  "idle synthetic worker announcement is consumed by the Rust fixture exactly once",
  { timeout: 20_000 },
  async (t) => {
    const f = await fixture(t);
    await f.reply({ finalOnly: true });
    await f.waitFor("initial reply playback", () => {
      const states = f.engine.filter((message) => message.type === "status");
      const speaking = states.findIndex((message) => message.state === "speaking");
      return (
        f.played.includes("The fixture reply crossed the language boundary.") &&
        speaking >= 0 &&
        states.slice(speaking + 1).some((message) => message.state === "listening")
      );
    });
    const baseline = f.engine.length;
    const playedBaseline = f.played.length;
    const inboxID = "msg_fixture_announcement";
    const assistantMessageID = "msg_fixture_summary";
    const enqueue = () =>
      f.sdk.emit("session.inbox.enqueued", {
        inboxID,
        item: {
          type: "synthetic",
          delivery: "queue",
          payload: {
            text: "Untrusted worker result",
            description: "Converse worker update",
            metadata: { [WORKER_UPDATE_MARKER]: true },
          },
        },
      });
    enqueue();
    f.sdk.emit("session.execution.started", {});
    f.sdk.emit("session.inbox.delivered", { inboxID });
    f.sdk.emit("session.step.started", {
      assistantMessageID,
      agent: "converse",
      model,
      started: Date.now(),
    });
    const summary = {
      assistantMessageID,
      ordinal: 0,
      text: "The background worker finished its task.",
    };
    f.sdk.emit("session.text.ended", summary);
    f.sdk.emit("session.text.ended", summary);
    f.sdk.emit("session.execution.succeeded", {});
    await f.waitFor("completed summary consumption and Rust drain acknowledgement", () => {
      const states = f.engine.slice(baseline).filter((message) => message.type === "status");
      return (
        f.played.slice(playedBaseline).includes(summary.text) &&
        states.some((message) => message.state === "speaking") &&
        states.at(-1)?.state === "listening"
      );
    });
    assert.deepEqual(f.played.slice(playedBaseline), [summary.text]);
    enqueue();
    f.sdk.emit("session.execution.started", {});
    f.sdk.emit("session.inbox.delivered", { inboxID });
    const replayedAssistantMessageID = "msg_fixture_replayed_summary";
    f.sdk.emit("session.step.started", {
      assistantMessageID: replayedAssistantMessageID,
      agent: "converse",
      model,
      started: Date.now(),
    });
    f.sdk.emit("session.text.ended", {
      ...summary,
      assistantMessageID: replayedAssistantMessageID,
    });
    f.sdk.emit("session.execution.succeeded", {});
    await deadline(f.sdk.flush(), "all duplicate SDK events consumed");
    const drainText = await f.drain();
    assert.deepEqual(f.played.slice(playedBaseline), [summary.text, drainText]);
    assert.deepEqual(
      f.host.filter(
        (message) => message.type === "announcement" && message.reply !== "fixture-drain:0",
      ),
      [
        {
          type: "announcement",
          speaker: "a1",
          reply: `${assistantMessageID}:0`,
          text: summary.text,
        },
      ],
      "Rust dedupe must not mask duplicate announcements sent by the bridge",
    );
    assert.equal(
      f.engine
        .slice(baseline)
        .filter((message) => message.type === "status" && message.state === "speaking").length,
      2, // Exactly one summary playback plus the explicit non-cancelling sentinel.
    );
    assert.equal(f.sdk.calls.filter((call) => call.method === "prompt").length, 1);
    await f.stop();
  },
);
