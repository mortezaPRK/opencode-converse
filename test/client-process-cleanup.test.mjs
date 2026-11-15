import assert from "node:assert/strict";
import test from "node:test";
import { removeAfterClose } from "./client-process-cleanup.mjs";

test("failed signaling and unconfirmed close preserve owned storage and report its path", async () => {
  for (const throws of [false, true]) {
    const child = {
      kill() {
        if (throws) throw new Error("signal failed");
        return false;
      },
    };
    try {
      child.kill("SIGKILL");
    } catch {
      /* Confirmation, not kill success, controls removal. */
    }
    let removed = false;
    let error;
    try {
      await removeAfterClose(
        "owned-live-storage",
        { child, closed: new Promise(() => {}) },
        async () => {
          removed = true;
        },
        5,
      );
    } catch (failure) {
      error = failure;
    }
    assert.ok(error);
    assert.equal(removed, false, "unknown child closure cannot release its storage");
    assert.match(error.message, /preserving storage/);
    assert.ok(error.message.includes("owned-live-storage"));
  }
});

test("exit or signal status alone is not close confirmation; delayed close permits removal", async () => {
  let confirm;
  const closed = new Promise((resolve) => {
    confirm = resolve;
  });
  let removed = false;
  const cleaning = removeAfterClose(
    "owned-storage",
    { child: { exitCode: 0, signalCode: "SIGKILL" }, closed },
    async () => {
      removed = true;
    },
  );
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(removed, false);
  confirm();
  await cleaning;
  assert.equal(removed, true);
});

test("a spawned child without a closure promise never permits storage removal", async () => {
  let removed = false;
  await assert.rejects(
    removeAfterClose("untracked-storage", { child: {} }, async () => {
      removed = true;
    }),
    /preserving storage/,
  );
  assert.equal(removed, false);
});
