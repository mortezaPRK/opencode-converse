# Third-party code and speech assets

The project license applies to project source, not to third-party libraries or
model weights. Preserve copyright and permission notices when reusing source.

The Rust speech stack uses ONNX Runtime (Silero VAD, Smart Turn, Moonshine), the `ptts`
Phonon runtime, and platform audio libraries. The top-level license of a wrapper
does not establish the terms of every bundled component.

Text-to-speech is **Gradium Phonon 31M** (`gradium/phonon-31m-en`). The `ptts`
runtime crate (0.7.1) is **MIT OR Apache-2.0**. The model's weights, tokenizer, and
default voice are published by Gradium under **Apache-2.0** (the Hugging Face model
card and the repository `LICENSE`, which is installed beside the weights). Phonon
builds on Pocket TTS and the Mimi codec developed by Kyutai; credit them and review
their upstream terms for the exact artifact before redistribution. The larger
Phonon 40M models are CC BY-NC 4.0 and must not be installed. Phonon performs no
phoneme conversion in its runtime, and the pinned asset set contains no eSpeak NG data; confirm this against the exact shipped artifacts.

Four extra English voices come from **`gradium/phonon-voices`** (pinned commit
`0ee17205f254b5abc43b266c8a0f215565c352f7`): Zoey, Maeve, Easton and Preston, each a
256,136-byte `.safetensors` speaker embedding with its own SHA-256 in
`assets/models.json`, installed locally and never fetched at run time. The
repository card declares **Apache-2.0** (`license: apache-2.0`); it has no separate
`LICENSE` file, so the Apache-2.0 text from `gradium/phonon-31m-en` is installed
beside them together with the card (`README.md`). Credit: voices by Gradium. The
voices are embeddings for Phonon S (31M), the model used here. Exact-artifact review
is still required before redistribution (see `docs/release-gates.md`).

Speech recognition is **Moonshine v2 Small Streaming** (123M, English), run by this
project's own `ve-asr` crate on the shared ONNX Runtime; the official Moonshine C++
library is not used. The upstream repository (`moonshine-ai/moonshine`, pinned
`d8d8fa29d05ca67441a7719e70099f9f48917a93`) states that its code and all English and
streaming speech-to-text models are **MIT** (Copyright (c) 2025 Useful Sensors, Inc.
dba Moonshine AI); only legacy non-English non-streaming models use the
non-commercial Moonshine Community License, and none of those is used. That
`LICENSE` is installed beside the weights. The Hugging Face card for
`moonshine-ai/moonshine-streaming-small` also declares MIT. The pinned runtime files are
quantized ONNX Runtime (`.ort`) conversions from Moonshine's model CDN
(`quantized_26_11_02`); they carry no per-file license metadata, so the MIT grant for
those exact converted artifacts must be confirmed in the exact-artifact review.
Whisper is no longer shipped: the ASR crate, whisper.cpp, GGML, and the Whisper
weights are removed. The Smart Turn feature frontend still uses a Rust port of the
Hugging Face Transformers Whisper log-mel math (Apache-2.0; see
`native/THIRD_PARTY.md`).
This inventory is not a completed exact-artifact license review.

Echo cancellation is **WebRTC AEC3** from `webrtc-audio-processing` 2.1.0 (tonarino Rust
wrapper and the PulseAudio repackaging of Google's WebRTC audio processing, built from
the bundled sources). The wrapper and WebRTC code are **BSD-3-Clause** (Copyright
(c) 2011 The WebRTC project authors / Google Inc.) with Google's WebRTC patent grant
(`PATENTS`, which terminates for a licensee who sues over WebRTC). It also contains
FFTPACK (UCAR), Ooura FFT, Mark Olesen FFT, Wilco Dijkstra `spl_sqrt_floor`, and
rnnoise-derived (Mozilla/Xiph/Jean-Marc Valin, BSD-3) sources, and builds against
abseil-cpp (Apache-2.0) fetched by meson. See `native/THIRD_PARTY.md`. Binary
redistribution must carry these notices; the exact-artifact review is still required.

Engines are published on GitHub releases (built in CI from this source, tested only on
Apple silicon). Each carries these notices through `native/THIRD_PARTY.md`. A review
of the exact shipped libraries, their resolved source revisions, and any
source-distribution obligations has not been completed; do not read a published
engine as a license audit.

Speech assets need separate verification:

| Asset           | License evidence required                                                                                        |
| --------------- | ---------------------------------------------------------------------------------------------------------------- |
| Silero VAD      | Exact model revision, upstream license and notices                                                               |
| Smart Turn      | Exact checkpoint, upstream license and notices                                                                   |
| Moonshine Small | Exact `.ort` files and tokenizer (hash-pinned), MIT notice from the pinned upstream `LICENSE`                    |
| Phonon 31M      | Exact weights/tokenizer/voice commit, Apache-2.0 notice, Kyutai (Pocket TTS/Mimi) attribution and upstream terms |

No model weights are included in the npm package or in a release. Users download
them from their upstream sources under those assets' own licenses. The package
contains no engine binary; the engine is downloaded from the GitHub release. Do not
advertise a complete third-party license audit until the review is complete.
