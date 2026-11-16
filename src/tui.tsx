import { createSignal } from "solid-js";
import { Plugin } from "@opencode/plugin/tui";
import { createBridge, isStoppedError, type Snapshot } from "./client/bridge.js";
import { localSettings } from "./client/runtime.js";
import type { LocalSettings } from "./protocol.js";

export function createTui(dependencies: { bridge?: typeof createBridge } = {}) {
  return Plugin.define({
    id: "converse",
    setup(ctx) {
      const [prefs] = ctx.storage.store<LocalSettings>("devices", { initial: {} });
      const [state, setState] = createSignal<Snapshot>({
        status: "off",
        sessionID: null,
        serverID: null,
      });
      let bridge: ReturnType<typeof createBridge> | undefined;
      let disposed = false;
      let stopGeneration = 0;
      const pendingStarts = new Set<Promise<void>>();
      const disposalErrors: unknown[] = [];
      const attempt = async (action: () => void | Promise<void>) => {
        try {
          await action();
        } catch (error) {
          if (!disposed)
            ctx.ui.toast.show({ title: "Converse", message: String(error), variant: "error" });
        }
      };
      const start = async () => {
        if (disposed) return;
        if (bridge && state().status !== "off") return;
        const retiring = bridge;
        const observedStop = stopGeneration;
        if (retiring) await retiring.stop();
        // Another start may have acquired the bridge during the same teardown.
        // Stop/disposal also fences starts that were already waiting here.
        if (disposed || observedStop !== stopGeneration || (bridge && bridge !== retiring)) return;
        const location = ctx.location ?? ctx.data.location.default();
        if (!location) throw new Error("Select an OpenCode location before starting Converse");
        const selected = ctx.ui.model.current();
        const current = (dependencies.bridge ?? createBridge)({
          client: ctx.client,
          // The TUI exposes one authenticated connection, not a desktop server registry.
          target: {
            serverID: "connected",
            directory: location.directory,
            ...(selected
              ? {
                  model: {
                    providerID: selected.providerID,
                    id: selected.modelID,
                    ...(selected.variant ? { variant: selected.variant } : {}),
                  },
                }
              : {}),
          },
          settings: localSettings(prefs),
          onChange(next) {
            if (!disposed && bridge === current) setState(next);
          },
          onError(error) {
            if (disposed) disposalErrors.push(error);
            else if (bridge === current)
              ctx.ui.toast.show({ title: "Converse", message: error.message, variant: "error" });
          },
        });
        bridge = current;
        let resolveStart!: () => void;
        let rejectStart!: (error: unknown) => void;
        const pending = new Promise<void>((resolve, reject) => {
          resolveStart = resolve;
          rejectStart = reject;
        });
        pendingStarts.add(pending);
        // Publish tracking before startup can synchronously notify the host.
        // stop() intentionally does not await this promise, avoiding a cycle.
        void (async () => {
          try {
            await current.start();
            if (disposed) await current.stop();
          } catch (error) {
            if (disposed && !isStoppedError(error)) disposalErrors.push(error);
            await current.stop();
            if (bridge === current) bridge = undefined;
            throw error;
          }
        })().then(resolveStart, rejectStart);
        try {
          await pending;
        } finally {
          pendingStarts.delete(pending);
        }
      };
      const stop = async () => {
        stopGeneration++;
        const current = bridge;
        if (!current) return;
        await current.stop();
        if (bridge === current) bridge = undefined;
      };
      const open = () => {
        const sessionID = state().sessionID;
        if (sessionID) ctx.ui.router.navigate({ type: "session", sessionID });
      };
      const commands = [
        { id: "start", title: "start conversation", run: () => attempt(start) },
        { id: "stop", title: "stop conversation", run: () => attempt(stop) },
        { id: "mute", title: "mute/unmute microphone", run: () => attempt(() => bridge?.mute()) },
        { id: "open", title: "open supervisor session", run: open },
        {
          id: "status",
          title: "show status",
          run: () => {
            ctx.ui.toast.show({ title: "Converse", message: state().status });
          },
        },
      ];
      const removeCommands = ctx.ui.slot({
        append: "app",
        render: () => {
          ctx.keymap.layer(() => ({
            mode: "global",
            commands: commands.map((command) => ({
              ...command,
              id: `converse.${command.id}`,
              title: `Converse: ${command.title}`,
              group: "Converse",
              palette: true,
              slash: { name: `converse-${command.id}` },
            })),
            bindings: commands.map((command) => `converse.${command.id}`),
          }));
          return null;
        },
      });
      const removeStatus = ctx.ui.slot({
        append: "home.footer.status",
        render: () => <text fg={ctx.theme.text.base}>Converse {state().status}</text>,
      });
      const removePrompt = ctx.ui.slot({
        append: "prompt.footer.status",
        render: () => <text fg={ctx.theme.text.base}>Converse {state().status}</text>,
      });
      return async () => {
        disposalErrors.length = 0;
        disposed = true;
        removeCommands();
        removeStatus();
        removePrompt();
        try {
          await stop();
        } catch (error) {
          disposalErrors.push(error);
        }
        const outcomes = await Promise.allSettled(pendingStarts);
        for (const outcome of outcomes)
          if (outcome.status === "rejected" && !isStoppedError(outcome.reason))
            disposalErrors.push(outcome.reason);
        if (disposalErrors.length)
          throw new AggregateError(
            [...new Set(disposalErrors)],
            "Converse disposal cleanup failed; a session or runtime may remain active",
          );
      };
    },
  });
}

export default createTui();
