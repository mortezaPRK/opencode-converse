import assert from "node:assert/strict";
import test from "node:test";
import { createMain } from "../dist/gui/main.js";
import { createBridge } from "../dist/client/bridge.js";
import { cleanupFixture } from "./client-cleanup-fixture.mjs";

function setup({ stop, start, fixture } = {}) {
  let implementation;
  let closeWindow;
  let changes = 0;
  let providerDisposed = 0;
  const credentials = { authorization: "Bearer main-only" };
  const endpoints = new Map([
    [
      "remote",
      { id: "remote", url: "https://chosen.example", headers: credentials, sameMachine: false },
    ],
    [
      "sidecar",
      {
        id: "sidecar",
        url: "http://127.0.0.1:4096",
        headers: { authorization: "Bearer sidecar-only" },
        sameMachine: true,
      },
    ],
  ]);
  const clients = [];
  const bridges = [];
  const errors = [];
  const ctx = {
    signal: new AbortController().signal,
    storage: {
      store: (name) => {
        assert.equal(name, "devices");
        return {
          value: { inputDevice: "host-mic", outputDevice: "host-speaker", executable: "/evil" },
        };
      },
    },
    serverEndpoints: { get: (id) => endpoints.get(id) },
    windows: {
      on(event, handler) {
        assert.equal(event, "close");
        closeWindow = handler;
        return () => {};
      },
    },
    provide(token, impl) {
      assert.equal(token.id, "converse.audio");
      implementation = impl;
      return {
        changed: () => {
          changes++;
        },
        emit: (...args) => errors.push(args),
        dispose: () => {
          providerDisposed++;
        },
      };
    },
  };
  const cleanup = createMain({
    makeClient(options) {
      const client = fixture?.client ?? { authenticatedEndpoint: options };
      clients.push(client);
      return client;
    },
    bridge(options) {
      if (fixture) {
        const current = createBridge({ ...options, runtime: fixture.runtime });
        bridges.push(current);
        return current;
      }
      const state = { status: "off", serverID: options.target.serverID, sessionID: "supervisor" };
      const bridge = {
        options,
        stopped: 0,
        muted: 0,
        snapshot: () => ({ ...state }),
        async start() {
          await start?.(bridge);
          state.status = "listening";
          options.onChange({ ...state });
        },
        async stop() {
          bridge.stopped++;
          state.status = "off";
          options.onChange({ ...state });
          await stop?.(bridge);
        },
        mute() {
          bridge.muted++;
        },
      };
      bridges.push(bridge);
      return bridge;
    },
  })(ctx);
  const caller = (window) => ({ window, signal: new AbortController().signal });
  return {
    api: implementation,
    caller,
    cleanup,
    clients,
    bridges,
    endpoints,
    credentials,
    errors,
    close: (id) => closeWindow({ id }),
    changes: () => changes,
    providerDisposed: () => providerDisposed,
  };
}

test("GUI resolves server ID to main-owned credentials for both remote and sidecar", async () => {
  const s = setup();
  const target = {
    serverID: "remote",
    directory: "/project",
    model: { providerID: "p", id: "m", variant: "v" },
    url: "https://attacker.example",
    executable: "/attacker",
  };
  await s.api.start(target, s.caller(1));
  target.directory = "/changed";
  target.model.id = "changed";
  s.credentials.authorization = "changed";
  assert.deepEqual(s.clients[0].authenticatedEndpoint, {
    baseUrl: "https://chosen.example",
    headers: { authorization: "Bearer main-only" },
  });
  assert.deepEqual(s.bridges[0].options.target, {
    serverID: "remote",
    directory: "/project",
    model: { providerID: "p", id: "m", variant: "v" },
  });
  assert.deepEqual(s.bridges[0].options.settings, {
    inputDevice: "host-mic",
    outputDevice: "host-speaker",
  });
  assert.deepEqual(s.api.state(2), {
    status: "listening",
    serverID: "remote",
    sessionID: "supervisor",
  });
  await s.api.stop(undefined, s.caller(1));
  await s.api.start({ serverID: "sidecar", directory: "/local" }, s.caller(2));
  assert.equal(s.clients[1].authenticatedEndpoint.baseUrl, "http://127.0.0.1:4096");
  assert.equal(s.bridges[1].options.target.serverID, "sidecar");
  await s.cleanup();
});

