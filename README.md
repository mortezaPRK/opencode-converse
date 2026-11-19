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
- The plugin owns both agents it uses: a hidden `converse` supervisor and a
  hidden `converse-worker` coding worker. Neither is selectable in normal agent
  lists, and workers never fall back to an OpenCode-provided agent.
- English speech is the initial scope. Permission requests are announced, but
  approvals happen in OpenCode's normal interface.
- The microphone starts only on an explicit command. Stopping a conversation
  releases its local runtime. Use headphones while echo behavior is being tested.

## Develop

Use Node.js 26 and Rust. Native inference builds additionally require CMake, a
C/C++ compiler, and libclang. Linux builds need ALSA development headers and
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
waits in silence before ending your turn; see [configuration](docs/configuration.md).

The transcript (latest 50 entries) is kept by the voice bridge, not read from the
host's session cache, so it shows your spoken prompts and the supervisor's
completed replies even while you are viewing another session.

Ask the supervisor to delegate a task, inspect one of its workers, steer it, or
stop it. Say "start a plan session about X" to pick any visible OpenCode agent
(`converse_list_agents`); without one, the hidden `converse-worker` is used.
To work in an existing session, run `/converse-attach` while viewing it, or ask
the supervisor to attach one from `converse_list_sessions`. Only sessions at the
conversation's own location can be attached, never supervisor sessions or
sessions held by another active conversation; attaching sends nothing, and
agent changes apply only when you ask for them. Attached sessions are prompted,
followed, and announced like any worker, and permission requests stay on screen. It reports worker updates while you are listening. Worker permission
requests must be decided on screen. If a server connection drops, start a new
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
versions. `asrThreads`, `ttsThreads`, and `endpointing` are the only runtime tuning
options. OpenCode owns provider credentials; they are never passed to the Rust
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
