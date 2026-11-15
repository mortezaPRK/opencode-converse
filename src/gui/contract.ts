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
    serverID: Schema.NullOr(Schema.String),
    sessionID: Schema.NullOr(Schema.String),
  }),
  methods: {
    start: {
      input: Schema.Struct({
        serverID: Schema.String,
        directory: Schema.String,
        model: Schema.optional(
          Schema.Struct({
            providerID: Schema.String,
            id: Schema.String,
            variant: Schema.optional(Schema.String),
          }),
        ),
      }),
    },
    stop: {},
    mute: {},
  },
  events: { error: Schema.Struct({ message: Schema.String }) },
});
