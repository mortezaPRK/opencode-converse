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

On Windows the executable has an `.exe` suffix. Building needs Rust and a C/C++
toolchain, plus **meson and ninja** on `PATH` (for example `brew install meson ninja`
or `pip install meson`) and libclang (Xcode command line tools on macOS) for the
bundled WebRTC audio processing build (`webrtc-audio-processing` `bundled`, about 40 s).
Meson downloads the pinned abseil-cpp 20240722.0 source (and its wrapdb build patch)
from github.com during that build, so the first build needs network access; Linux/Windows
builds of this dependency have not been verified. Linux additionally needs ALSA development headers and
pkg-config. Speech recognition (Moonshine) and the VAD/turn models run on CPU through the
shared ONNX Runtime on every platform. macOS has
a minimum deployment target of 13.4 for the downloaded ONNX Runtime archive.

## Offline speech listening comparison

With the pinned models already installed, generate WAV samples without opening
microphone or playback devices:

```sh
cargo run --manifest-path native/Cargo.toml --locked -p opencode-converse --features native-audio --example speech_quality -- /absolute/path/to/new-listening-directory
```

The output directory must not already exist. Samples compare production
phrase-aware chunking, whole-input synthesis, and deliberately forced 24-word
cuts (a diagnostic control, not an exact historical baseline). Start with sample
02 for mid-sentence seams and sample 04 for clause pacing. Phonon sampling is
stochastic, so listen to multiple examples rather than interpreting a single
waveform difference as proof. Each line prints time to first audible sample
(TTFA, through the production streaming joiner) and the real-time factor
(synthesis time / audio time; lower is faster); a final line times how quickly
cancellation returns. These measure offline synthesis on this machine, not
playback-device latency. Use `--release`; debug builds are far slower.

The production chunker keeps sentence context up to 320 UTF-8 bytes or 40 words,
then prefers clause punctuation before falling back to a bounded word/token cut.
Common English titles and uppercase initials/acronyms do not force sentence
breaks. Ambiguous acronym-final sentences may stay together until another clear
boundary or finalization. This larger context can delay the first chunk of a
long, unpunctuated streamed reply; model and playback queue limits remain intact.

## Offline ASR comparison

`asr_quality` measures speech recognition on identical audio, with no devices. `synth`
renders fixed coding-vocabulary utterances through Phonon once (16 kHz mono f32 files plus
`refs.txt` in a new directory); `run` transcribes those files and prints per-utterance
text, word errors, latency, load time, and RSS, plus silence/low-noise rejection checks.
Phonon sampling is stochastic, so compare ASR builds on the same `synth` directory:

```sh
cargo run --release --manifest-path native/Cargo.toml --locked -p opencode-converse --features native-audio --example asr_quality -- synth /absolute/path/to/new-directory
cargo run --release --manifest-path native/Cargo.toml --locked -p opencode-converse --features native-audio --example asr_quality -- run /absolute/path/to/new-directory
```

References are the written text, so spoken forms such as "zero point three" or "dot ts"
count as errors for engines that normalize to "0.3" or ".ts". Use for relative comparison.

## Offline echo-cancellation measurement

`aec_quality` needs the installed models and macOS `say`; it opens no audio device. It
simulates an echo path (30-200 ms delay, reverb, loudspeaker clipping) around the real
AEC3, turn detector with its barge-in guard, and Moonshine, and prints ERLE, false
speech frames, near-end detection delay, and double-talk ASR. `--quick` runs one path,
`--hard` uses heavy clipping and a 450 ms tail. It is a simulation, not a device test.

```sh
cargo run --release --manifest-path native/Cargo.toml --locked -p opencode-converse --features native-audio --example aec_quality
```

## Native linking constraints

- Text-to-speech uses the pure-Rust `ptts` crate (Gradium Phonon 31M, CPU, q8
  weights) with only its `hf` tokenizer feature. It reads a local model folder and
  never downloads; the `hub` feature must stay disabled. No shared libraries are
  required beside the executable, so the engine is one file.
- The build helper prepares `target/<profile>/deps` (and
  `<target>/<triple>/<profile>/deps` when `CARGO_BUILD_TARGET` is set), resolving
  the artifact root through Cargo metadata, including `target-dir` configuration.
  For target-specific helper builds, set that variable explicitly, including when
  Cargo config already sets `build.target`.
- The ONNX model crates (VAD, turn, and Moonshine ASR) explicitly use ORT API 17. Changing API/optimization
  behavior can alter Smart Turn decisions. They statically link their ORT
  archive.
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
regular-file/directory-only archive extraction, and atomic per-asset commit. The
Phonon TTS asset pins every member file (URL, SHA-256, bytes) at one Hugging Face
commit and publishes the complete directory atomically without overwriting.
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
