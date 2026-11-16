import assert from "node:assert/strict";
import { syncBuiltinESMExports } from "node:module";
import test from "node:test";
import { createBridge } from "../dist/client/bridge.js";

const turn = () => new Promise((resolve) => setImmediate(resolve));
const active = {
  id: "converse",
  features: { server: true, rpc: true },
  state: { status: "active" },
};

function clock(t) {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  // Update the helper's named node:timers/promises binding to the mocked clock.
  syncBuiltinESMExports();
  t.after(() => {
    t.mock.timers.reset();
    syncBuiltinESMExports();
  });
}

function setup({ list, hello }) {
  const calls = { list: 0, hello: 0, allocated: 0, runtime: 0 };
  let readSignal;
  const bridge = createBridge({
    client: {
      plugin: {
        list: async (input, options) => {
          assert.deepEqual(input, { location: { directory: "/selected" } });
          calls.list++;
          readSignal = options.signal;
          return list ? list(input, options) : { data: [active] };
        },
      },
      rpc: () => ({
        hello: async (_input, options) => {
          calls.hello++;
          readSignal = options.signal;
          return hello(options.signal);
        },
        start: async () => {
          calls.allocated++;
          return { sessionID: "unexpected" };
        },
        stop: async () => {},
      }),
    },
    target: { serverID: "selected-server", directory: "/selected" },
    settings: {},
    runtime() {
      calls.runtime++;
      return { ready: Promise.resolve(), send() {}, stop: async () => {} };
    },
    onChange() {},
    onError() {},
  });
  return { bridge, calls, signal: () => readSignal };
}

for (const phase of ["plugin.list", "hello"])
  test(`shared readiness deadline aborts ignored ${phase} without allocating`, async (t) => {
    clock(t);
    const s = setup({
      ...(phase === "plugin.list" ? { list: () => new Promise(() => {}) } : {}),
      hello: () => new Promise(() => {}),
    });
    t.after(() => s.bridge.stop());
    const starting = s.bridge.start();
    let settled = false;
    void starting.then(
      () => {
        settled = true;
      },
      () => {
        settled = true;
      },
    );
    const rejected = assert.rejects(starting, /15 seconds.*selected server/i);
    await turn();
    assert.equal(s.calls.allocated, 0);
    assert.equal(s.calls.runtime, 0);
    t.mock.timers.tick(15_000);
    await turn();
    assert.equal(settled, true, "an ignored adapter must not defeat the readiness deadline");
    await rejected;
    assert.equal(s.signal().aborted, true);
    assert.equal(s.calls.allocated, 0);
    assert.equal(s.calls.runtime, 0);
    await s.bridge.stop();
  });

test("readiness reload retry expires at the original fifteen-second budget", async (t) => {
  clock(t);
  let activate;
  let lists = 0;
  const s = setup({
    list: () =>
      ++lists < 3
        ? { data: [active] }
        : new Promise((resolve) => {
            activate = resolve;
          }),
    hello: async () => {
      throw { _tag: "RpcError", type: "rpc.unavailable", message: "Reloading" };
    },
  });
  t.after(() => s.bridge.stop());
  let settled = false;
  let failure;
  void s.bridge.start().then(
    () => {
      settled = true;
    },
    (error) => {
      settled = true;
      failure = error;
    },
  );
  await turn();
  t.mock.timers.tick(100);
  await turn();
  t.mock.timers.tick(200);
  await turn();
  assert.equal(typeof activate, "function");
  t.mock.timers.tick(14_500);
  activate({ data: [active] });
  await turn();
  assert.equal(s.calls.hello, 3);
  t.mock.timers.tick(199);
  await turn();
  assert.equal(settled, false);
  t.mock.timers.tick(1);
  await turn();
  assert.equal(settled, true, "reload retry must not reset the original deadline");
  assert.match(failure.message, /15 seconds/);
  assert.equal(s.signal().aborted, true);
  assert.equal(s.calls.allocated, 0);
  assert.equal(s.calls.runtime, 0);
});
