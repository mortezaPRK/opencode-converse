import { createSignal } from "solid-js";
import {
  Command,
  ComposerPill,
  Plugin,
  TitlebarItem,
  type ComposerPillRow,
  type ComposerPillTone,
} from "@opencode/plugin/gui";
import { ConverseDesktop } from "./gui/contract.js";
import { main } from "./gui/main.js";
import {
  normalizeVoiceStatus,
  presence,
  PRESENCE_ERROR_MS,
  type PresenceTone,
} from "./presence.js";

/** The composer pill colors its icon with these tones only; the rest keep the app default. */
const pillTone = (tone: PresenceTone): ComposerPillTone | undefined =>
  tone === "success" || tone === "warning" || tone === "danger" ? tone : undefined;

export default Plugin.define({
  main,
  setup(ctx) {
    const audio = ctx.main(ConverseDesktop);
    // The engine is idle after a failed turn or start, so the error outlives the
    // status change but not the surface: it lapses on its own timer.
    const [failure, setFailure] = createSignal<string>();
    let failureTimer: ReturnType<typeof setTimeout> | undefined;
    const report = (message: string) => {
      setFailure(message);
      clearTimeout(failureTimer);
      failureTimer = setTimeout(() => setFailure(undefined), PRESENCE_ERROR_MS);
    };
    const view = () => presence(normalizeVoiceStatus(audio.state()?.status), failure());
    const attempt = async (action: () => void | Promise<void>) => {
      try {
        await action();
      } catch (error) {
        ctx.toast.show({ title: "Converse", description: String(error), variant: "error" });
      }
    };
    const start = async () => {
      const session = ctx.sessions.current();
      if (!session)
        throw new Error(
          "Open a session on the desired server and location before starting Converse",
        );
      await audio.start(
        { serverID: session.server.id, directory: session.directory },
        { signal: ctx.signal },
      );
    };
    const open = () => {
      const state = audio.state();
      if (state?.serverID && state.sessionID)
        ctx.tabs.open({ server: state.serverID, id: state.sessionID }, { focus: true });
    };
    const actions = [
      { id: "start", title: "start conversation", run: () => attempt(start) },
      {
        id: "stop",
        title: "stop conversation",
        run: () => attempt(() => audio.stop({ signal: ctx.signal })),
      },
      {
        id: "mute",
        title: "mute/unmute microphone",
        run: () => attempt(() => audio.mute({ signal: ctx.signal })),
      },
      { id: "open", title: "open supervisor session", run: open },
      {
        id: "status",
        title: "show status",
        run: () => {
          ctx.toast.show({
            title: "Converse",
            description: `${view().label}. ${view().detail}`,
          });
        },
      },
    ];
    for (const action of actions)
      ctx.add(Command, () => ({
        id: action.id,
        title: `Converse: ${action.title}`,
        group: "Converse",
        slash: { name: `converse-${action.id}` },
        enabled: ctx.app.platform === "desktop",
        run: action.run,
      }));
    // The sidebar pill in a vertical tab layout, and the titlebar pill otherwise.
    ctx.add(TitlebarItem, () => ({
      id: "status",
      label: `Converse: ${view().label}`,
      title: `Converse: ${view().label}. ${view().detail}`,
      icon: view().icon,
      // Starting has no controls to offer yet; the host's spinner says so.
      busy: view().state === "loading",
      pressed: view().active,
      run: () => {
        void attempt(view().active ? () => audio.stop({ signal: ctx.signal }) : start);
      },
    }));
    // Above the supervisor's composer, a state chip with labels and the actions
    // that apply; other sessions never show it.
    ctx.add(ComposerPill, {
      id: "status",
      show: (input) => {
        const state = audio.state();
        const current = view();
        if (!state?.sessionID || input.session.id !== state.sessionID) return;
        if (!current.active && current.state !== "error") return;
        const muted = state.status === "muted";
        const rows: ComposerPillRow[] = [];
        if (current.state === "error")
          rows.push({
            id: "dismiss",
            label: "Dismiss error",
            icon: "outline-xmark",
            run: () => setFailure(undefined),
          });
        rows.push(
          {
            id: "mute",
            label: muted ? "Unmute microphone" : "Mute microphone",
            icon: muted ? "outline-audio" : "outline-eye-slash",
            run: () => void attempt(() => audio.mute({ signal: ctx.signal })),
          },
          {
            id: "open",
            label: "Open supervisor session",
            icon: "square-arrow-top-right",
            run: open,
          },
          {
            id: "stop",
            label: "Stop conversation",
            icon: "stop",
            tone: "danger",
            run: () => void attempt(() => audio.stop({ signal: ctx.signal })),
          },
        );
        return {
          label: `Converse · ${current.label}`,
          icon: current.icon,
          tone: pillTone(current.tone),
          title: `Converse: ${current.label}. ${current.detail}`,
          rows,
        };
      },
    });
    const remove = audio.on("error", ({ message }) => {
      report(message);
      ctx.toast.show({ title: "Converse", description: message, variant: "error" });
    });
    return () => {
      clearTimeout(failureTimer);
      remove();
    };
  },
});