test("owner close cannot stop a replacement already queued by another window", async () => {
  const s = setup();
  await s.api.start({ serverID: "remote", directory: "/chosen" }, s.caller(1));
  const starting = s.api.start({ serverID: "sidecar", directory: "/other" }, s.caller(2));
  s.close(1);
  await starting;
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(s.bridges.length, 2);
  assert.equal(s.bridges[1].stopped, 0);
  assert.equal(s.api.state().status, "listening");
  await s.cleanup();
});

test("GUI Stop immediately cancels owned startup but a foreign window cannot", async () => {
  let release;
  const pending = new Promise((resolve) => {
    release = resolve;
  });
  const s = setup({ start: () => pending });
  const starting = s.api.start({ serverID: "remote", directory: "/chosen" }, s.caller(1));
  await new Promise((resolve) => setImmediate(resolve));
  const foreign = s.api.stop(undefined, s.caller(2));
  void foreign.catch(() => {});
  assert.equal(s.bridges[0].stopped, 0);
  const stopping = s.api.stop(undefined, s.caller(1));
  assert.equal(s.bridges[0].stopped, 1, "Stop must cancel before queued startup settles");
  release();
  await assert.rejects(foreign, /another window/);
  await Promise.all([starting, stopping]);
  await s.cleanup();
});

test("GUI does not discover or fall back from an unavailable selected endpoint", async () => {
  const s = setup();
  await assert.rejects(
    s.api.start({ serverID: "missing", directory: "/project" }, s.caller(1)),
    /Selected server is unavailable/,
  );
  assert.equal(s.clients.length, 0);
  assert.equal(s.bridges.length, 0);
  s.endpoints.set("mismatch", { ...s.endpoints.get("sidecar"), id: "sidecar" });
  await assert.rejects(
    s.api.start({ serverID: "mismatch", directory: "/project" }, s.caller(1)),
    /Selected server is unavailable/,
  );
  await s.cleanup();
});

test("GUI releases a failed internal run so another window can restart", async () => {
  const s = setup();
  await s.api.start({ serverID: "remote", directory: "/project" }, s.caller(1));
  s.bridges[0].options.onChange({ status: "off", serverID: "remote", sessionID: "supervisor" });
  await s.api.start({ serverID: "sidecar", directory: "/other" }, s.caller(2));
  assert.equal(s.bridges.length, 2);
  assert.ok(s.bridges[0].stopped > 0);
  assert.equal(s.api.state(2).serverID, "sidecar");
  await s.cleanup();
});

test("GUI queued restart awaits retiring teardown and rechecks caller cancellation", async () => {
  for (const cancelled of [false, true]) {
    let release;
    const stopping = new Promise((resolve) => {
      release = resolve;
    });
    const s = setup({ stop: (bridge) => (bridge === s.bridges[0] ? stopping : undefined) });
    await s.api.start({ serverID: "remote", directory: "/project" }, s.caller(1));
    const controller = new AbortController();
    const restarting = s.api.start(
      { serverID: "sidecar", directory: "/other" },
      { window: 2, signal: controller.signal },
    );
    // The start is already queued when the old bridge reports off.
    s.bridges[0].options.onChange({ status: "off", serverID: "remote", sessionID: "supervisor" });
    await new Promise((resolve) => setImmediate(resolve));
    assert.equal(s.bridges.length, 1, "replacement cannot acquire devices before teardown");
    if (cancelled) controller.abort();
    release();
    await restarting;
    assert.equal(s.bridges.length, cancelled ? 1 : 2);
    await s.cleanup();
  }
});

