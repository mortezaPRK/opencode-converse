# OpenCode Converse

A local voice supervisor for OpenCode. Talk to a dedicated supervisor, delegate
work to sessions, and hear progress while keeping speech processing on your
machine.

**Development preview.** The source targets OpenCode terminal and Desktop on
macOS, Linux, and Windows. Native packaging, real-device testing on all three
systems, and exact-artifact license review are release gates—not completed
support claims. The package has not been published.

The current SDK target is OpenCode **2.3.0**.

## Design

- Rust owns capture, transcription, synthesis, turn-taking, speech shaping, and
  conversation state.
- A small TypeScript plugin connects the Rust process to OpenCode's authenticated
  client and adds terminal/Desktop controls.
- The supervisor and worker sessions run on the selected OpenCode server. Audio
  and speech models stay local, including when that server is remote.
- The voice agent always has its own session (a hidden `converse` supervisor)
  and never owns other sessions. Sessions it creates default to a hidden
  `converse-worker` agent, which is only their creation-time agent; no session
  is ever branded, hidden, retitled, or switched to another agent.
- A conversation can have up to four named voice agents (`agents` option). Each has
  its own voice and its own OpenCode session; you address one by name, and with no
  name the first agent answers. Rust owns addressing, the speaking floor, and the
  per-speaker voice; the plugin only routes each prompt to the addressee's session.
- With two or more agents they can talk to each other (`agentChat`, on by default),
  bounded by the engine: at most 4 agent turns, 90 seconds and 2 round trips per
  thing you say; speaking ends it. Only one agent prompts a followed session, and
  only you can reassign it ("Grace, take over the build session").
- English speech is the initial scope. Permission requests are announced, but
  approvals happen in OpenCode's normal interface.
- The microphone starts only on an explicit command. Stopping a conversation
  releases its local runtime. Echo cancellation (`echoCancellation`, on by default)
  lets you talk over the assistant on speakers, but it is verified only offline
  (simulated echo) so far, not on real devices; if the assistant interrupts itself
  or you cannot interrupt it, use headphones or set `echoCancellation: "off"`.

## Develop

Use Node.js 26 and Rust. Native inference builds additionally require CMake, meson,
ninja, a C/C++ compiler, and libclang. Linux builds need ALSA development headers and
`pkg-config`. Windows native builds require MSVC and the Windows SDK.

```sh
npm ci
npm run build
npm run engine:build
```

The engine is built under `native/target/release/` (`.exe` on Windows). Stage it
into the npm package before packing:

```sh
npm run engine:build
npm run engine:stage
```

The plugin copies the staged engine and its shared libraries into the fixed app
state folder on first use. There is no engine path option and no environment
override. If models are missing, the runtime error prints the exact installed
engine path to run. Model installation and diagnostics are explicit local
operations:

```sh
<state>/bin/<version>/opencode-converse models install
<state>/bin/<version>/opencode-converse doctor
```

Model URLs, sizes, and SHA-256 digests are recorded in
[`assets/models.json`](assets/models.json). The same file defines the VAD, turn,
ASR, and TTS roles and their runtime paths. ASR is Moonshine v2 Small Streaming
(123M, English, MIT; run on ONNX Runtime). TTS is Gradium Phonon 31M (Apache-2.0
weights; see `THIRD_PARTY.md`). No weights are shipped in the npm package. Review the asset terms before downloading or redistributing them.
Models stay in `<state>/models`, so they are reused when the engine version
changes.

