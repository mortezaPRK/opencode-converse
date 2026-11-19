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

The packaged engine and its shared libraries are copied into
`<state>/bin/<package-version>/` on first use. The complete marker makes later
starts reuse it. Speech models stay in `<state>/models`, so they are reused when
the package or engine version changes.

The npm package must include `assets/bin/<platform>-<arch>/`. Maintainers stage it
after building the native runtime:

```sh
npm run engine:build
npm run engine:stage
```

`npm run engine:stage` copies the self-contained release executable (ONNX Runtime
is linked statically; Phonon is pure Rust) into `assets/bin/<platform>-<arch>/`.
No engine or model download happens at runtime.

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

The supervisor session is created with the hidden `converse` agent. Every worker
session is created with the hidden `converse-worker` agent. Both agents are
hidden from the normal agent picker and lists. Workers keep normal OpenCode
permission behavior: the user approves or rejects requests on screen.

## Speech model inventory

Speech model names, roles, URLs, sizes, and hashes live only in
[`assets/models.json`](../assets/models.json). The Rust runtime resolves the VAD,
turn, ASR, and TTS paths from the fixed `<state>/models` directory using the
inventory roles. There are no per-model path options. The TTS role is the Phonon
31M English checkpoint, a directory of individually pinned files (URL, SHA-256, and
byte count each) installed atomically under `<state>/models/tts-phonon-31m-en`.
Its weights, tokenizer, and default voice are Apache-2.0; see
[`THIRD_PARTY.md`](../THIRD_PARTY.md).

Install the pinned defaults with the engine installed in the state folder:

```sh
<state>/bin/<package-version>/opencode-converse models install
<state>/bin/<package-version>/opencode-converse doctor
```

## Inference tuning

`asrThreads` and `ttsThreads` are bounded to 1–256. `useGpu` defaults to true on
macOS and false elsewhere. Phonon (`ptts`) chooses its own CPU threading, so
`ttsThreads` is currently accepted but has no effect.

`endpointing` controls how long the engine waits in silence before it treats your
turn as finished: `"responsive"` (default) checks after about 0.26 s of silence and
stops waiting at 1.9 s; `"patient"` checks after about 0.64 s and stops waiting at
3 s, which suits slower speech with longer pauses.

## Barge-in

While the assistant speaks, interrupting requires about 480 ms of clearly loud
speech. This guard is deliberately strict because acoustic echo cancellation is not
implemented yet, and it is not configurable. The audio that tripped the guard is sent
to speech recognition, so the start of your interruption is kept. On interruption the
engine reports `played_ms` and `generated_ms` (reply audio the speaker actually played
versus audio generated) and a bounded `heard` text (up to 4 KiB: fully played
sentences plus a word-aligned estimate of the one being played). The desktop
transcript uses these to mark the reply "Interrupted" and show what was heard; they
are never sent to the agent or the server. `heard` is an estimate proportional to
audio position and is omitted when unknown.

While spoken output is paused nothing is audible, so there is no echo and the normal
(non-strict) speech-start check applies instead of the 480 ms guard. Interrupting
while paused discards the held speech; it is not resumed afterwards.

## Host preferences

Microphone and speaker device selections are local host preferences stored by
the TUI/Desktop host. They are never read from server or project configuration.
