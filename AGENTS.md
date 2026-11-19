# OpenCode Converse

OpenCode plugin and local Rust voice runtime.

- Rust owns audio, speech, text shaping, and conversation state. TypeScript owns OpenCode SDK integration and UI adapters only.
- One local Rust subprocess per conversation; JSON lines over stdio. Never send server credentials to Rust.
- Engine and speech-model locations are fixed under the app state folder. Plugin options or environment variables must not select or override them. Copy the packaged engine into the state folder on first use, keep models reusable there, and never send server credentials to Rust. Device preferences stay in host storage.
- Bind every conversation to the server and location selected at start. Never silently retarget or discover a different server.
- Supervisor tools must not approve permission requests. The user approves through OpenCode.
- Long output and rolling event history must stay bounded without ending otherwise healthy conversations.
- No historical aliases, migration instructions, alternative native loaders, or fallback coding agents.
- Preserve third-party licenses and attribution. Native binary/model redistribution requires an exact-artifact license review.
- Use `functions.patch` for edits. Keep generated build products and dependency folders out of Git.
- Verify TypeScript with `npm run check` and `npm test`; Rust with the commands in package.json. Report platform/device checks that were not run.