Add the **package root** to your global `opencode.jsonc`. Converse requires
explicit supervisor and worker models:

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "plugins": [
    {
      "package": "/absolute/path/to/opencode-converse",
      "options": {
        "supervisorModel": "provider/supervisor-model",
        "workerModel": "provider/worker-model",
      },
    },
  ],
}
```

The full option list, model reference forms, speech-model overrides, and path
rules are documented in [configuration](docs/configuration.md) and
[`options.schema.json`](docs/options.schema.json).

The server, terminal, and Desktop entries are one plugin package. For a remote
connection, configure the plugin on the remote server too. The Rust engine and
speech models always stay in the local app state folder of the machine running
the UI. Desktop loads its UI entry from its own local server; its voice commands
target the server selected when the conversation starts. Rebuild, then use
`/reload` to reload development changes.

## Use

Open the desired workspace/server, then choose **Converse: start conversation**
from the command palette or run `/converse-start`.

| Command             | Action                                                |
| ------------------- | ----------------------------------------------------- |
| `/converse-start`   | Start a conversation on the selected server/workspace |
| `/converse-stop`    | Stop and release the local runtime                    |
| `/converse-mute`    | Mute or unmute microphone capture                     |
| `/converse-pause`   | Pause or resume spoken output                         |
| `/converse-attach`  | Talk to the session you are viewing (starts if idle)  |
| `/converse-detach`  | Stop talking to the session you are viewing           |
| `/converse-open`    | Open the dedicated supervisor session                 |
| `/converse-details` | Show the live transcript, activity, and controls      |

Click the Converse status control, or run `/converse-details`, to open the
host's sidebar panel with the live transcript and pause/resume, mute,
open-session, and stop controls. Narrow screens use the host's drawer. Click
the composer status chip for a compact pop-up with the current activity and
quick pause/resume, mute, transcript, and stop controls. Pausing spoken output
keeps the local runtime, the supervisor, and any in-flight work running; the held
speech continues on resume. Speaking while paused counts as an interruption and
discards the held speech. If you cut a reply off, the transcript marks it
"Interrupted" and shows the part you heard (an estimate; the agent's context is
unchanged). `endpointing` (`responsive` or `patient`) tunes how long the engine
waits in silence before ending your turn; `acknowledgement` (`auto` or `off`)
controls a short neutral spoken cue ("Okay.", "On it.") when the reply is slow to
start; `echoCancellation` (`on` or `off`) removes the assistant's own voice from
the microphone; see [configuration](docs/configuration.md).

The transcript (latest 50 entries) is kept by the voice bridge, not read from the
host's session cache, so it shows your spoken prompts and the supervisor's
completed replies even while you are viewing another session.

Ask the supervisor to delegate a task, inspect a session it follows, steer it, or
interrupt it. Say "start a plan session about X" and it creates a new session with
that visible OpenCode agent (`converse_list_agents`); without one, the hidden
`converse-worker` agent is used. An existing session's agent is never changed.
To talk to an existing session, run `/converse-attach` while viewing it, or ask
the supervisor to attach one from `converse_list_sessions`. Only sessions at the
conversation's own location can be attached, never supervisor sessions or
sessions another active conversation already follows. The voice agent never
owns a session: attaching only reads it and records it in the plugin's own
storage, sending nothing and changing no metadata, title, or agent. Followed
sessions (attached or created) can be prompted, interrupted, and announced, and
up to 16 can be followed at once. `/converse-detach` (or asking the supervisor)
stops following a session; this leaves the session and any running work
untouched, and stopping the conversation detaches everything. Sessions that
earlier versions marked with `converse:worker` metadata are not treated as
followed. The supervisor reports updates for followed sessions while you are
listening. Permission requests must be decided on screen. If a server connection drops, start a new
conversation explicitly; user prompts are not automatically replayed.
Long replies are shortened for speech; the full response remains available in
the supervisor session.

## Configuration

Converse is configured through plugin options. `supervisorModel` and
`workerModel` are required; there is no implicit fallback to the composer
selection or to an OpenCode-provided agent. Each accepts `"provider/model"`,
`"provider/model#variant"`, or the object form
`{ "providerID": "…", "id": "…", "variant": "…" }`.

The engine and speech-model locations are fixed under the app state folder.
There is no engine, data, or model path option and no `CONVERSE_BIN` override.
The plugin copies the packaged engine into `<state>/bin/<version>/` on first use,
and speech models stay in `<state>/models` so they are reused across engine
versions. `asrThreads`, `ttsThreads`, `endpointing`, `acknowledgement`, `echoCancellation`, and `agentChat` are the only runtime tuning
options, plus `agents` (see [`docs/configuration.md`](docs/configuration.md#agents)),
for example `"agents": [{ "name": "Ada", "voice": "zoey" }, { "name": "Grace", "voice": "easton" }]`.
Say "Ada, …" or "…, Grace" to address one; unnamed speech goes to the first agent.
`agentChat` (`on` default, or `off`) lets agents hand the floor to each other.

Try it: with the two agents above, say "Ada, ask Grace what she thinks of the plan."
Ada answers "Grace, what do you think of the plan?" in her voice, then Grace replies
in hers; say anything to cut in and the chat stops. To move a followed session, say
"Grace, take over the build session". Set `"agentChat": "off"` if you want every
agent to answer only you.
OpenCode owns provider credentials; they are never passed to the Rust
process. Device preferences stay in host storage.

Each conversation keeps its original server and workspace target. Changing tabs
does not redirect a running conversation. Stop and start to select another target.
Use a consistent workspace path: OpenCode treats symlink and other path spellings
as separate locations. Delegation can reach known saved projects and worktrees
across the selected server, not only the active project. Workers use the hidden
`converse-worker` agent unless you name another visible agent, and keep normal
OpenCode permission rules.

## Verify

```sh
npm run check
npm test
npm run test:integration
npm run engine:test
npm run engine:fmt
```

The dependency-light Rust controller and protocol tests run without models or
audio devices. Those tests do not establish native audio or remote Desktop
support. See [release gates](docs/release-gates.md) for the remaining acceptance
matrix.

## License

Project source is MIT licensed; retained notices are in [LICENSE](LICENSE).
Native libraries and model assets have separate terms. See
[THIRD_PARTY.md](THIRD_PARTY.md). No native binary release is cleared for
redistribution merely because its source builds.
