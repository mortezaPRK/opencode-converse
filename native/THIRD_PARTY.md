# Native dependency and model notices

The engine's Cargo manifests declare MIT licensing. Audio/inference source was
selectively derived from the MIT-declared Rust engine originally imported from
https://github.com/mortezaPRK/live-speech at
`c0cf67d22626d7cd4492c6930b5536973820093a`. The private repository and that
revision were verified through authenticated GitHub API access; an anonymous
404 is not evidence that the revision is nonexistent. The revision's
`voice-engine/Cargo.toml` declares `license = "MIT"` for the workspace, but its
recursive tree contains no LICENSE/COPYING file. Manifest declarations and
revision existence are provenance evidence, **not a completed license audit**.
Verification of the actual upstream license grant and required copyright/notices
remains a redistribution gate. No private source is made public by this notice.
This is not a runtime dependency. The project license does not relicense
third-party code or models downloaded by the user.

## Log-mel frontend and reference fixtures

`crates/ve-turn/src/mel.rs` is a modified Rust port of the Whisper normalization,
STFT/log-mel, and Slaney filterbank math described in Hugging Face Transformers.
The reference checked for this review is **v4.46.3**, not a claim about the exact
version originally used by the private upstream:

- [Whisper feature extractor](https://github.com/huggingface/transformers/blob/v4.46.3/src/transformers/models/whisper/feature_extraction_whisper.py):
  Copyright 2022 The HuggingFace Inc. team; Apache License 2.0.
- [Audio utilities](https://github.com/huggingface/transformers/blob/v4.46.3/src/transformers/audio_utils.py):
  Copyright 2023 The HuggingFace Inc. team and the librosa & torchaudio authors;
  Apache License 2.0. The file attributes filterbank adaptations to torchaudio
  and librosa; these attributions must not be dropped.
- [Transformers license](https://github.com/huggingface/transformers/blob/v4.46.3/LICENSE):
  Apache License 2.0, **not MIT**. Source-port license/notice obligations must be
  reviewed and retained separately from the project's MIT declaration before
  distributing the native sources or binaries; this notice is not clearance.

`tests/data/mel_filters.f32` came from the private upstream revision above. Its
values match the v4.46.3 Slaney filterbank after f32 conversion. The new
`logmel_{nonzero,silence,left_padded}.f32` fixtures are generated from synthetic
integer-defined waveforms, not recordings, model outputs, or downloaded models.
`tests/generate_mel_reference.py` invokes unchanged hash-pinned upstream NumPy
methods and documents regeneration. Fixtures use little-endian f32, mel-major
80 × 800 layout; SHA-256 values are:

- nonzero: `86b2be68412b4ad179a142192889c3d2af931aae674915f7483685ed8f29b2c1`
- silence: `c4a1d90bbbdc1c49cf89b8f73360019b1b0fe01ae48a9dfd6fc2c4a3228b8859`
- left-padded: `09fa7d997d5f72fdfe8c4827acb4d75580fa150bc6c3ac26cd9977f077356ccb`

## Dependencies and models

The source setup uses these components, among others:

- **Moonshine v2 Small Streaming runtime**: implemented in `crates/ve-asr` directly
  against ONNX Runtime following the inference procedure in the MIT-licensed
  `moonshine-ai/moonshine` C++ sources (`core/moonshine-streaming-model.cpp`,
  `core/bin-tokenizer`); the greedy-decode length heuristic (6.5 tokens/s) and the
  `tokenizer.bin` layout come from there. No Moonshine source is copied or linked.
  Copyright (c) 2025 Useful Sensors, Inc. (dba Moonshine AI), MIT.
- ONNX Runtime: MIT.
- **webrtc-audio-processing 2.1.0** (`webrtc-audio-processing-sys` + wrapper +
  `webrtc-audio-processing-config`, tonarino; `license-file = COPYING`, BSD-3-Clause,
  identical to WebRTC's): echo cancellation (AEC3) and high-pass filter, statically
  linked via the `bundled` feature (about 29 MB static archive before linking). Its
  vendored sources and the notices to preserve in a binary distribution:
  - WebRTC (`webrtc/LICENSE`, BSD-3, Copyright (c) 2011 The WebRTC project authors)
    and the WebRTC Additional IP Rights Grant (Patents) in `webrtc/PATENTS`.
  - `third_party/pffft` (FFTPACK license, UCAR; Julien Pommier), `third_party/rnnoise`
    (BSD-3, Mozilla, Xiph.Org, Jean-Marc Valin, Mark Borgerding), `modules/third_party/fft`
    (Mark Olesen, Queen's Univ. at Kingston), `common_audio/third_party/ooura` (Takuya
    Ooura, permissive), `common_audio/third_party/spl_sqrt_floor` (Wilco Dijkstra).
  - abseil-cpp 20240722.0 (Apache-2.0), downloaded by meson at build time
    (tarball SHA-256 `f50e5ac311a81382da7fa75b97310e4b9006474f9560ac46f54a9967f07d4ae3`,
    plus the meson wrapdb build patch). Apache-2.0 notices must be shipped.
    Build tools only: meson, ninja, bindgen/libclang, autotools (crate). This review
    is by license-file inspection of the 2.1.0 crate sources, **not** an exact-artifact
    audit of a shipped binary; that remains a redistribution gate.
- **ptts 0.7.1** (Gradium Phonon runtime, `github.com/gradium-ai/xn-ptts`):
  **MIT OR Apache-2.0** per its crates.io manifest. Built with only the `hf`
  (tokenizer) feature; the `hub` network-download feature is never enabled. Its
  resolved dependencies (for example `xn`, `tokenizers`, `safetensors`) are
  inspected from Cargo.lock like any other crate.
- Rust dependencies: inspect each crate's license in Cargo.lock's resolved source.

Models are downloaded separately, never included in the npm source package:

- Silero VAD: upstream MIT notices.
- Smart Turn v3: upstream BSD-2-Clause notices.
- Moonshine v2 Small Streaming English (`quantized_26_11_02` `.ort` files from
  `download.moonshine.ai`, hashes pinned in `assets/models.json`): **MIT** per the
  upstream repository `LICENSE` (pinned `d8d8fa29d05ca67441a7719e70099f9f48917a93`,
  installed beside the weights) and the `moonshine-ai/moonshine-streaming-small`
  model card. Non-English legacy Moonshine models are non-commercial and are not
  used. Confirm the grant for the exact converted artifacts before redistribution.
- Phonon 31M English (`gradium/phonon-31m-en`, pinned commit in
  `assets/models.json`): weights, tokenizer, and the default voice are
  **Apache-2.0** per the model card and the `LICENSE` file installed with them.
  The model builds on Pocket TTS and the Mimi codec from Kyutai: retain Kyutai
  attribution and verify the upstream terms of those components for the exact
  artifact. Phonon 40M checkpoints are CC BY-NC 4.0 and are not used.

Before distributing native prebuilts or models, collect the corresponding
copyright/license notices and review redistribution obligations, especially
the exact shipped artifacts. This change provides explicit source builds, not prebuilt
native redistribution, and does not claim all dependencies are MIT. There is no
blanket clearance; exact-artifact review remains required.
