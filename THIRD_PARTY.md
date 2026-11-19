# Third-party code and speech assets

The project license applies to project source, not to third-party libraries or
model weights. Preserve copyright and permission notices when reusing source.

The Rust speech stack uses whisper-rs/whisper.cpp/GGML, ONNX Runtime, the `ptts`
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

The resolved **whisper-rs 0.16.0** Rust binding and **whisper-rs-sys 0.15.0** FFI
binding declare **Unlicense**, not MIT. Their source archives identify revision
`7558e1b72f54f2f22a53589afb77e65681834c36`; the binding's
[Unlicense notice](https://codeberg.org/tazz4843/whisper-rs/src/commit/7558e1b72f54f2f22a53589afb77e65681834c36/LICENSE)
is separate from the **MIT** notice shipped for whisper.cpp / bundled GGML
source (Copyright (c) 2023-2024 The ggml authors). Exact-artifact review must
retain these distinct notices and reverify the versions actually shipped.
This inventory does not establish blanket native redistribution clearance.

Before distributing native bundles, inventory the exact shipped libraries and
their resolved source revisions, collect all required notices, and determine
source-distribution and other obligations for the resulting bundle. Source-only
CI artifacts are not evidence that a native bundle is cleared for redistribution.

Speech assets need separate verification:

| Asset           | License evidence required                                                                                        |
| --------------- | ---------------------------------------------------------------------------------------------------------------- |
| Silero VAD      | Exact model revision, upstream license and notices                                                               |
| Smart Turn      | Exact checkpoint, upstream license and notices                                                                   |
| Whisper English | Exact weights, upstream license and notices                                                                      |
| Phonon 31M      | Exact weights/tokenizer/voice commit, Apache-2.0 notice, Kyutai (Pocket TTS/Mimi) attribution and upstream terms |

No model weights are included in the npm package. The package may include a
prebuilt engine under `assets/bin/`; those are
third-party-bearing artifacts and require exact-artifact license review before
redistribution. Do not publish native release bundles or advertise a complete
third-party license audit until that review is complete.
