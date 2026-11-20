# Configuration

Converse is configured through plugin options. The JSON Schema is
[`options.schema.json`](options.schema.json).

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "plugins": [
    {
      "package": "/absolute/path/to/opencode-converse",
      "options": {
        "supervisorModel": "provider/supervisor-model",
        "workerModel": "provider/worker-model",
        "asrThreads": 4,
        "ttsThreads": 4,
      },
    },
  ],
}
```

## Fixed state folder

The engine, speech models, downloads, and engine lock live in one fixed app state
folder. They are not configurable through plugin options or environment
variables.

| Platform | State folder                                                                         |
| -------- | ------------------------------------------------------------------------------------ |
| macOS    | `~/Library/Application Support/opencode-converse`                                    |
| Linux    | `$XDG_DATA_HOME/opencode-converse`, or `~/.local/share/opencode-converse` when unset |
| Windows  | `%LOCALAPPDATA%\opencode-converse`                                                   |

Layout:

```text
<state>/
  bin/<package-version>/opencode-converse[.exe]
  models/
  downloads/
  engine.lock
```

On first use the plugin downloads this version's engine from its GitHub release
(`https://github.com/mortezaPRK/opencode-converse/releases/download/v<version>/opencode-converse-<version>-<platform>-<arch>[.exe]`)
into `<state>/bin/<package-version>/`, together with the native third-party
notices (`THIRD_PARTY.md`). The size and SHA-256 are pinned in the package's
`assets/engines.json`; a mismatch, oversize, or interrupted download installs
nothing. Concurrent starts share one install through a lock file and an atomic
rename. The complete marker makes later starts reuse it with no network access. Speech models stay in `<state>/models`, so they are reused when
the package or engine version changes.

Published targets are `darwin-arm64`, `linux-x64`, and `win32-x64`. On another
platform the plugin reports that no engine is published. Developers can build and
install one locally, which replaces the download for that version:

```sh
npm run build
npm run engine:build
npm run engine:stage
```

`npm run engine:stage` installs the self-contained release executable (ONNX
Runtime is linked statically; Phonon is pure Rust) into `<state>/bin/<version>/`.
Speech models are installed automatically on first start; see below.

## Models

`supervisorModel` and `workerModel` are required. There is no implicit fallback to
the composer selection or to an OpenCode-provided agent.

Each model accepts either a reference string:

```jsonc
"supervisorModel": "provider/supervisor-model"
"workerModel": "provider/worker-model#fast"
```

or the object form:

```jsonc
{
  "providerID": "provider",
  "id": "worker-model",
  "variant": "fast",
}
```

