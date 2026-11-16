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

The engine executable is built under `native/target/release/` (`.exe` on Windows).
Model installation and diagnostics are explicit local operations:

```sh
native/target/release/opencode-converse models install
native/target/release/opencode-converse doctor
```

Model URLs, sizes, and SHA-256 digests are recorded in
[`assets/models.json`](assets/models.json). No weights are shipped in the npm
package. Review the asset terms before downloading or redistributing them.

Set `CONVERSE_BIN` in the environment of the local OpenCode process to the
absolute engine executable path for development. Do not put an executable path
in OpenCode plugin options. For Desktop, the environment must be available to the
Desktop app itself, not just a terminal that is already running.

Add the **package root** to your global `opencode.jsonc`:

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "plugins": ["/absolute/path/to/opencode-converse"],
}
```

The server, terminal, and Desktop entries are one plugin package. For a remote
connection, configure the plugin on the remote server too. Desktop loads its UI
entry from its own local server; its voice commands target the server selected
when the conversation starts. Rebuild, then use `/reload` to reload development
changes.

## Use

Open the desired workspace/server, then choose **Converse: start conversation**
from the command palette or run `/converse-start`.

| Command            | Action                                                |
| ------------------ | ----------------------------------------------------- |
| `/converse-start`  | Start a conversation on the selected server/workspace |
| `/converse-stop`   | Stop and release the local runtime                    |
| `/converse-mute`   | Mute or unmute microphone capture                     |
| `/converse-open`   | Open the dedicated supervisor session                 |
| `/converse-status` | Show conversation status                              |

Ask the supervisor to delegate a task, inspect one of its workers, steer it, or
stop it. It reports worker updates while you are listening. Worker permission
requests must be decided on screen. If a server connection drops, start a new
conversation explicitly; user prompts are not automatically replayed.
Long replies are shortened for speech; the full response remains available in
the supervisor session.

## Configuration

The default experience needs no plugin options. The supervisor uses the selected
OpenCode model unless a server-side override is set. OpenCode owns provider
credentials; they are never passed to the Rust process.

Server options are limited to `supervisorModel`, `workerModel`, and `workerAgent`.
Model overrides use `{ "providerID": "…", "id": "…", "variant": "…" }`, with
`variant` optional. Local device preferences belong to host storage, not remote
or project configuration.

Each conversation keeps its original server and workspace target. Changing tabs
does not redirect a running conversation. Stop and start to select another target.
Use a consistent workspace path: OpenCode treats symlink and other path spellings
as separate locations. Delegation can reach known saved projects and worktrees
across the selected server, not only the active project. Workers use the trusted
configured/default agent and its normal OpenCode permissions.

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
