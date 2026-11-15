import { Command, Plugin, TitlebarItem } from "@opencode/plugin/gui";
import { ConverseDesktop } from "./gui/contract.js";
import { main } from "./gui/main.js";

export default Plugin.define({
  main,
  setup(ctx) {
    const audio = ctx.main(ConverseDesktop);
    const status = () => audio.state()?.status ?? "off";
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
      const model = ctx.screen.current()?.composer.model();
      await audio.start(
        {
          serverID: session.server.id,
          directory: session.directory,
          ...(model
            ? {
                model: {
                  providerID: model.providerID,
                  id: model.modelID,
                  ...(model.variant ? { variant: model.variant } : {}),
                },
              }
            : {}),
        },
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
          ctx.toast.show({ title: "Converse", description: status() });
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
    ctx.add(TitlebarItem, () => ({
      id: "status",
      label: `Converse: ${status()}`,
      pressed: status() !== "off",
      run: () => {
        void attempt(status() === "off" ? start : () => audio.stop({ signal: ctx.signal }));
      },
    }));
    return audio.on("error", ({ message }) => {
      ctx.toast.show({ title: "Converse", description: message, variant: "error" });
    });
  },
});
