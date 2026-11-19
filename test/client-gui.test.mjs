import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import test from "node:test";
import { Command, ComposerPill, Panel, TitlebarItem } from "@opencode/plugin/gui";
import { createBridge } from "../dist/client/bridge.js";
import { PRESENCE_ERROR_MS } from "../dist/presence.js";
import { cleanupFixture } from "./client-cleanup-fixture.mjs";

// TypeScript emits the standard Solid JSX runtime import for the Dialog view.
// Node resolves that subpath to Solid's server build, while the Electron host
// supplies the DOM runtime. Tests never render; this only lets them load it.
registerHooks({
  resolve(specifier, context, nextResolve) {
    return nextResolve(
      specifier === "solid-js/jsx-runtime"
        ? new URL("../dist/gui/jsx-runtime.js", import.meta.url).href
        : specifier,
      context,
    );
  },
});
const gui = (await import("../dist/gui.js")).default;
const { createMain } = await import("../dist/gui/main.js");

function setup({ stop, start, fixture, options } = {}) {
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
    options: options ?? {},
    client: {
      rpc: () => ({
        hello: async () => ({ protocol: 1, packageRoot: "/local/package" }),
      }),
    },
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
      const state = {
        status: "off",
        paused: false,
        serverID: options.target.serverID,
        sessionID: "supervisor",
        interrupted: null,
      };
      const bridge = {
        options,
        stopped: 0,
        muted: 0,
        paused: [],
        snapshot: () => ({ ...state }),
        async start() {
          await start?.(bridge);
          state.status = "listening";
          options.onChange({ ...state });
        },
        async stop() {
          bridge.stopped++;
          state.status = "off";
          state.paused = false;
          options.onChange({ ...state });
          await stop?.(bridge);
        },
        mute() {
          bridge.muted++;
        },
        pause(paused) {
          bridge.paused.push(paused);
          state.paused = paused;
          options.onChange({ ...state });
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
    url: "https://attacker.example",
    executable: "/attacker",
  };
  await s.api.start(target, s.caller(1));
  target.directory = "/changed";
  s.credentials.authorization = "changed";
  assert.deepEqual(s.clients[0].authenticatedEndpoint, {
    baseUrl: "https://chosen.example",
    headers: { authorization: "Bearer main-only" },
  });
  assert.deepEqual(s.bridges[0].options.target, {
    serverID: "remote",
    directory: "/project",
  });
  assert.deepEqual(s.bridges[0].options.settings, {
    inputDevice: "host-mic",
    outputDevice: "host-speaker",
  });
  assert.equal(s.bridges[0].options.packageRoot, "/local/package");
  assert.deepEqual(s.api.state(2), {
    status: "listening",
    paused: false,
    serverID: "remote",
    sessionID: "supervisor",
    interrupted: null,
  });
  await s.api.stop(undefined, s.caller(1));
  await s.api.start({ serverID: "sidecar", directory: "/local" }, s.caller(2));
  assert.equal(s.clients[1].authenticatedEndpoint.baseUrl, "http://127.0.0.1:4096");
  assert.equal(s.bridges[1].options.target.serverID, "sidecar");
  await s.cleanup();
});

