import assert from "node:assert/strict";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { createTui } from "../dist/tui.js";
import { createBridge, isStoppedError } from "../dist/client/bridge.js";
import { cleanupFixture } from "./client-cleanup-fixture.mjs";

const tick = () => new Promise((resolve) => setImmediate(resolve));

function setup({ fixture, options } = {}) {
  const bridges = [];
  const errors = [];
  let commands;
  let release;
  const teardown = new Promise((resolve) => {
    release = resolve;
  });
  const ctx = {
    client: fixture?.client ?? {},
    location: { directory: "/chosen" },
    storage: { store: () => [{}] },
    options: options ?? {},
    theme: { text: { base: "white" } },
    keymap: {
      layer: (get) => {
        commands = get().commands;
      },
    },
    ui: {
      model: { current: () => undefined },
      toast: { show: (value) => errors.push(value) },
      slot: (slot) => {
        if (slot.append === "app") slot.render();
        return () => {};
      },
    },
  };
  const cleanup = createTui({
    bridge: (options) => {
      if (fixture) {
        const current = createBridge({ ...options, runtime: fixture.runtime });
        bridges.push(current);
        return current;
      }
      const bridge = {
        options,
        stops: 0,
        start: async () =>
          options.onChange({ status: "listening", serverID: "connected", sessionID: "s" }),
        stop: async () => {
          bridge.stops++;
          options.onChange({ status: "off", serverID: "connected", sessionID: "s" });
          if (bridge === bridges[0]) await teardown;
        },
        mute() {},
      };
      bridges.push(bridge);
      return bridge;
    },
  }).setup(ctx);
  return {
    bridges,
    errors,
    cleanup,
    release,
    run: (id) => commands.find((c) => c.id === `converse.${id}`).run(),
  };
}

test("TUI concurrent starts after teardown install only one replacement", async () => {
  const s = setup();
  await s.run("start");
  s.bridges[0].options.onChange({ status: "off", serverID: "connected", sessionID: "s" });
  const starts = [s.run("start"), s.run("start")];
  await tick();
  assert.equal(s.bridges.length, 1);
  s.release();
  await Promise.all(starts);
  assert.equal(s.bridges.length, 2);
  await s.cleanup();
  assert.ok(s.bridges[1].stops > 0);
});

test("TUI restarts after server cleanup refusal or movement, but not before late native close", async () => {
  for (const mode of ["stop", "moved", "late-close"]) {
    const fixture = cleanupFixture();
    const s = setup({ fixture });
    await s.run("start");
    if (mode === "moved") {
      fixture.move();
      await tick();
    } else {
      if (mode === "late-close") fixture.processes[0].exitOnShutdown = false;
      await s.run("stop");
    }
    if (mode === "late-close") {
      await s.run("start");
      assert.equal(s.bridges.length, 1);
      fixture.processes[0].emit("close", null, "SIGKILL");
    }
    await s.run("start");
    assert.equal(s.bridges.length, 2);
    assert.ok(s.errors.some((error) => /server cleanup refused/.test(error.message)));
    assert.ok(fixture.calls.every((call) => call.location.directory === "/chosen"));
    await assert.rejects(
      s.cleanup(),
      (error) =>
        error instanceof AggregateError &&
        error.errors.some((failure) => /server cleanup refused/.test(failure.message)),
    );
  }
});

test("TUI stop or disposal during retiring teardown prevents queued replacement", async () => {
  for (const dispose of [false, true]) {
    const s = setup();
    await s.run("start");
    s.bridges[0].options.onChange({ status: "off", serverID: "connected", sessionID: "s" });
    const starting = s.run("start");
    await tick();
    const stopping = dispose ? s.cleanup() : s.run("stop");
    s.release();
    await Promise.all([starting, stopping]);
    assert.equal(s.bridges.length, 1);
    if (!dispose) await s.cleanup();
  }
});