test("GUI Stop fences starts requested before it across windows and during retirement", async () => {
  for (const queued of [false, true])
    for (const stopWindow of [1, 2]) {
      let release;
      const retiring = new Promise((resolve) => {
        release = resolve;
      });
      const s = setup({ stop: (bridge) => (bridge === s.bridges[0] ? retiring : undefined) });
      const input = { serverID: "remote", directory: "/project" };
      await s.api.start(input, s.caller(1));
      const beforeStop = s.api.start(input, s.caller(2));
      s.bridges[0].options.onChange({ status: "off", serverID: "remote", sessionID: "supervisor" });
      // Either still queued, or already waiting inside the retiring bridge's stop.
      if (!queued) await new Promise((resolve) => setImmediate(resolve));
      const stopping = s.api.stop(undefined, s.caller(stopWindow));
      release();
      await Promise.all([beforeStop, stopping]);
      assert.equal(
        s.bridges.length,
        1,
        "Stop must not allow an older Start to allocate a replacement",
      );
      assert.equal(s.api.state().status, "off");
      await s.api.start(input, s.caller(2));
      assert.equal(s.bridges.length, 2, "Start requested after Stop is allowed");
      await assert.rejects(s.api.stop(undefined, s.caller(1)), /another window/);
      assert.equal(
        s.bridges[1].stopped,
        0,
        "foreign Stop must neither change ownership nor stop the replacement",
      );
      await s.cleanup();
    }
});

test("GUI foreign Stop does not cancel an owner's already queued Start", async () => {
  const s = setup();
  const input = { serverID: "remote", directory: "/project" };
  await s.api.start(input, s.caller(1));
  const starting = s.api.start(input, s.caller(1));
  await assert.rejects(s.api.stop(undefined, s.caller(2)), /another window/);
  await starting;
  assert.equal(s.bridges.length, 1);
  assert.equal(s.bridges[0].stopped, 0);
  assert.equal(s.api.state().status, "listening");
  await s.cleanup();
});

test("GUI owns one run across windows, owner closes cleanup, stale callbacks ignored", async () => {
  const s = setup();
  await s.api.start({ serverID: "remote", directory: "/project" }, s.caller(1));
  await assert.rejects(
    s.api.start({ serverID: "sidecar", directory: "/other" }, s.caller(2)),
    /another window/,
  );
  await assert.rejects(s.api.stop(undefined, s.caller(2)), /another window/);
  await assert.rejects(s.api.mute(undefined, s.caller(2)), /another window/);
  assert.equal(s.bridges.length, 1);
  await s.api.mute(undefined, s.caller(1));
  assert.equal(s.bridges[0].muted, 1);
  s.close(2);
  assert.equal(s.bridges[0].stopped, 0);
  s.close(1);
  await new Promise((resolve) => setImmediate(resolve));
  assert.ok(s.bridges[0].stopped > 0);
  await s.api.start({ serverID: "sidecar", directory: "/new" }, s.caller(2));
  const before = s.changes();
  s.bridges[0].options.onChange({ status: "speaking", serverID: "remote", sessionID: "stale" });
  s.bridges[0].options.onError(new Error("stale"));
  assert.equal(s.changes(), before);
  assert.equal(s.errors.length, 0);
  assert.equal(s.api.state(1).serverID, "sidecar");
  await s.cleanup();
});

