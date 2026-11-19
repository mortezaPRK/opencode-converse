/** @jsxImportSource solid-js */
import { Index, Show, createSignal } from "solid-js";
import {
  Command,
  ComposerPill,
  Panel,
  Plugin,
  TitlebarItem,
  useDrawer,
  type ComposerPillMenuProps,
  type ComposerPillTone,
  type SessionRef,
} from "@opencode/plugin/gui";
import { ConverseDesktop } from "./gui/contract.js";
import { main } from "./gui/main.js";
import { agentColor, entryLabel, speakerSuffix } from "./roster-view.js";
import {
  normalizeVoiceStatus,
  presence,
  PRESENCE_ERROR_MS,
  type Presence,
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
    const view = (): Presence => {
      const state = audio.state();
      const current = presence(normalizeVoiceStatus(state?.status), failure(), state?.progress);
      if (!state?.paused || current.state === "error") return current;
      return {
        ...current,
        label: "Paused",
        detail:
          state.status === "muted"
            ? "Spoken output paused. Microphone muted."
            : "Spoken output paused. The conversation keeps running.",
        icon: "outline-audio",
        tone: "warning",
        frames: ["❙❙"],
        interval: 0,
      };
    };
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
    // Start from the viewed session when idle; never retarget a running conversation.
    const attachCurrent = async () => {
      const session = ctx.sessions.current();
      if (!session) throw new Error("Open the session you want to talk to first");
      const live = audio.state();
      if (live?.sessionID) {
        if (live.sessionID === session.id)
          throw new Error("This is the Converse supervisor session");
        if (live.serverID !== session.server.id)
          throw new Error("Stop Converse and start it from a session on this server");
      } else await start();
      await audio.attach({ sessionID: session.id }, { signal: ctx.signal });
    };
    const detachCurrent = async () => {
      const session = ctx.sessions.current();
      if (!session) throw new Error("Open the session you want to stop talking to first");
      const live = audio.state();
      if (!live?.sessionID) throw new Error("Converse is not running");
      if (live.serverID !== session.server.id)
        throw new Error("This session is on a different server than the conversation");
      await audio.detach({ sessionID: session.id }, { signal: ctx.signal });
    };
    const open = () => {
      const state = audio.state();
      if (state?.serverID && state.sessionID)
        ctx.tabs.open({ server: state.serverID, id: state.sessionID }, { focus: true });
    };
    // The transcript is owned by the main-process bridge and arrives with the state,
    // so it never depends on the host's per-session message cache.
    const entries = () => audio.state()?.transcript ?? [];
    const multi = () => (audio.state()?.participants.length ?? 0) > 1;
    const paused = () => audio.state()?.paused === true;
    const muted = () => audio.state()?.status === "muted";
    const supervisor = () => {
      const live = audio.state();
      if (!live?.sessionID) return "No supervisor session.";
      const last = entries().at(-1);
      if (!last) return "Supervisor is idle. Say what you need.";
      return "The latest exchange is in the transcript.";
    };
    const DETAILS_PANEL = "details";
    const detailsKey = () => `${ctx.id}:${DETAILS_PANEL}`;
    // The host's native sidebar surface, not a centered modal. On narrow
    // screens the same view opens in the host's drawer.
    const Details = (props: { session: SessionRef }) => {
      const drawer = useDrawer();
      const close = () => {
        drawer?.close();
        ctx.layout.close(detailsKey(), props.session);
      };
      return (
        <div
          style={{
            display: "flex",
            "flex-direction": "column",
            gap: "10px",
            height: "100%",
            "min-height": 0,
            padding: "12px",
            "box-sizing": "border-box",
          }}
        >
          <div style={{ display: "flex", "flex-direction": "column", gap: "2px" }}>
            <div style={{ "font-weight": 600 }}>Activity</div>
            <div>
              {view().label}
              {speakerSuffix(audio.state() ?? { participants: [], speaker: null })}
            </div>
            <div style={{ opacity: 0.75 }}>{view().detail}</div>
            <div style={{ opacity: 0.75 }}>{supervisor()}</div>
            <Show when={multi()}>
              <div style={{ display: "flex", gap: "10px", "flex-wrap": "wrap" }}>
                <Index each={audio.state()?.participants ?? []}>
                  {(participant, index) => (
                    <span style={{ color: agentColor(index), "font-weight": 600 }}>
                      {participant().name}
                    </span>
                  )}
                </Index>
              </div>
            </Show>
            <Show when={paused()}>
              <div style={{ opacity: 0.75 }}>
                Spoken output is paused. The runtime and in-flight work keep running.
              </div>
            </Show>
          </div>
          <div
            style={{
              display: "flex",
              "flex-direction": "column",
              gap: "6px",
              flex: 1,
              "min-height": 0,
              "overflow-y": "auto",
            }}
          >
            <div style={{ "font-weight": 600 }}>Transcript</div>
            <Show
              when={entries().length > 0}
              fallback={<div style={{ opacity: 0.6 }}>No messages yet.</div>}
            >
              <div style={{ display: "flex", "flex-direction": "column", gap: "10px" }}>
                <Index each={entries()}>
                  {(entry) => (
                    <div style={{ display: "flex", "flex-direction": "column", gap: "2px" }}>
                      <div
                        style={{
                          "font-size": "11px",
                          "letter-spacing": "0.04em",
                          opacity: 0.85,
                          "text-transform": "uppercase",
                          color: agentColor(entry().agent),
                        }}
                      >
                        {entryLabel(entry(), multi())}
                      </div>
                      <div
                        style={{
                          "white-space": "pre-wrap",
                          "overflow-wrap": "anywhere",
                          ...(entry().role === "note"
                            ? { opacity: 0.7, "font-style": "italic" }
                            : {}),
                        }}
                      >
                        {entry().text}
                      </div>
                    </div>
                  )}
                </Index>
                <Show when={audio.state()?.interrupted}>
                  {(cut) => (
                    <div style={{ opacity: 0.7, "font-size": "12px" }}>
                      Interrupted
                      {cut().playedPercent === undefined
                        ? ""
                        : ` after ${cut().playedPercent}% of the spoken reply`}
                      {cut().heard ? `. Heard: “${cut().heard}”` : ""}
                    </div>
                  )}
                </Show>
              </div>
            </Show>
          </div>
          <div
            style={{
              display: "flex",
              gap: "6px",
              "flex-wrap": "wrap",
              "justify-content": "flex-end",
            }}
          >
            <Show when={failure()}>
              <ctx.ui.Button onClick={() => setFailure(undefined)}>Dismiss error</ctx.ui.Button>
            </Show>
            <ctx.ui.Button
              variant="contrast"
              disabled={!view().active}
              onClick={() => void attempt(() => audio.pause({ paused: !paused() }))}
            >
              {paused() ? "Resume spoken output" : "Pause spoken output"}
            </ctx.ui.Button>
            <ctx.ui.Button onClick={() => void attempt(() => audio.mute({ signal: ctx.signal }))}>
              {muted() ? "Unmute microphone" : "Mute microphone"}
            </ctx.ui.Button>
            <ctx.ui.Button onClick={open}>Open session</ctx.ui.Button>
            <ctx.ui.Button
              variant="danger"
              onClick={() => void attempt(() => audio.stop({ signal: ctx.signal }))}
            >
              Stop
            </ctx.ui.Button>
            <ctx.ui.Button onClick={close}>Close</ctx.ui.Button>
          </div>
        </div>
      );
    };
    const openDetails = () => {
      const current = audio.state();
      if (!current?.sessionID) {
        ctx.toast.show({ title: "Converse", description: "No live conversation to show." });
        return;
      }
      const sessions = ctx.sessions.list();
      const target =
        sessions.find(
          (session) => session.id === current.sessionID && session.server.id === current.serverID,
        ) ??
        ctx.sessions.current() ??
        sessions[0];
      if (!target) {
        ctx.toast.show({
          title: "Converse",
          description: "Open a session before showing Converse details.",
        });
        return;
      }
      ctx.layout.open(detailsKey(), target, { tab: "preview" });
    };
    ctx.add(Panel, {
      id: DETAILS_PANEL,
      region: "side",
      width: 380,
      transient: true,
      mobile: { title: "Converse", order: 10, kind: "drawer", icon: "waveform" },
      list: ({ open }) =>
        open.includes(DETAILS_PANEL) ? [{ id: DETAILS_PANEL, title: "Converse" }] : [],
      render: (props) => <Details session={props.session} />,
    });
    // The composer status chip opens this compact popover for quick control;
    // the full transcript stays in the sidebar panel behind "Transcript".
    const Quick = (props: { close(): void }) => (
      <div
        style={{
          display: "flex",
          "flex-direction": "column",
          gap: "10px",
          "min-width": "240px",
          "max-width": "340px",
        }}
      >
        <div style={{ display: "flex", "flex-direction": "column", gap: "2px" }}>
          <div>
            {view().label}
            {speakerSuffix(audio.state() ?? { participants: [], speaker: null })}
          </div>
          <div style={{ opacity: 0.75 }}>{view().detail}</div>
          <div style={{ opacity: 0.75 }}>{supervisor()}</div>
          <Show when={paused()}>
            <div style={{ opacity: 0.75 }}>Spoken output is paused; work keeps running.</div>
          </Show>
        </div>
        <div style={{ display: "flex", gap: "6px", "flex-wrap": "wrap" }}>
          <Show when={failure()}>
            <ctx.ui.Button onClick={() => setFailure(undefined)}>Dismiss error</ctx.ui.Button>
          </Show>
          <ctx.ui.Button
            variant="contrast"
            disabled={!view().active}
            onClick={() => void attempt(() => audio.pause({ paused: !paused() }))}
          >
            {paused() ? "Resume" : "Pause"}
          </ctx.ui.Button>
          <ctx.ui.Button onClick={() => void attempt(() => audio.mute({ signal: ctx.signal }))}>
            {muted() ? "Unmute" : "Mute"}
          </ctx.ui.Button>
          <ctx.ui.Button
            onClick={() => {
              props.close();
              openDetails();
            }}
          >
            Transcript
          </ctx.ui.Button>
          <ctx.ui.Button
            variant="danger"
            onClick={() => {
              props.close();
              void attempt(() => audio.stop({ signal: ctx.signal }));
            }}
          >
            Stop
          </ctx.ui.Button>
        </div>
      </div>
    );
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
      {
        id: "pause",
        title: "pause/resume spoken output",
        run: () => attempt(() => audio.pause({ paused: audio.state()?.paused !== true })),
      },
      { id: "attach", title: "talk to this session", run: () => attempt(attachCurrent) },
      { id: "detach", title: "stop talking to this session", run: () => attempt(detachCurrent) },
      { id: "open", title: "open supervisor session", run: open },
      { id: "details", title: "show conversation details", run: openDetails },
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
        if (view().active) openDetails();
        else void attempt(start);
      },
    }));
    // Above the supervisor's composer, the status chip opens a compact popover;
    // the full transcript remains in the sidebar panel. Other sessions never
    // show it.
    ctx.add(ComposerPill, {
      id: "status",
      show: (input) => {
        const state = audio.state();
        const current = view();
        if (!state?.sessionID || input.session.id !== state.sessionID) return;
        if (!current.active && current.state !== "error") return;
        return {
          label: `Converse · ${current.label}`,
          icon: current.icon,
          tone: pillTone(current.tone),
          title: `Converse: ${current.label}. Open quick controls.`,
          menu: (menu: ComposerPillMenuProps) => <Quick close={menu.close} />,
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