test("TUI disposal waits for every late allocation and cleanup, including a previously stopped startup", async () => {
  const fixture = cleanupFixture();
  const accepted = [];
  const cleanup = [];
  const rpc = fixture.client.rpc();
  fixture.client.rpc = () => ({
    ...rpc,
    start: () => new Promise((resolve) => accepted.push(resolve)),
    stop: (input, options) => new Promise((resolve) => cleanup.push({ input, options, resolve })),
  });
  const s = setup({ fixture });
  const first = s.run("start");
  await tick();
  await s.run("stop");
  const second = s.run("start");
  await tick();
  assert.equal(accepted.length, 2);
  let disposed = false;
  const disposing = s.cleanup().then(() => {
    disposed = true;
  });
  await tick();
  assert.equal(disposed, false, "disposal must retain the stopped startup's pending allocation");
  accepted[0]({ sessionID: "late-first" });
  accepted[1]({ sessionID: "late-second" });
  await tick();
  assert.equal(cleanup.length, 2);
  assert.equal(disposed, false, "late server cleanup must finish before disposal resolves");
  cleanup[0].resolve();
  await tick();
  assert.equal(disposed, false);
  cleanup[1].resolve();
  await Promise.all([first, second, disposing]);
  assert.equal(fixture.processes.length, 0);
  assert.ok(cleanup.every((call) => call.options.location.directory === "/chosen"));
});

test("TUI disposal reports cleanup refusal for a session allocated after local stop", async () => {
  const fixture = cleanupFixture();
  const rpc = fixture.client.rpc();
  let accept;
  const calls = [];
  fixture.client.rpc = () => ({
    ...rpc,
    start: () =>
      new Promise((resolve) => {
        accept = resolve;
      }),
    stop: async (input, options) => {
      calls.push({ input, options });
      throw new Error("late server cleanup refused");
    },
  });
  const s = setup({ fixture });
  const starting = s.run("start");
  await tick();
  const disposing = s.cleanup();
  const failed = assert.rejects(
    disposing,
    (error) =>
      error instanceof AggregateError &&
      error.errors.some((failure) => /late.*session.*cleanup.*failed/i.test(failure.message)),
  );
  accept({ sessionID: "late-session" });
  await Promise.all([starting, failed]);
  assert.equal(fixture.processes.length, 0);
  assert.equal(calls.length, 1);
  assert.deepEqual(calls[0].options.location, { directory: "/chosen" });
});

test("TUI disposal treats readiness cancellation as normal rather than a cleanup failure", async () => {
  const fixture = cleanupFixture();
  fixture.client.plugin.list = () => new Promise(() => {});
  const s = setup({ fixture });
  const starting = s.run("start");
  await tick();
  await s.cleanup();
  await starting;
  assert.equal(fixture.processes.length, 0);
  assert.deepEqual(s.errors, []);
});

test("TUI disposal during readiness backoff accepts the promise AbortError with its stopped cause", async () => {
  assert.equal(isStoppedError(new DOMException("unrelated abort", "AbortError")), false);
  assert.equal(
    isStoppedError(
      Object.assign(new Error("unrelated abort"), {
        name: "AbortError",
        cause: new Error("other cause"),
      }),
    ),
    false,
  );
  const fixture = cleanupFixture();
  let lists = 0;
  let allocated = 0;
  fixture.client.plugin.list = async () => {
    lists++;
    return { data: [] };
  };
  const rpc = fixture.client.rpc();
  fixture.client.rpc = () => ({
    ...rpc,
    start: async (...args) => {
      allocated++;
      return rpc.start(...args);
    },
  });
  const s = setup({ fixture });
  const starting = s.run("start");
  await tick();
  assert.equal(lists, 1, "readiness must have entered its real promise backoff");
  await s.cleanup();
  await starting;
  assert.equal(allocated, 0);
  assert.equal(fixture.processes.length, 0);
  assert.deepEqual(s.errors, []);
});

test("TUI disposal keeps unknown native termination as a reported cleanup failure", async () => {
  const fixture = cleanupFixture();
  const s = setup({ fixture });
  await s.run("start");
  fixture.processes[0].exitOnShutdown = false;
  await assert.rejects(
    s.cleanup(),
    (error) =>
      error instanceof AggregateError &&
      error.errors.some((failure) => /termination is unconfirmed/.test(failure.message)),
  );
  fixture.processes[0].emit("close", null, "SIGKILL");
});

test("TUI hands configured inference tuning to the bridge", async () => {
  const s = setup({ options: { asrThreads: 2, useGpu: false } });
  await s.run("start");
  assert.deepEqual(s.bridges[0].options.runtimeConfig, { asrThreads: 2, useGpu: false });
  assert.equal(
    `${s.bridges[0].options.packageRoot}/`,
    fileURLToPath(new URL("../", import.meta.url)),
  );
  s.release();
  await s.cleanup();
});

test("TUI refuses invalid inference tuning before starting", async () => {
  const s = setup({ options: { asrThreads: 0 } });
  await s.run("start");
  assert.equal(s.bridges.length, 0);
  assert.match(s.errors.at(-1)?.message ?? "", /between 1 and 256/);
  await s.cleanup();
});
