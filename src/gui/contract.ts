import { Schema } from "effect";
import { Ipc } from "@opencode/plugin/gui";

export const Devices = Schema.Struct({
  inputDevice: Schema.optional(Schema.String),
  outputDevice: Schema.optional(Schema.String),
});
export const ConverseDesktop = Ipc.define({
  id: "converse.audio",
  state: Schema.Struct({
    status: Schema.String,
    paused: Schema.Boolean,
    serverID: Schema.NullOr(Schema.String),
    sessionID: Schema.NullOr(Schema.String),
    interrupted: Schema.NullOr(
      Schema.Struct({
        heard: Schema.optional(Schema.String),
        playedPercent: Schema.optional(Schema.Number),
      }),
    ),
  }),
  methods: {
    start: {
      input: Schema.Struct({
        serverID: Schema.String,
        directory: Schema.String,
      }),
    },
    stop: {},
    mute: {},
    pause: {
      input: Schema.Struct({ paused: Schema.Boolean }),
    },
  },
  events: { error: Schema.Struct({ message: Schema.String }) },
});