The supervisor session is created with the hidden `converse` agent. A session the
voice agent creates uses the hidden `converse-worker` agent unless the user asks
for another visible primary agent (for example `plan`), validated against the
server's agent list; this is a new session, and the agent of an existing session
is never changed (OpenCode's prompt API has no per-prompt agent). Both hidden
agents are absent from the normal agent picker and lists. The voice agent never
owns other sessions: followed sessions (created or attached via `/converse-attach`
or `converse_attach_session`) are tracked only in the plugin's storage, at most 16
per conversation and one conversation per session, and are never marked or
modified. `/converse-detach` or `converse_detach_session` stops following one
without touching its work; stopping the conversation detaches all. Sessions follow
normal OpenCode permission behavior: the user approves or rejects requests on
screen; they are not announced by voice. Questions from followed sessions are announced,
and the user answers them on screen.

## Agents

`agents` defines the conversation's named voice agents. It is optional: without it
there is one agent in the engine's default voice, exactly as before. The roster is
read when the conversation starts and is fixed for its lifetime; to change it,
edit the option and start a new conversation.

```jsonc
"agents": [
  { "name": "Ada", "voice": "zoey", "agent": "plan", "prompt": "You review designs." },
  { "name": "Grace", "voice": "easton", "model": "provider/other-model" }
]
```

| Field    | Rules                                                                                                                                                                            |
| -------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `name`   | Required. 2 to 16 ASCII letters, unique ignoring case. Reserved words (stop, mute, pause, resume, start, attach, detach, user, assistant, everyone, all, system) are rejected.   |
| `voice`  | One of `zoey`, `maeve`, `easton`, `preston`, unique per agent. Omitted: the first unused voice in that order.                                                                    |
| `agent`  | OpenCode primary agent this agent's own session runs as. Checked against the server's visible primary agents when the conversation starts. Omitted: the hidden `converse` agent. |
| `model`  | Model for this agent's session. Omitted: `supervisorModel`.                                                                                                                      |
| `prompt` | Extra instructions for this agent only, at most 1000 characters.                                                                                                                 |

Up to four agents. Each agent has its **own OpenCode session**, created lazily the
first time it is addressed (the first agent's is created at start), with its agent
and model fixed at creation: OpenCode's prompt API has no per-prompt agent, so
Converse never switches an existing session's agent. An agent follows other
sessions only through plugin storage, with its own follow list, and never owns or
changes them. Setting `agent` to a custom agent means that agent's own permissions
apply to the voice session; the Converse tools and instructions are added to it
because Converse owns the session, never to any other session.

**Addressing.** Say a name first ("Ada, check the build") or last after a comma
("check the build, Ada"). The engine matches the name tolerantly (case, punctuation,
and small spelling variants such as "Gracie"; names under four letters must match
exactly), removes it from the prompt, and sends the prompt to that agent's session.
A name in the middle of a sentence ("ask Grace to check it") does not address
anyone. When you name nobody, **the first configured agent answers**. The
acknowledgement cue and the reply are spoken in the addressee's voice.

**What each agent sees.** Its own session, and the prompts addressed to it, plus a
short bounded digest of the other voices: at most the last 6 lines from other
agents (and your lines addressed to them), each clipped to 160 characters, in at
most 1.5 KB, marked as context rather than instructions. Agents never see the whole
conversation.

**One voice at a time.** The engine keeps a single floor. If another agent's reply
arrives while one is speaking, it is queued (at most 8 items and 64 KiB) and spoken
afterwards in its own voice; speaking over them cancels the active and the queued
speech.

### Agents talking to each other

With two or more agents and `agentChat` `"on"` (the default; `"off"` disables it), an
agent whose completed reply starts with another agent's name and a comma ("Grace, can
you check that?"), or ends with ", Grace", hands the floor over. The reply is spoken
in the speaker's voice and delivered to the addressee's own session as
`[Ada → Grace]: can you check that?`, followed by the same bounded digest. The
addressee's reply is then spoken in its voice, and may hand over again.

The engine enforces the budget, never the model. Per user turn:

- at most **4 agent turns** in a row (the one answering you counts as the first);
- at most **90 seconds** from when you finished speaking;
- no more than **2 round trips** between the same two agents.

When a hand-off is refused for one of these reasons, the transcript shows a short
"Agent chat ended" note and control returns to you. A reply that names nobody, names
itself, or names someone who is not on the roster ends the chain silently. Your
speech at any moment cancels what is being spoken or queued and ends the chain;
muting does too. Every new thing you say starts a fresh budget. Worker announcements
never start a chain. Each hand-off is a full model turn, so a chain costs up to four
turns; set `agentChat: "off"` to keep every agent answering only you.

### Followed sessions: one prompter at a time

Only one agent prompts a given followed session: the agent that attached or started
it. The others get a clear refusal that names the holder when they try to prompt,
interrupt or attach it, and cannot take it from each other. Only **you** can
reassign it: say "Grace, take over the build session". Grace's `converse_take_over_session`
tool then works only if that turn is your own speech, as reported by your local
client (not a relayed agent line, and not a worker update). Worker announcements for
a followed session are spoken in the holder's voice. The lock is in memory for the
conversation, and the OpenCode server does not enforce it: another client can still
prompt the session.

## Speech model inventory

Speech model names, roles, URLs, sizes, and hashes live only in
[`assets/models.json`](../assets/models.json). The Rust runtime resolves the VAD,
turn, ASR, and TTS paths from the fixed `<state>/models` directory using the
inventory roles. There are no per-model path options. The TTS role is the Phonon
31M English checkpoint, a directory of individually pinned files (URL, SHA-256, and
byte count each) installed atomically under `<state>/models/tts-phonon-31m-en`.
Its weights, tokenizer, and default voice are Apache-2.0; see
[`THIRD_PARTY.md`](../THIRD_PARTY.md). The ASR role is Moonshine v2 Small Streaming
(English, MIT), also a pinned directory installed under
`<state>/models/asr-moonshine-small-streaming-en` and loaded by the engine's own
ONNX Runtime code.

The four shipped voices are Apache-2.0 Phonon voice embeddings from
`gradium/phonon-voices` (a pinned commit, individually hashed in `assets/models.json`)
installed under `<state>/models/tts-phonon-voices-en`. They are loaded by the same
local-files-only synthesizer as the default voice.

Starting a conversation checks the installed models through the engine
(`models status --json`, offline) and, when anything is missing, runs
`models install --json` as a child process with a sanitized environment (no
server credentials). Progress appears in the starting state ("Downloading speech
models 120/210 MB"). Stopping the conversation kills the installer; a stall of
two minutes without output or 45 minutes overall fails the start with the asset,
the reason, and the manual command. Installers in several windows queue on the
engine lock, and the later one finds everything installed.

Each download is verified against its pinned size and SHA-256 and installed
atomically; the downloaded file is then removed from `<state>/downloads/`, as are
stale files that belong to no current pinned asset (regular files directly inside
that folder only) and leftover staging folders from an interrupted install.

To install or check by hand (troubleshooting, offline preparation) with the engine
in the state folder:

```sh
<state>/bin/<package-version>/opencode-converse models status
<state>/bin/<package-version>/opencode-converse models install
<state>/bin/<package-version>/opencode-converse doctor
```

## Inference tuning

`asrThreads` and `ttsThreads` are bounded to 1–256. Speech recognition (Moonshine)
and the VAD/turn models run on the CPU; there is no GPU option. Phonon (`ptts`) chooses its own CPU threading, so
`ttsThreads` is currently accepted but has no effect.

`endpointing` controls how long the engine waits in silence before it treats your
turn as finished: `"responsive"` (default) checks after about 0.26 s of silence and
stops waiting at 1.9 s; `"patient"` checks after about 0.64 s and stops waiting at
3 s, which suits slower speech with longer pauses.

`acknowledgement` (`"auto"` default, or `"off"`) controls a short spoken cue while
the supervisor is still thinking. If no reply audio has started about 0.6 s after
your spoken turn is sent, the engine says one neutral phrase ("Okay.", "On it.",
"Let me check.", rotated). The phrases claim nothing about success, are
synthesized once when the engine starts (so they play with no synthesis delay),
and are spoken at most once per turn. They are skipped when the reply has already
begun, for empty or rejected transcripts, after you interrupt or cancel, while
output is paused or muted, and for worker announcements. If the reply starts while
the cue is playing, the cue finishes and the reply follows it. The cue is never
sent to the agent and is not added to the transcript. With `"off"` it is not
synthesized at all. There is no environment override.

Independently of this option, the first spoken chunk of each reply may end at the
first clause boundary (comma, semicolon, colon, or dash) once it has at least four
words, so speech can start before the first full sentence has arrived. Later chunks
keep whole sentences.

`echoCancellation` (`"on"` default, or `"off"`) subtracts the engine's own playback
from the microphone with WebRTC AEC3, so you can interrupt on speakers. The reference
is the exact audio the output device callback plays (including silence while paused or
after an interruption), so the filter stays adapted across interruptions. It adds up to
10 ms of capture latency and some CPU. Quality depends on your speakers, microphone
and room and has not been verified on real hardware; use `"off"` (and headphones) if it
misbehaves. There is no environment override.

## Barge-in

While the assistant speaks, the interrupt guard depends on `echoCancellation`. With it
`"on"`, speech must be clearly voiced (VAD probability above 0.7) for 4 consecutive
32 ms frames (about 128 ms). With `"off"`, the engine cannot tell your voice from its
own echo, so it requires 15 frames above 0.85 (about 480 ms). Neither guard is
configurable. With echo cancellation on, the 4 trigger frames plus 4 frames (about
128 ms) before them are sent to speech recognition; with it off, the 15-frame window
is. The audio that tripped the guard is sent
to speech recognition, so the start of your interruption is kept. On interruption the
engine reports `played_ms` and `generated_ms` (reply audio the speaker actually played
versus audio generated) and a bounded `heard` text (up to 4 KiB: fully played
sentences plus a word-aligned estimate of the one being played). The desktop
transcript uses these to mark the reply "Interrupted" and show what was heard; they
are never sent to the agent or the server. `heard` is an estimate proportional to
audio position and is omitted when unknown.

While spoken output is paused nothing is audible, so there is no echo and the normal
(non-strict) speech-start check applies instead of the playback guard. Interrupting
while paused discards the held speech; it is not resumed afterwards.

## Host preferences

Microphone and speaker device selections are local host preferences stored by
the TUI/Desktop host. They are never read from server or project configuration.
