import { OpenCode } from "@opencode/client";
import type { Plugin } from "@opencode/plugin/gui";
import { createBridge, isStoppedError, type Snapshot } from "../client/bridge.js";
import { localSettings } from "../client/runtime.js";
import { parseRuntimeConfig } from "../config.js";
import { Converse } from "../rpc.js";
import { ConverseDesktop, Devices } from "./contract.js";

export function createMain(
  dependencies: { makeClient?: typeof OpenCode.make; bridge?: typeof createBridge } = {},
): Plugin.MainSetup {
  return (ctx) => {
    const prefs = ctx.storage.store("devices", { schema: Devices, initial: {} });
    let state: Snapshot = {
      status: "off",
      paused: false,
      serverID: null,
      sessionID: null,
      interrupted: null,
    };
    let owner: number | undefined;
    let bridge: ReturnType<typeof createBridge> | undefined;
    let disposed = false;
    let stopRequests = 0;
    const disposalErrors: unknown[] = [];
    let operation = Promise.resolve();
    const serialize = (action: () => Promise<void>) => {
      const next = operation.then(action);
      operation = next.catch(() => {});
      return next;
    };
    const stop = async () => {
      const current = bridge;
      await current?.stop();
      if (bridge === current) {
        bridge = undefined;
        owner = undefined;
      }
    };
    const checkOwner = (window: number) => {
      if (owner !== undefined && owner !== window)
        throw new Error("Converse is owned by another window");
    };
    const provider = ctx.provide(ConverseDesktop, {
      state: () => state,
      start(input, caller) {
        const observedStop = stopRequests;
        return serialize(async () => {
          if (
            observedStop !== stopRequests ||
            disposed ||
            caller.signal.aborted ||
            ctx.signal.aborted
          )
            return;
          checkOwner(caller.window);
          if (bridge && state.status !== "off") return;
          if (bridge) await stop();
          if (
            observedStop !== stopRequests ||
            disposed ||
            caller.signal.aborted ||
            ctx.signal.aborted
          )
            return;
          if (!input.directory.trim())
            throw new Error("Select a location before starting Converse");
          const endpoint = ctx.serverEndpoints.get(input.serverID);
          if (!endpoint || endpoint.id !== input.serverID)
            throw new Error(
              "Selected server is unavailable; reconnect it before starting Converse",
            );
          // Both local sidecar and remote servers use this exact authenticated path.
          // Neither the URL nor credentials cross window IPC or runtime stdio.
          const client = (dependencies.makeClient ?? OpenCode.make)({
            baseUrl: endpoint.url,
            headers: { ...endpoint.headers },
          });
          owner = caller.window;
          const runtimeConfig = parseRuntimeConfig(ctx.options);
          let packageRoot: string | undefined;
          try {
            const hello = await ctx.client.rpc(Converse).hello(undefined, {
              signal: AbortSignal.any([caller.signal, AbortSignal.timeout(5000)]),
            });
            packageRoot = hello.packageRoot;
          } catch (error) {
            throw new Error(
              "Converse could not read the local plugin package; restart OpenCode or reinstall the plugin.",
              { cause: error },
            );
          }
          if (!packageRoot)
            throw new Error(
              "Converse's local plugin did not report its package root; reinstall the plugin.",
            );
          const current = (dependencies.bridge ?? createBridge)({
            client,
            target: { serverID: input.serverID, directory: input.directory },
            settings: localSettings(prefs.value),
            packageRoot,
            ...(Object.keys(runtimeConfig).length ? { runtimeConfig } : {}),
            onChange(next) {
              if (disposed || bridge !== current) return;
              state = next;
              if (next.status === "off") {
                owner = undefined;
                // Release window ownership, but retain the retiring bridge so a
                // start already ahead of this cleanup must await its teardown.
                void serialize(async () => {
                  if (bridge === current) await stop();
                }).catch(() => {});
              }
              provider.changed();
            },
            onError(error) {
              if (disposed && !isStoppedError(error)) disposalErrors.push(error);
              else if (!disposed && bridge === current)
                provider.emit("error", { message: error.message }, owner);
            },
          });
          bridge = current;
          const cancel = () => {
            void current.stop().catch(() => {});
          };
          caller.signal.addEventListener("abort", cancel, { once: true });
          try {
            await current.start();
          } catch (error) {
            // The queue tail swallows failures only for sequencing. Retain real
            // disposal-time startup failures independently before releasing it.
            if (disposed && !isStoppedError(error)) disposalErrors.push(error);
            await stop().catch(() => {});
            throw error;
          } finally {
            caller.signal.removeEventListener("abort", cancel);
          }
        });
      },
      async stop(_input, caller) {
        checkOwner(caller.window);
        stopRequests++;
        const current = bridge;
        void current?.stop().catch(() => {});
        return serialize(async () => {
          if (bridge === current) {
            checkOwner(caller.window);
            await stop();
          }
        });
      },
      mute(_input, caller) {
        return serialize(async () => {
          checkOwner(caller.window);
          bridge?.mute();
        });
      },
      pause(input, caller) {
        return serialize(async () => {
          checkOwner(caller.window);
          bridge?.pause(input.paused);
        });
      },
    });
    const close = ctx.windows.on("close", (window) => {
      if (window.id !== owner) return;
      // Cancel startup now, not after its handshake completes in the queue.
      const current = bridge;
      void current?.stop().catch(() => {});
      void serialize(async () => {
        if (bridge === current) await stop();
      }).catch(() => {});
    });
    return async () => {
      disposalErrors.length = 0;
      disposed = true;
      close();
      await bridge?.stop().catch(() => {});
      await operation;
      await stop();
      provider.dispose();
      if (disposalErrors.length)
        throw new AggregateError(
          [...new Set(disposalErrors)],
          "Converse GUI disposal cleanup failed; a session may remain active",
        );
    };
  };
}

export const main = createMain();