test("GUI restarts after server cleanup refusal or session movement, but waits for late native close", async () => {
  for (const mode of ["stop", "moved", "late-close"]) {
    const fixture = cleanupFixture();
    const s = setup({ fixture });
    const input = { serverID: "remote", directory: "/chosen" };
    await s.api.start(input, s.caller(1));
    if (mode === "moved") {
      fixture.move();
      await new Promise((resolve) => setImmediate(resolve));
    } else if (mode === "late-close") {
      fixture.processes[0].exitOnShutdown = false;
      await assert.rejects(s.api.stop(undefined, s.caller(1)), /termination is unconfirmed/);
      await assert.rejects(s.api.start(input, s.caller(2)), /termination is unconfirmed/);
      assert.equal(s.bridges.length, 1);
      const changes = s.changes();
      fixture.processes[0].emit("close", null, "SIGKILL");
      await s.api.start(input, s.caller(2));
      assert.equal(s.changes() - changes, 3, "retry must not emit another off notification");
    } else {
      await s.api.stop(undefined, s.caller(1));
    }
    if (mode !== "late-close") await s.api.start(input, s.caller(2));
    assert.equal(s.bridges.length, 2);
    assert.ok(s.errors.some(([, error]) => /server cleanup refused/.test(error.message)));
    assert.ok(fixture.calls.every((call) => call.location.directory === "/chosen"));
    await assert.rejects(
      s.cleanup(),
      (error) =>
        error instanceof AggregateError &&
        error.errors.some((failure) => /server cleanup refused/.test(failure.message)),
    );
  }
});

test("GUI disposal rejects unknown native termination and retains cleanup until a confirmed retry", async () => {
  let confirmed = false;
  const failure = new Error("native termination unconfirmed");
  const s = setup({
    stop: async () => {
      if (!confirmed) throw failure;
    },
  });
  await s.api.start({ serverID: "remote", directory: "/chosen" }, s.caller(1));
  await assert.rejects(s.cleanup(), (error) => error === failure);
  assert.equal(
    s.providerDisposed(),
    0,
    "do not discard the cleanup interface while native exit is unknown",
  );
  confirmed = true;
  await s.cleanup();
  assert.equal(s.providerDisposed(), 1);
});

test("GUI disposal reports a late allocated session whose original-location cleanup fails", async () => {
  const fixture = cleanupFixture();
  const rpc = fixture.client.rpc();
  let accept;
  const stops = [];
  fixture.client.rpc = () => ({
    ...rpc,
    start: () =>
      new Promise((resolve) => {
        accept = resolve;
      }),
    stop: async (input, options) => {
      stops.push({ input, options });
      throw new Error("late cleanup refused");
    },
  });
  const s = setup({ fixture });
  const starting = s.api.start({ serverID: "remote", directory: "/chosen" }, s.caller(1));
  const startupFailed = assert.rejects(starting, /late.*session.*cleanup.*failed/i);
  await new Promise((resolve) => setImmediate(resolve));
  let settled = false;
  const disposing = s.cleanup();
  void disposing.then(
    () => {
      settled = true;
    },
    () => {
      settled = true;
    },
  );
  const disposalFailed = assert.rejects(
    disposing,
    (error) =>
      error instanceof AggregateError &&
      error.errors.some((failure) => /late.*session.*cleanup.*failed/i.test(failure.message)),
  );
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(settled, false);
  accept({ sessionID: "late-session" });
  await Promise.all([startupFailed, disposalFailed]);
  assert.equal(fixture.processes.length, 0);
  assert.deepEqual(stops[0].options.location, { directory: "/chosen" });
  assert.equal(
    s.providerDisposed(),
    1,
    "confirmed native cleanup releases the provider while reporting the late server failure",
  );
});

test("GUI disposal ignores intentional startup cancellation and queued pre-start validation errors", async () => {
  for (const backoff of [false, true]) {
    const fixture = cleanupFixture();
    fixture.client.plugin.list = () =>
      backoff ? Promise.resolve({ data: [] }) : new Promise(() => {});
    const s = setup({ fixture });
    const starting = s.api.start({ serverID: "remote", directory: "/chosen" }, s.caller(1));
    const cancelled = assert.rejects(starting, /stopped|abort/i);
    await new Promise((resolve) => setImmediate(resolve));
    await s.cleanup();
    await cancelled;
    assert.equal(fixture.processes.length, 0);
    assert.equal(s.providerDisposed(), 1);
  }
  const invalid = setup();
  await assert.rejects(
    invalid.api.start({ serverID: "missing", directory: "/chosen" }, invalid.caller(1)),
    /unavailable/,
  );
  await invalid.cleanup();
  assert.equal(invalid.providerDisposed(), 1);
});
