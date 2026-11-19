import { dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { createEffect, createSignal, on, onCleanup, Show } from "solid-js";
import { Plugin } from "@opencode/plugin/tui";
import { createBridge, isStoppedError, type Snapshot } from "./client/bridge.js";
import { localSettings } from "./client/runtime.js";
import { parseRuntimeConfig } from "./config.js";
import { presence, presenceGlyph, PRESENCE_ERROR_MS, type PresenceTone } from "./presence.js";
import type { LocalSettings } from "./protocol.js";

/** dist/tui.js -> package root. */
const PACKAGE_ROOT = dirname(dirname(fileURLToPath(import.meta.url)));

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
      // A failed turn or start leaves the engine idle, so the error outlives the
      // status change but never the conversation: it lapses on its own timer.
      const [failure, setFailure] = createSignal<string>();
      let failureTimer: ReturnType<typeof setTimeout> | undefined;
      const report = (message: string) => {
        setFailure(message);
        clearTimeout(failureTimer);
        failureTimer = setTimeout(() => setFailure(undefined), PRESENCE_ERROR_MS);
      };
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
        const runtimeConfig = parseRuntimeConfig(ctx.options);
        const current = (dependencies.bridge ?? createBridge)({
          client: ctx.client,
          // The TUI exposes one authenticated connection, not a desktop server registry.
          target: { serverID: "connected", directory: location.directory },
          settings: localSettings(prefs),
          packageRoot: PACKAGE_ROOT,
          ...(Object.keys(runtimeConfig).length ? { runtimeConfig } : {}),
          onChange(next) {
            if (!disposed && bridge === current) setState(next);
          },
          onError(error) {
            if (disposed) disposalErrors.push(error);
            else if (bridge === current) {
              report(error.message);
              ctx.ui.toast.show({ title: "Converse", message: error.message, variant: "error" });
            }
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
      const view = () => presence(state().status, failure());
      const toneColor = (tone: PresenceTone) => {
        if (tone === "success") return ctx.theme.text.feedback.success.base;
        if (tone === "info") return ctx.theme.text.feedback.info.base;
        if (tone === "warning") return ctx.theme.text.feedback.warning.base;
        if (tone === "danger") return ctx.theme.text.feedback.error.base;
        if (tone === "muted") return ctx.theme.text.muted;
        return ctx.theme.text.base;
      };
      // The one moving part: a state that has several frames steps through them
      // at its own pace, and idle or muted states hold a single glyph still.
      const Glyph = () => {
        const [frame, setFrame] = createSignal(0);
        let ticker: ReturnType<typeof setInterval> | undefined;
        createEffect(
          on(view, (current) => {
            setFrame(0);
            clearInterval(ticker);
            ticker =
              current.frames.length > 1
                ? setInterval(() => setFrame((value) => value + 1), current.interval)
                : undefined;
          }),
        );
        onCleanup(() => clearInterval(ticker));
        return (
          <span style={{ fg: toneColor(view().tone) }}>{presenceGlyph(view(), frame())} </span>
        );
      };
      const InlineStatus = () => (
        <box flexShrink={0}>
          <text wrapMode="none">
            <Glyph />
            <span style={{ fg: ctx.theme.text.base }}>Converse</span>
            <span style={{ fg: ctx.theme.text.muted }}> · {view().label}</span>
          </text>
        </box>
      );
      const SidebarStatus = () => (
        <Show when={view().active || view().state === "error"}>
          <box
            backgroundColor={ctx.theme.background.raised.high}
            paddingTop={1}
            paddingBottom={1}
            paddingLeft={2}
            paddingRight={2}
            gap={1}
            onMouseUp={() => {
              if (state().sessionID) open();
            }}
          >
            <box flexDirection="row" gap={1} justifyContent="space-between">
              <text fg={ctx.theme.text.base}>
                <b>Converse</b>
              </text>
              <text fg={toneColor(view().tone)} wrapMode="none">
                <Glyph />
                {view().label}
              </text>
            </box>
            <text fg={ctx.theme.text.muted}>{view().detail}</text>
          </box>
        </Show>
      );
      const commands = [
        { id: "start", title: "start conversation", run: () => attempt(start) },
        { id: "stop", title: "stop conversation", run: () => attempt(stop) },
        { id: "mute", title: "mute/unmute microphone", run: () => attempt(() => bridge?.mute()) },
        { id: "open", title: "open supervisor session", run: open },
        {
          id: "status",
          title: "show status",
          run: () => {
            ctx.ui.toast.show({
              title: "Converse",
              message: `${view().label}. ${view().detail}`,
            });
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
        render: () => <InlineStatus />,
      });
      const removePrompt = ctx.ui.slot({
        append: "prompt.footer.status",
        render: () => <InlineStatus />,
      });
      // The session sidebar is where a running conversation should be visible;
      // an idle one stays in the footer so every session is not cluttered.
      const removeSidebar = ctx.ui.slot({
        append: "sidebar.footer",
        render: () => <SidebarStatus />,
      });
      return async () => {
        disposalErrors.length = 0;
        disposed = true;
        clearTimeout(failureTimer);
        removeCommands();
        removeStatus();
        removePrompt();
        removeSidebar();
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
