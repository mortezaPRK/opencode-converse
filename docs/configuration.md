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

`npm run engine:stage` copies the release executable and its sherpa/ONNX Runtime
libraries into `assets/bin/<platform>-<arch>/`. No engine or model download
happens at runtime.

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
inventory roles. There are no per-model path options.

Install the pinned defaults with the engine installed in the state folder:

```sh
<state>/bin/<package-version>/opencode-converse models install
<state>/bin/<package-version>/opencode-converse doctor
```

## Inference tuning

`asrThreads` and `ttsThreads` are bounded to 1–256. `useGpu` defaults to true on
macOS and false elsewhere.

## Host preferences

Microphone and speaker device selections are local host preferences stored by
the TUI/Desktop host. They are never read from server or project configuration.