test("GUI passes inference tuning and rejects invalid values", async () => {
  const s = setup({ options: { asrThreads: 2, useGpu: false } });
  await s.api.start({ serverID: "remote", directory: "/chosen" }, s.caller(1));
  assert.deepEqual(s.bridges[0].options.runtimeConfig, { asrThreads: 2, useGpu: false });
  await s.cleanup();

  const invalid = setup({ options: { asrThreads: 0 } });
  await assert.rejects(
    invalid.api.start({ serverID: "remote", directory: "/chosen" }, invalid.caller(1)),
    /between 1 and 256/,
  );
  await invalid.cleanup();
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
  await assert.rejects(s.api.pause({ paused: true }, s.caller(2)), /another window/);
  await s.api.pause({ paused: true }, s.caller(1));
  assert.deepEqual(s.bridges[0].paused, [true]);
  assert.equal(s.api.state(1).paused, true);
  await s.api.pause({ paused: false }, s.caller(1));
  assert.deepEqual(s.bridges[0].paused, [true, false]);
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

// The window setup only renders through the host, so the test records what it
// contributes and asks the same reactive thunks the host would read.
function statusSetup({ status = "off", sessionID = "supervisor" } = {}) {
  const state = { status, paused: false, sessionID };
  let titlebar;
  let pill;
  let listener;
  let panel;
  const pauses = [];
  const panels = [];
  const commands = new Map();
  const session = { id: "supervisor", server: { id: "remote" } };
  const ctx = {
    id: "converse",
    signal: new AbortController().signal,
    app: { platform: "desktop" },
    toast: { show: () => {} },
    sessions: { current: () => session, list: () => [session] },
    screen: { current: () => undefined },
    tabs: { open: () => true },
    layout: {
      open: (...args) => panels.push(args),
      close: () => {},
    },
    add(registry, value) {
      if (registry === TitlebarItem) titlebar = value;
      else if (registry === ComposerPill) pill = value;
      else if (registry === Panel) panel = value;
      else if (registry === Command) {
        const item = typeof value === "function" ? value() : value;
        commands.set(item.id, item);
      }
    },
    main: () => ({
      state: () => ({ ...state }),
      start: async () => {},
      stop: async () => {},
      mute: async () => {},
      pause: async (input) => {
        pauses.push(input.paused);
        state.paused = input.paused;
      },
      on: (name, handler) => {
        listener = handler;
        return () => {
          listener = undefined;
        };
      },
    }),
  };
  const cleanup = gui.setup(ctx);
  return {
    state,
    pauses,
    panels,
    panel,
    command: (id) => commands.get(id),
    item: () => (typeof titlebar === "function" ? titlebar() : titlebar),
    pill,
    shown: (id = "supervisor") => pill.show({ session: { id } }),
    fail: (message) => listener?.({ message }),
    cleanup,
  };
}

test("GUI status pill names the state and picks artwork for it", () => {
  const s = statusSetup();
  const idle = s.item();
  assert.equal(idle.label, "Converse: Idle");
  assert.equal(idle.icon, "waveform");
  assert.equal(idle.pressed, false);
  assert.equal(idle.busy, false);
  assert.match(idle.title, /Not running/);

  s.state.status = "listening";
  const listening = s.item();
  assert.equal(listening.label, "Converse: Listening");
  assert.equal(listening.icon, "outline-wave-audio");
  assert.equal(listening.pressed, true);
  assert.match(listening.title, /Mic open/);

  s.state.status = "hearing";
  assert.equal(s.item().icon, "outline-audio");
  s.state.status = "thinking";
  assert.equal(s.item().icon, "outline-brain");
  s.state.status = "speaking";
  assert.equal(s.item().icon, "speech-bubble");
  s.state.status = "muted";
  assert.equal(s.item().icon, "outline-eye-slash");

  s.state.status = "loading";
  assert.equal(s.item().busy, true, "the host spinner covers start-up");
  assert.equal(s.item().pressed, true);
  s.cleanup();
});

test("GUI composer chip stays on the supervisor session and opens a quick popover", () => {
  const s = statusSetup();
  assert.equal(s.shown(), undefined, "idle shows no chip");

  s.state.status = "listening";
  const view = s.shown();
  assert.equal(view.label, "Converse · Listening");
  assert.equal(view.icon, "outline-wave-audio");
  assert.equal(view.tone, "success");
  assert.match(view.title, /quick controls/);
  assert.equal(view.rows, undefined, "the pill never expands actions inline");
  assert.equal(view.run, undefined);
  assert.equal(typeof view.menu, "function", "the pill opens a compact popover");
  assert.equal(s.shown("worker"), undefined, "other sessions never show it");

  s.state.status = "muted";
  assert.equal(s.shown().label, "Converse · Muted");
  assert.equal(s.shown().tone, "warning");

  s.state.sessionID = null;
  assert.equal(s.shown(), undefined, "a stopped bridge has no chip");
  s.cleanup();
});

test("GUI active status opens details and pause/resume keeps the conversation alive", () => {
  const s = statusSetup({ status: "speaking" });
  assert.equal(s.panel.region, "side");
  assert.deepEqual(s.panel.list({ open: [] }), []);
  assert.deepEqual(s.panel.list({ open: ["details"] }), [{ id: "details", title: "Converse" }]);
  assert.equal(s.panel.mobile.kind, "drawer");
  s.item().run();
  assert.equal(s.panels.length, 1, "the sidebar status opens the live detail panel");
  assert.equal(s.item().pressed, true);

  const pause = s.command("pause");
  pause.run();
  assert.deepEqual(s.pauses, [true]);
  assert.equal(s.item().label, "Converse: Paused");
  assert.match(s.item().title, /keeps running/);

  pause.run();
  assert.deepEqual(s.pauses, [true, false]);
  assert.equal(s.item().label, "Converse: Speaking");
  assert.equal(s.panels.length, 1, "pause and resume never replace the panel");
  s.cleanup();
});

test("GUI errors outlive the idle status, open the sidebar panel and can expire", () => {
  const s = statusSetup({ status: "thinking" });
  s.fail("engine crashed");
  const failed = s.item();
  assert.equal(failed.label, "Converse: Error");
  assert.equal(failed.icon, "circle-exclamation");
  assert.equal(failed.pressed, true, "the running conversation keeps its controls");

  const view = s.shown();
  assert.equal(view.tone, "danger");
  assert.equal(view.rows, undefined, "errors surface in the popover, not inline");
  assert.equal(typeof view.menu, "function");
  s.item().run();
  assert.equal(s.panels.length, 1);
  s.cleanup();
});

test("GUI error chip expires after the presence window", (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const s = statusSetup({ status: "off" });
  s.fail("start failed");
  assert.equal(s.item().label, "Converse: Error");
  t.mock.timers.tick(PRESENCE_ERROR_MS);
  assert.equal(s.item().label, "Converse: Idle", "the chip lapses back to the engine state");
  s.cleanup();
});
