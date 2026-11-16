# Local Rust runtime

`opencode-converse` is the only executable. The default Cargo package build is
model-free and cannot silently open a test fixture. Real capture/inference uses
the `native-audio` feature. `run --test-fixture` and
`run --test-fixture-loading` are explicit, test-only stdio fixtures.
`doctor --test-only-inference` explicitly loads the real models and runs a
headless VAD/turn/TTS/resampling/ASR smoke test without opening audio devices.
It does not print the recognized text or claim microphone/playback validation.

```sh
cargo test --manifest-path native/Cargo.toml -p opencode-converse
node native/scripts/build.mjs
native/target/release/opencode-converse models install
native/target/release/opencode-converse doctor
```

On Windows the executable has an `.exe` suffix. Building needs Rust, CMake,
C/C++, and libclang. Linux additionally needs ALSA development headers and
pkg-config. macOS uses Whisper Metal; other targets use CPU Whisper. macOS has
a minimum deployment target of 13.4 for the downloaded ONNX Runtime archive.

## Native linking constraints

- `sherpa-rs-sys 0.6.8` downloads its exact sherpa-onnx distribution, including
  shared libraries. These libraries must stay beside the executable when it is
  installed; copying only the executable is not a complete native installation.
- The build helper prepares `target/<profile>/deps` because this dependency's
  library-copy code assumes it exists, which is not true with Cargo 1.100's
  artifact layout. Direct Cargo builds need equivalent directory preparation.
  It resolves the artifact root through Cargo metadata (including `target-dir`
  configuration) and also prepares `<target>/<triple>/<profile>/deps` when
  `CARGO_BUILD_TARGET` is set. For target-specific helper builds, set that variable
  explicitly, including when Cargo config already sets `build.target`.
- The final executable adds a relative library rpath on macOS and Linux.
- The ONNX model crates explicitly use ORT API 17. Changing API/optimization
  behavior can alter Smart Turn decisions. They statically link their ORT
  archive, while sherpa uses the shared runtime supplied by its distribution.
- The macOS static ORT archive's CoreML objects require Apple's compiler-rt
  builtins. The model crates locate this through the selected compiler's
  resource directory, not a hardcoded Xcode installation path.
  Both compiler-rt probes track `CC` and preserve a bounded local compiler
  invocation (program plus arguments, including wrappers and quoted paths).
  They append `-print-resource-dir` without invoking a shell or adding a parser
  dependency. The build-script tests compile standalone and mock macOS resource
  probes on every host; they do not claim device or cross-linker validation.

## Models and local state

All model locations are below the OS local data directory's
`opencode-converse/models`: `~/Library/Application Support` on macOS,
`%LOCALAPPDATA%` on Windows, and `$XDG_DATA_HOME` or `~/.local/share` on Linux.
No project config or stdio command can select paths. Debug builds alone accept
the absolute `OPENCODE_CONVERSE_DEV_DATA_DIR` local-development override.

`models install` is explicit network access. It uses the compiled
`assets/models.json`, exact byte counts and SHA-256, per-asset OS locks, bounded
regular-file/directory-only archive extraction, and atomic per-asset commit.
It does not replace differing existing models without user action. The verified
downloads are retained in the local `downloads` directory for repeat installs.
Startup never downloads models. `doctor` checks presence only, never opens a
device, and does not claim integrity or inference/device verification.

The engine holds an OS file lock for the process lifetime. Protocol/frame and
speech queues are bounded. EOF and shutdown do not wait for third-party model
constructors or inference to finish; process teardown releases devices and the
lock. Transcripts are protocol payloads only, never log messages.

Long model output is clipped for speech, never treated as a fatal conversation
error. Each turn or announcement speaks at most 48 chunks and one “The rest is
on screen” cue. Announcement storage and reply-ID history are also bounded.
Only a processed matching Hearing event can release a live capture-thread barge-in
fence; mute retires the old capture epoch. An older Drained event or announcement
cannot release the live fence. Native worker failures have an out-of-band atomic
health flag, independent of event-queue space.

Mute cancels playback before retiring the capture epoch, then drops the
independently owned microphone stream and its capture worker;
unmute opens a fresh capture stream without reloading models or output playback.
Tests verify the close/reopen ownership transitions, not physical OS indicator,
microphone, or playback behavior.

Capture delivers typed `CaptureEvent::Frame(&[f32])` and `CaptureEvent::Gap`
events from its worker. A raw-ring overrun publishes a gap without needing ring
space, discards old-generation raw frames, and resets both resampling and the
partial target-rate block before fresh PCM. Gap events may coalesce repeated
overruns; consumers invalidate incomplete hearing, not a transcript already
handed off. An empty PCM frame is never used as a discontinuity marker.

ASR runs on a separate bounded worker while capture and barge-in detection continue.
A capture sequence gap resets turn detection and abandons only hearing that has
not yet been handed to ASR; a complete submitted utterance can still produce its
transcript. A newer detected utterance supersedes an older pending ASR result:
only the transcript matching the controller's current hearing becomes a prompt.
Mute retires the capture epoch, so results from that epoch are discarded even
after unmute. If the bounded ASR queue is full, the new utterance is abandoned
without blocking capture or terminating the conversation.

The detector retains at most 120 seconds of an utterance (about 7.7 MB of 16 kHz
mono `f32` samples, plus one framing block). Ordinary requests end at the detected
turn boundary. Continuous speech or noise reaching this fixed limit is finalized
to keep capture memory and ASR work bounded; no partial-transcript accumulation is
performed. The minimum speech duration counts voiced frames after SpeechStart,
not the retained preroll.

Output producers must call `AudioIo::finish_output()` after an utterance's final
samples to recover a rate-conversion tail. Temporary streaming gaps do not flush
DSP state. Cancellation silences immediately; the callback's final queue drain
waits for the resampling worker's generation acknowledgement before new speech.
Counter resets precede generation publication. Worker transfers increment the
downstream count before a Release decrement upstream; the pending-frame read
uses Acquire loads in upstream-to-downstream order (completion markers, engine,
DSP, device) to prevent a torn drained result without callback locks or waits.

Model-free DSP checks (no capture/playback devices or model files):

```sh
cargo test --manifest-path native/Cargo.toml --locked -p ve-audio -p ve-turn -p ve-vad
node --test test/native-build.test.mjs
```

The log-mel tests compare all 80 × 800 features, with one and four threads, against
committed Transformers v4.46.3 NumPy fixtures. Explicit fixture regeneration uses
`crates/ve-turn/tests/generate_mel_reference.py` with NumPy 2.1.3 and network access
to hash-pinned source files; normal Rust tests are offline and model-free.
Python and NumPy are not required by engine builds or default CI tests. The
generator's source SHA-256 and NumPy-version checks are explicit exits and stay
active under `python -O` / `PYTHONOPTIMIZE`.

Optional development-only guard tests use stdlib Python, mock fetches and NumPy,
and never compile/execute fetched source or regenerate fixtures:

```sh
python3 native/crates/ve-turn/tests/test_generate_mel_reference.py
```

Root `npm test` includes the native build-helper and compiler-input regressions
on every platform.

See `THIRD_PARTY.md` before redistributing any native library or model.
