import assert from "node:assert/strict";
import test from "node:test";
import { hostTransports } from "./client-host-transport.mjs";

test("finite host deadlines do not cap the lifecycle-bound event response body", async (t) => {
  const deadlines = [];
  t.mock.method(AbortSignal, "timeout", () => {
    const controller = new AbortController();
    deadlines.push(controller);
    return controller.signal;
  });
  const bodies = [];
  const transports = hostTransports(
    "http://127.0.0.1:1234",
    async (_url, init) =>
      new Response(
        new ReadableStream({
          start(controller) {
            bodies.push(controller);
            init.signal.addEventListener(
              "abort",
              () => controller.error(new Error("aborted transport")),
              { once: true },
            );
          },
        }),
      ),
  );
  const lifecycle = new AbortController();
  const finite = await transports.request("http://127.0.0.1:1234/finite", {
    signal: lifecycle.signal,
  });
  const response = await transports.subscription("http://127.0.0.1:1234/events", {
    signal: lifecycle.signal,
  });
  assert.equal(deadlines.length, 1, "only finite requests receive the 15-second deadline");
  deadlines[0].abort();
  await assert.rejects(finite.body.getReader().read(), /aborted transport/);
  const reader = response.body.getReader();
  bodies[1].enqueue(new TextEncoder().encode("event after finite deadline"));
  assert.equal(
    new TextDecoder().decode((await reader.read()).value),
    "event after finite deadline",
  );
  const pending = reader.read();
  lifecycle.abort();
  await assert.rejects(pending, /aborted transport/);
  assert.throws(
    () => transports.subscription("https://elsewhere.invalid/events", { signal: lifecycle.signal }),
    /owned server/,
  );
});
