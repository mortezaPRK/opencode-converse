# Third-party code and speech assets

The project license applies to project source, not to third-party libraries or
model weights. Preserve copyright and permission notices when reusing source.

The Rust speech stack uses whisper-rs/whisper.cpp/GGML, ONNX Runtime,
sherpa-rs/sherpa-onnx, and platform audio libraries. Native dependency bundles can
include eSpeak NG code and data with GPL-family licensing. The top-level license
of a wrapper does not establish the terms of every bundled component.

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

| Asset           | License evidence required                             |
| --------------- | ----------------------------------------------------- |
| Silero VAD      | Exact model revision, upstream license and notices    |
| Smart Turn      | Exact checkpoint, upstream license and notices        |
| Whisper English | Exact weights, upstream license and notices           |
| Piper Lessac    | Exact voice model card, dataset terms and attribution |

No model weights are included in the npm package. Do not publish native release
bundles or advertise a complete third-party license audit until exact-artifact
review is complete.
