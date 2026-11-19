import { setTimeout as delay } from "node:timers/promises";
import { isRpcError, type OpenCodeClient } from "@opencode/client";

/** Abort even if an SDK adapter has not settled its outstanding read yet. */
function abortable<T>(read: Promise<T>, signal: AbortSignal): Promise<T> {
  return new Promise((resolve, reject) => {
    const cleanup = () => signal.removeEventListener("abort", aborted);
    const aborted = () => {
      cleanup();
      reject(signal.reason);
    };
    signal.addEventListener("abort", aborted, { once: true });
    void read.then(
      (value) => {
        cleanup();
        resolve(value);
      },
      (error: unknown) => {
        cleanup();
        reject(error);
      },
    );
    if (signal.aborted) aborted();
  });
}

/** Location plugins load lazily. Readiness and safe hello share one fixed budget. */
export async function waitForConverse(
  client: OpenCodeClient,
  directory: string,
  hello: (signal: AbortSignal) => Promise<{ protocol: 2 }>,
  lifetime: AbortSignal,
): Promise<{ protocol: 2 }> {
  const deadline = new AbortController();
  const signal = AbortSignal.any([lifetime, deadline.signal]);
  const timer = setTimeout(
    () =>
      deadline.abort(
        new Error(
          "Converse server plugin did not become ready within 15 seconds on the selected server. " +
            "Add @mortz.prk/opencode-converse to that server's OpenCode plugins and check its plugin load errors.",
        ),
      ),
    15_000,
  );
  let backoff = 100;
  try {
    while (true) {
      signal.throwIfAborted();
      const listed = await abortable(
        client.plugin.list({ location: { directory } }, { signal }),
        signal,
      );
      const plugin = listed.data.find((entry) => entry.id === "converse");
      if (plugin?.state.status === "failed") {
        throw new Error(
          `Converse server plugin failed to load: ${plugin.state.error}. Check the selected server's plugin configuration and logs.`,
        );
      }
      if (plugin?.state.status === "active" && plugin.features.server && plugin.features.rpc) {
        try {
          return await abortable(hello(signal), signal);
        } catch (error) {
          // A reload can withdraw RPC after plugin.list reports active. Only the
          // side-effect-free hello is retried; never allocate or replay a prompt.
          if (!isRpcError(error) || error.type !== "rpc.unavailable") throw error;
        }
      }
      await delay(backoff, undefined, { signal });
      backoff = Math.min(backoff * 2, 1000);
    }
  } catch (error) {
    if (deadline.signal.aborted && !lifetime.aborted) throw deadline.signal.reason;
    throw error;
  } finally {
    clearTimeout(timer);
  }
}
