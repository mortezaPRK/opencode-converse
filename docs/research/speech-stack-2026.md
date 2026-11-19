# Speech and audio stack research — November 2026

Status: **research, partly implemented** as of 19 November 2026 (engine 0.7.0). The original document of
18 November 2026 was research only. Since then: Piper/sherpa-onnx was replaced by Gradium Phonon 31M (`ptts`); Whisper `base.en` was replaced by Moonshine v2 Small Streaming (§6.4, phase one);
played/generated sample accounting reports what was actually heard on interrupt; `endpointing: responsive|patient`
was added; spoken output can be paused; engine 0.4.0 adds a cached neutral acknowledgement (§5.4/§6, `acknowledgement: auto|off`) and an earlier first chunk (clause cut after ≥4 words); Silero VAD was already v6.2; engine 0.6.0 adds named voice agents with per-speaker Phonon voices and engine 0.7.0 bounded agent-to-agent chat (see `multi-agent-voice.md`); engine 0.5.0 added WebRTC AEC3 echo cancellation (§4.4/§6.3, `echoCancellation: on|off`) with a relaxed barge-in guard. Sections describing Piper are the
pre-change baseline. The document exists to make future changes cheap to justify and easy to
reject.

> **Implementation note (19 November 2026).** Recommendation 1 (§6.1) has been implemented:
> Piper/sherpa-onnx was replaced by Gradium Phonon 31M through `ptts =0.7.1` (CPU, q8, `hf`
> tokenizer feature only, no `hub`). The `assets/models.json` inventory gained a `files`
> format pinning each member (config, tokenizer, `model.q8.gguf`, `default-voice.safetensors`,
> `LICENSE`) at `gradium/phonon-31m-en@8f7a37c94a4c58b04c8287300c53f11d2e208eae`. §1, §3, and
> §6.1 below describe the earlier state and the pre-change rationale. First measurement
> (`speech_quality` example, release build, this Apple-silicon dev machine, CPU q8, one run,
> not a controlled benchmark): time to first audible sample 35–136 ms, real-time factor
> about 0.09–0.15 (roughly 7–11× real time), model load ~0.05 s, cancellation returns in
> ~20 ms. No playback device was involved.

Scope: the speech path of `opencode-converse` — microphone in, voice activity detection,
end-of-turn detection, speech-to-text, text-to-speech, playback, and barge-in — for a
local-first Rust runtime that supervises a coding agent.

Research date: **18 November 2026**. All prices are USD list prices observed on that date
and exclude tax and negotiated rates.

## How to read the evidence in this document

- **V** marks a vendor- or author-reported number. **I** marks an independent measurement.
- Benchmarks with different datasets, text normalisation, decoding settings, or model
  revisions are **not** directly comparable. Percentages are quoted with their source
  rather than merged into a single ranking.
- **unverified** means the claim could not be established from a primary source during
  this review. It is not a synonym for "false".
- "Bundle: yes" means the published license permits redistribution **subject to**
  preserving notices and reviewing the exact shipped artifact. It is not legal advice and
  it is not an exact-artifact clearance, which this project already requires elsewhere.
- No model was run, no device was benchmarked, and no latency was measured for this
  document. Everything about this machine's real performance remains an open question
  until `native/runtime/examples/speech_quality.rs` is extended.

---

## 1. Where the stack is today

_Pre-change baseline (Piper era, 18 November 2026); kept as written for history. Phonon replaced the TTS row on 19 November 2026._

Facts below were read from the repository, not from research.

| Stage        | Implementation                                                                                                                    | Notes and limits                                                                                                                                                                                  |
| ------------ | --------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Device I/O   | `cpal` 0.18.2, `rtrb` 0.4, `rubato` 5.0.1 (`crates/ve-audio`)                                                                     | Full duplex at the device's current rate, resampling off the audio callback, capture downmixed to mono then converted to 16 kHz. Barge-in flushes queued output in O(1) via a generation counter. |
| VAD          | Silero VAD ONNX through `ort =2.0.0-rc.15` (`crates/ve-vad`)                                                                      | Exactly 512 samples (32 ms) at 16 kHz, 64-sample context, recurrent state `(2,1,128)`. Model pinned by URL + SHA-256 from a `snakers4/silero-vad` commit.                                         |
| End of turn  | Pipecat Smart Turn v3.2 int8 CPU ONNX + a Rust port of the Whisper log-mel frontend (`crates/ve-turn`, `native/core/src/turn.rs`) | Start after 5 voiced frames (`p > 0.5`, ~160 ms); consult Smart Turn after 8 silent frames (`p < 0.35`, ~256 ms); hard flush at 60 frames (~1.9 s); decision threshold `p > 0.5`.                 |
| ASR          | `whisper-rs` 0.16.0 → whisper.cpp/GGML, `ggml-base.en.bin` (148 MB), Metal on macOS (`crates/ve-asr`)                             | **Batch, not streaming**: one `transcribe()` call per finalised utterance, greedy, `single_segment`, `no_context`, English only. Rejects output when `no_speech_probability > 0.6`.               |
| TTS          | Piper `en_US-lessac-medium` through the sherpa-onnx 1.12.9 C API (`crates/ve-tts`)                                                | CPU provider, `max_num_sentences = 1`, 22.05 kHz, cancellable progress callback, 20 s cap per chunk.                                                                                              |
| Text shaping | `runtime/src/speech.rs`                                                                                                           | Markdown/code stripped, chunks ≤ 320 bytes and ≤ 40 words, sentence then clause boundaries preferred.                                                                                             |
| Seams        | `crates/ve-tts/src/joiner.rs`                                                                                                     | Edge-silence trim, 130 ms clause gaps, 300 ms sentence gaps, 1 ms fades.                                                                                                                          |
| Barge-in     | `runtime/src/audio.rs`, `runtime/src/generation.rs`, `crates/ve-audio/src/io.rs`                                                  | While output is queued or generation is busy, SpeechStart requires `p > 0.85` for **15 consecutive frames (~480 ms)**. Then: invalidate generation, flush output, cancel TTS.                     |
| Models       | `assets/models.json`, installed by `runtime/src/models.rs`                                                                        | HTTPS-only, size + SHA-256 pinned, per-asset locks, bounded `tar.bz2` extraction, no startup downloads.                                                                                           |
| Echo / noise | **None**                                                                                                                          | No AEC, no noise suppression. `README.md` tells users to use headphones; `docs/release-gates.md` requires an echo/self-trigger test before speaker use.                                           |

### What that design buys, and what it costs

**Buys.** A small, auditable bundle: ~226 MB of speech assets, one ONNX Runtime, one
whisper build, deterministic install, and a barge-in path that cannot deadlock because the
audio callback only watches a counter.

**Costs, in the order they hurt the conversation:**

1. **No acoustic echo cancellation.** The loudspeaker is a second talker. Everything else
   in this list is downstream of that: the 480 ms strict barge-in guard exists precisely
   because the VAD cannot distinguish the agent's voice from the user's.
2. **Batch-only ASR.** No partial transcript, no early-arriving accuracy, and a fixed
   cost paid _after_ the turn ends. Whisper also pads to a 30 s window per call, so short
   utterances do not get cheaper with length.
3. **`base.en` plus `no_context`.** `single_segment` and `no_context` remove the very
   mechanism (prompt biasing) that helps Whisper with identifiers, paths, and package
   names — exactly the vocabulary this product is about.
4. **Sentence-level TTS.** Piper with `max_num_sentences = 1` over ≤ 320-byte chunks
   means the first audible sample waits for a whole synthesised chunk, and prosody cannot
   span a sentence.
5. **A GPL-family component in the shipped bundle.** Piper's frontend drags in eSpeak NG
   code and data; `THIRD_PARTY.md` already flags this as a redistribution question rather
   than a resolved one.

Section 6 turns these into a ranked plan.

---

## 2. Speech-to-text

### 2.1 Local / on-device candidates

| Candidate (verified current status)                                              | Size, languages                                                     | Accuracy evidence                                                                       | Latency / streaming                                                                                                                                | Code → weights                                                                                                    | Bundle                                                                           |
| -------------------------------------------------------------------------------- | ------------------------------------------------------------------- | --------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| **whisper.cpp + Whisper** (runtime v1.9.5, 6 Oct 2026) — ships today             | tiny 39M → large 1.55B; turbo 809M; multilingual and `.en` variants | Distil-v3.5 authors measure turbo at 7.30% short-form OOD / 10.25% long-form OOD. **V** | Not a native streaming architecture; overlapping windows give partials (published example refreshes every 500 ms)                                  | **MIT** runtime; OpenAI repo says MIT weights while the official HF `large-v3` card says **Apache-2.0**           | Yes, with artifact-specific notices                                              |
| **WhisperKit** (Argmax OSS v1.1.1, 8 Oct 2026)                                   | CoreML Whisper; turbo compressed 1.6 GB → 0.6 GB per paper          | d750 encoder: 2.25% LibriSpeech test-clean, 12.85% Earnings22 subset. **V**             | Paper headline 0.46 s latency with specialised encoder; not a guarantee for every OSS checkpoint. Buffered/incremental, not causal                 | **MIT** OSS code → checkpoint license                                                                             | Yes for cleared OSS assets                                                       |
| **FluidAudio** (v0.18.3, 7 Nov 2026)                                             | CoreML/ANE SDK: Parakeet TDT 0.6B, Parakeet EOU 120M, Nemotron 0.6B | Catalog reports Parakeet Ultra 2.13% LS-clean vs v3 2.27%. **V**                        | ~190× realtime batch on M4 Pro; TDT sliding windows are pseudo-streaming; EOU/Nemotron have dedicated streaming paths                              | **Apache-2.0** SDK → model-specific licenses, not all Apache                                                      | Yes (official Rust/Tauri wrapper exists)                                         |
| **Parakeet TDT 0.6B v2 / v3** (May / Aug 2025)                                   | 600M; v2 English, v3 25 European languages                          | Open ASR 8-set average 6.05% (v2) / 6.34% (v3); LS-clean 1.69% / 1.93%. **V**           | Full-attention batch; streaming adaptations are buffered compromises. CoreML, MLX, ONNX and GGUF paths exist without CUDA                          | Apache-2.0 NeMo and runtimes → **CC-BY-4.0 weights**                                                              | Yes, attribution required                                                        |
| **Moonshine v2** (paper 12 Feb 2026; library v0.1.6, 6 Nov 2026)                 | tiny 34M, small 123M, medium 245M; streaming models in 8 languages  | English 8-set average **12.00% / 7.84% / 6.65%**; original Base is 10.07% at 58M. **V** | **Native streaming encoder** with bounded lookahead; M3 post-utterance response **50 / 148 / 258 ms**, compute 8 / 18 / 29%. Not TTFT measurements | **MIT** code; all streaming STT and English weights **MIT**; legacy non-English offline weights are **Community** | Yes for MIT artifacts only                                                       |
| **Distil-Whisper** (distil-large-v3.5)                                           | 756M, English, ~1.51 GB FP16 payload                                | 7.08% short-form OOD / 11.39% long-form OOD vs turbo 7.30 / 10.25. **V**                | 1.46× turbo throughput; still not native streaming                                                                                                 | **MIT** code and weights                                                                                          | Yes                                                                              |
| **MLX implementations** (mlx-whisper 0.4.3, parakeet-mlx 0.7.0, MLX-Audio 0.6.6) | Wrappers, not new acoustic models                                   | No defensible single "MLX WER"; conversion fidelity needs per-model testing             | Apple GPU acceleration; "streaming" in MLX-Audio sometimes means text streaming, not live audio ingestion                                          | mlx-whisper / MLX-Audio **MIT**, parakeet-mlx **Apache-2.0** → original weight terms                              | Yes, weights unchanged                                                           |
| **Nemotron ASR streaming 0.6B** (Mar 2026; 3.5 multilingual Jun 2026)            | 600M; English or 40 locales                                         | 6.93% @1.12 s, 7.67% @160 ms, 8.43% @80 ms 8-set average. **V**                         | **True cache-aware streaming**, 80–1120 ms chunks; official NeMo-Speech.cpp v0.3.0 (Oct 2026) with CPU/Metal/GGUF                                  | Apache-2.0 code → **NVIDIA Open Model License** (en) or **OpenMDW-1.1** (3.5)                                     | Licensing review required; both permit commercial redistribution with conditions |
| **Kyutai STT 1B / 2.6B**                                                         | 1B en/fr, 2.6B en                                                   | Comparable WER **unverified**                                                           | Delayed-streams: 500 ms configured delay (1B, includes semantic VAD) or 2.5 s (2.6B). Official MLX/Swift/Rust                                      | Python MIT, Rust Apache, weights **CC-BY-4.0**                                                                    | Yes with attribution                                                             |
| **Voxtral Mini 4B Realtime** (Feb 2026)                                          | ~4B, 13 languages                                                   | Vendor average 8.72% @480 ms, 6.73% @2.4 s. **V**                                       | Native causal streaming, recommended 480 ms delay. Mac TTFT unverified                                                                             | Weights **Apache-2.0**                                                                                            | Yes                                                                              |
| **Qwen3-ASR** (Jan 2026)                                                         | 0.6B / 1.7B, 30 languages + 22 Chinese dialects                     | Vendor FLEURS-English 4.39% / 3.35%. **V**                                              | Unified streaming/offline; official streaming path documented via vLLM. Mac latency unverified                                                     | **Apache-2.0** weights                                                                                            | Yes                                                                              |
| **Apple SpeechAnalyzer / SpeechTranscriber** (macOS 26+)                         | Proprietary, system-managed assets                                  | Apple WER unverified                                                                    | On-device, live volatile + final results. Assets downloaded and updated by the OS, outside app control                                             | Proprietary; no weight redistribution                                                                             | App may call the API; cannot bundle the assets                                   |
| **IBM Granite Speech 5.0** (Oct 2026)                                            | 2B; 470M TurboCTC variant                                           | 4.0 reports 5.52% and 280× on a server. **V**                                           | Primarily offline speech-language models; MLX `stream=True` means decoder text after encoding, not live audio                                      | **Apache-2.0**                                                                                                    | Yes                                                                              |

### 2.2 Cloud APIs

Cloud weights are proprietary and unavailable for bundling in every case below; an SDK
license is not a redistribution license. Accuracy and latency figures are independent
Bluejay MISTT measurements unless marked, and its streaming latency is measured **after a
forced end of speech**, which is not the same as time-to-first-word.

| Provider / model                                                         | Accuracy and latency                                                                                 | Streaming                                                                           | Price                                                                          |
| ------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------- | ------------------------------------------------------------------------------ |
| **OpenAI** gpt-4o-transcribe / mini, GPT Transcribe, GPT Live Transcribe | 14.35% / 6.13% (4o, mini), 4.66% (Transcribe), 4.96% @609 ms (Live). AA-WER separately gives 4o 4.0% | GPT Live emits deltas while speech arrives; GPT Transcribe starts after turn commit | $0.36/h, $0.18/h, $0.27/h, $1.02/h (Live)                                      |
| **Deepgram** Nova-3, Flux (en / multilingual)                            | Nova-3 batch 4.67%, streaming 5.43% @103 ms; Flux en 6.58% @95 ms, multi 7.25% @98 ms                | WebSockets; Flux bundles turn detection with recognition                            | Nova-3 promo $0.288/h mono; Flux $0.39/h (verify account rate)                 |
| **AssemblyAI** Universal-Streaming → STT-3.7 (17 Nov 2026)               | Universal en 8.74%; 3.7-Agent vendor claim 4.46% WER, final transcript 86 ms after speech            | True streaming; 32 languages in 3.7; SLAM-1 deprecated                              | $0.15/h legacy; 3.7 Realtime $0.30/h; Agent $0.45/h; billed on connection time |
| **ElevenLabs** Scribe v2 / v3, v2 Realtime / v3 Live                     | v2 batch 3.60%; v2 RT 4.55% @139 ms; v3 Live 3.55% @85 ms                                            | Separate batch and native live APIs, 90+ languages                                  | $0.20/h batch, $0.22/h realtime                                                |
| **Groq** whisper-large-v3-turbo                                          | 6.88%, 194 ms batch E2E, 41.3× RTFx (independent)                                                    | **File API only**; application-side chunking; 10 s minimum billing per request      | $0.04/h                                                                        |

### 2.3 Redistribution gotchas

1. **CC-BY is not "no obligations".** Credit the creator, link the license, keep notices,
   and state that conversion or quantisation happened. Applies to Parakeet, Canary, Kyutai.
2. **Nemotron licenses differ per checkpoint.** English uses NVIDIA's Open Model License
   (agreement + attribution + guardrail terms); 3.5 multilingual uses OpenMDW-1.1.
   Neither is Apache.
3. **Converted-model metadata can be wrong.** Some Nemotron CoreML cards say Apache while
   the upstream English checkpoint does not. A converter cannot erase upstream terms.
4. **Moonshine's non-English offline weights are not MIT.** The Community license includes
   registration and a $1 M annual-revenue threshold. Streaming models and English are MIT.
5. **Whisper's license is internally inconsistent.** OpenAI's repository says MIT for
   weights; the official HF `large-v3` card says Apache-2.0. Both permit commercial
   redistribution; the shipped manifest must name the exact source.
6. **Nothing from a cloud provider is bundleable.** OpenAI, Deepgram, AssemblyAI, and
   ElevenLabs offer service access, not redistributable weights.

### 2.4 Shortlist

1. **Moonshine v2 small / medium** — best first challenger. MIT streaming weights, ONNX
   artifacts that fit the `ort` dependency this project already links, small footprint, and
   M3 measurements aimed at exactly this interaction.
2. **Parakeet TDT v2/v3 via CoreML/FluidAudio or native GGUF** — best fast final
   transcript, best English accuracy per parameter, several non-CUDA deployment paths.
3. **whisper.cpp + large-v3-turbo** — keep as the conservative portable baseline and as
   the multilingual escape hatch; accept windowed streaming.
4. **Nemotron streaming 0.6B** — architecturally the best match for continuous
   conversation, but blocked behind a custom-license review and unmeasured Mac latency.
5. **Voxtral Realtime / Qwen3-ASR** — multilingual expansion only.

Cloud, if ever opted in: **ElevenLabs Scribe v3 Live** and **Deepgram Flux** for a fresh
bake-off, **Groq turbo** for the cheapest push-to-talk finalisation.

---

## 3. Text-to-speech

For this product the metric is **time to first audible sample**, not studio quality.
Note the difference between _streaming bytes of a finished sentence_ and _generating audio
frame by frame within an unfinished utterance_. Most "streaming" TTS claims are the former.

### 3.1 Local candidates

| Candidate (verified status)                                                           | Size                                                   | Code / weights                                                                                 | Streaming and latency evidence                                                                                                                    | Rust path                                                           | Bundle                                                              |
| ------------------------------------------------------------------------------------- | ------------------------------------------------------ | ---------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------- | ------------------------------------------------------------------- |
| **Gradium Phonon 31M** (`phonon-31m-en`, runtime v0.7.1, Nov 2026)                    | 31M; `model.q8.gguf` **64.5 MB**; 24 kHz mono; English | Runtime **MIT OR Apache-2.0**; weights, tokenizer, voices **Apache-2.0**                       | **True framewise streaming** (80 ms flow-matching frames + Mimi decode). Vendor: **26× realtime, 5 ms TTFB on one M5 CPU core**; 41×/6 ms on four | `ptts` crate, `tts.stream()` yields PCM chunks; no PyTorch          | **Yes** — but the 40M variants are **CC-BY-NC**                     |
| **Kyutai Pocket TTS** (v4.0.1, Oct 2026)                                              | 100M; Oct English variants 18.2M / 6.9M / 1.9M         | Code **MIT**; weights **CC-BY-4.0**                                                            | Native streaming; ~200 ms first chunk, 6× realtime on M4 Air, 2 cores. Vendor.                                                                    | Official standalone Rust port, Candle, sherpa-onnx                  | Yes with attribution; voice assets separate                         |
| **Kokoro-82M** (weights v1.0, Jan 2025)                                               | 82M; ONNX ~300 MB fp32 / ~80 MB quantised              | **Apache-2.0** core and weights; `kokoro-onnx` MIT                                             | Sentence/phrase chunk synthesis, not framewise. **Already loadable by the sherpa-onnx build this repo vendors** (see §6.1)                        | `ort`, sherpa-onnx, `kokoro-tts` crate                              | Yes, audit phonemiser deps                                          |
| **Piper** (legacy 2023.11.14-2 archived; successor `piper1-gpl` v1.8.0) — ships today | lessac medium ONNX **63.5 MB**                         | Legacy engine **MIT**; successor engine **GPL-3.0-or-later**; **voice rights vary by dataset** | Segment-at-a-time PCM                                                                                                                             | `ort` / sherpa-onnx / C API                                         | **Conditional** — training-chain review, eSpeak NG is GPLv3         |
| **Apple AVSpeechSynthesizer**                                                         | OS-managed                                             | Proprietary framework and voices                                                               | Queued utterances, PCM buffer callback; no published TTFA                                                                                         | `tts` crate, `objc2`, or a small bridge                             | **No** redistribution of assets; macOS SLA §2F restricts output use |
| **MLX-Audio 0.6.6**                                                                   | Wrapper for many models                                | Runtime **MIT**; each model keeps its own license                                              | Model-specific streaming                                                                                                                          | Python/Swift; `mlx-rs` is a tensor binding, not a port of MLX-Audio | Per model                                                           |
| **Qwen3-TTS 12Hz** (Jan 2026)                                                         | 0.6B / 1.7B                                            | **Apache-2.0**                                                                                 | Streaming architecture; 97 ms claim is a server result, not a Mac one                                                                             | MLX port; native Rust needs a port                                  | Yes                                                                 |
| **Orpheus TTS 0.1**                                                                   | 3B + SNAC                                              | Code/metadata Apache-2.0; **Llama-3.2-3B ancestry**                                            | ~100–200 ms on its accelerated server path, not Mac CPU; README admits skipped frames in realtime mode                                            | llama.cpp + SNAC, not turnkey                                       | Conditional component review                                        |
| **Chatterbox** Multilingual V3 500M / Turbo 350M / Nano 110M                          | as listed                                              | Code and published cards **MIT**                                                               | Reference API returns a waveform; no established incremental PCM API. **Retired, unmaintained since Aug 2026**                                    | MLX ports; substantial native work                                  | Yes in principle                                                    |
| **Dia / Dia2, Sesame CSM-1B**                                                         | 1.6B / 1B–2B / 1B+codec                                | Apache-2.0 declarations; codecs separate                                                       | Dia is dialogue-oriented and weak under ~5 s clips; Dia2 streams but Apple latency unverified                                                     | MLX ports                                                           | Conditional                                                         |
| **Supertonic 3** (Apr 2026)                                                           | 99M, 31 languages                                      | Examples MIT; weights **OpenRAIL-M**                                                           | ONNX CPU synthesis; **archived**                                                                                                                  | `ort` + Rust example                                                | Conditional on OpenRAIL-M                                           |
| **NeuTTS Air**                                                                        | ~552M total, ~360M active                              | Air weights Apache-2.0, but **repo code is NeuTTS Open License** ($5 M revenue threshold)      | GGUF streaming; 111 LM tokens/s on M4 CPU excluding codec                                                                                         | Clean native implementation needed                                  | Careful, split licensing                                            |

Vendor quality data point worth keeping in perspective: Phonon's blind listening test
(468 raters, 8,290 votes, English) puts **Phonon 31M at Elo 2098 vs Kokoro 2043**, with a
"hard set" LLM-WER of 1.5% vs 3.5% on structured data such as numbers, dates, codes, and
units. **V**, and run by the model's author — but "hard set" is precisely this product's
text distribution.

### 3.2 Cloud

| Provider / model                                   | Streaming and latency                                                                                     | $/1M chars                                                          |
| -------------------------------------------------- | --------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------- |
| **OpenAI** `tts-1` / `tts-1-hd`, `gpt-4o-mini-tts` | HTTP chunked audio, full text per request; no published TTFA                                              | $15 / $30; mini is token-priced ($0.60/M text in + $12/M audio out) |
| **ElevenLabs** Flash v2.5, v3, v4 Turbo            | HTTP + WebSocket input streaming; docs say ~100 ms median excluding network                               | $40 (Flash/Turbo), $80 (v3/v4)                                      |
| **Cartesia** Sonic 3.6 (stable Aug 2026)           | WebSocket text continuations + audio streaming; "under 90 ms" marketing                                   | ~$37–50/M by plan                                                   |
| **Deepgram** Aura-2 / Flux TTS                     | Streams bytes; their own tutorial works out to 616 ms TTFB (277 ms net of TLS) — illustrative, not an SLA | $30 / $45                                                           |
| **Google** Chirp 3 HD / Neural2 / WaveNet          | Bidirectional streaming documented for Chirp 3 HD only                                                    | $30 / $16 / $4                                                      |
| **Azure** Neural / Neural HD                       | SDK exposes first-byte metrics; no universal guarantee                                                    | $15 / $22 (East US S1)                                              |
| **Amazon Polly** Neural / Generative               | `SynthesizeSpeech` returns a stream from supplied text; not token-input streaming                         | $16 / $30                                                           |

For a token-by-token agent reply, only the bidirectional-streaming providers avoid waiting
for the whole answer — and none of them keeps audio local, which for a coding assistant
means potentially speaking proprietary code. Cloud TTS should stay an explicit opt-in.

### 3.3 Licensing gotchas

1. **Do not bundle by family name.** Phonon 31M is Apache and Phonon 40M is CC-BY-NC;
   NeuTTS Air weights are Apache while its repository code is not; Kani older is Apache
   while Kani-TTS-2 is LFM1.0.
2. **Permissive inference code does not clear weights.** F5-TTS is MIT code with CC-BY-NC
   weights; XTTS-v2's CPML restricts both model _and outputs_ to non-commercial use.
3. **Phonemisation can introduce GPL.** eSpeak NG is GPLv3. This already affects the
   shipped Piper path and is the reason `THIRD_PARTY.md` has a section about it. Phonon
   performs no phoneme conversion, which is a licensing simplification, not just a
   quality choice.
4. **Voices are separate assets.** Kyutai's catalog mixes CC0, CC-BY, and non-commercial
   recordings; a usable acoustic model does not authorise redistributing every voice.
5. **Cloning consent is not conveyed by a model license.** Pocket, Dia, and CSM each state
   explicit-consent requirements; none of them grant permission to impersonate.

### 3.4 Shortlist

1. **Phonon 31M (`ptts`)** — smallest credible download, true framewise streaming,
   Apache weights, Rust-native runtime, and no phonemiser. Best fit for this project.
2. **Pocket TTS** — the same architectural family, MIT code, CC-BY weights, official Rust
   binary; a good second implementation and a fallback if Phonon's English-only scope bites.
3. **Kokoro-82M** — conservative fixed-voice baseline that is a _configuration change_ in
   the sherpa-onnx build already vendored here; no framewise streaming.
4. **Qwen3-TTS 0.6B** — multilingual upgrade experiment, real port work required.
5. **Apple AVSpeechSynthesizer** — optional OS backend for zero-download operation, kept
   out of the redistributable-asset story.

---

## 4. VAD, turn detection, barge-in, echo cancellation

**VAD is not echo cancellation.** The agent's own voice is speech; a perfect VAD will
detect it if it leaks into the microphone. Full-duplex barge-in needs an echo-controlled
microphone signal, not a better threshold.

### 4.1 Components

| Component                                 | License                                                             | Cost                                                            | Framing / latency                                                                                   | Rust path                                                                                         |
| ----------------------------------------- | ------------------------------------------------------------------- | --------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| **Silero VAD v5 / v6**                    | **MIT**                                                             | <1 ms per 32 ms chunk on one thread (upstream)                  | 512 samples @16 kHz (or 256 @8 kHz); state carried across frames                                    | Direct `ort` (as today); `silero-vad-rust` 6.2.2; `voice_activity_detector` (v5, older `ort` pin) |
| **WebRTC GMM VAD**                        | WebRTC **BSD-3**; `webrtc-vad` crate **MIT**                        | Tiny, no neural runtime                                         | 10/20/30 ms frames, 8–48 kHz, aggressiveness 0–3                                                    | `webrtc-vad` (0.4.0, last released 2019)                                                          |
| **TEN VAD**                               | **Not plain Apache-2.0** — extra terms forbid competing deployments | Vendor claims lower cost than Silero                            | 160/256-sample hops @16 kHz                                                                         | C ABI or ONNX                                                                                     |
| **Picovoice Cobra**                       | SDK repo Apache, engine under commercial terms; AccessKey required  | Lightweight per vendor                                          | Query frame length at runtime                                                                       | C FFI                                                                                             |
| **Apple Voice Processing I/O**            | Proprietary OS framework                                            | OS-managed                                                      | Device/graph dependent; `setVoiceProcessingEnabled` since macOS 10.15                               | `objc2-avf-audio`, or CoreAudio AU via `coreaudio-rs`                                             |
| **Apple SpeechDetector** (macOS 26+)      | Proprietary                                                         | —                                                               | **Only works with SpeechTranscriber/DictationTranscriber**; no general speech-boundary event stream | Swift shim; not a standalone barge-in VAD                                                         |
| **Pipecat Smart Turn v3.x** — ships today | **BSD-2-Clause**                                                    | 12.6 ms on c7a.2xlarge including preprocessing; ~8 MB quantised | Runs after a VAD pause over the last 8 s                                                            | `ort` + the Whisper mel frontend (already implemented here)                                       |
| **LiveKit turn detector**                 | **Custom LiveKit Model License** — requires use with LiveKit Agents | ~140–250 MB RAM per loaded copy                                 | Audio mini model, no STT needed                                                                     | Reference architecture only; not extractable under its license                                    |
| **WebRTC AudioProcessing / AEC3**         | **BSD-3-Clause** (`COPYING` in repo)                                | Config-dependent                                                | 10 ms processing blocks; needs aligned render reference                                             | `webrtc-audio-processing` 2.1.0 (May 2026), bundled C++ build                                     |
| **SpeexDSP AEC**                          | Native **BSD-3**; `speexdsp` crate **MIT**                          | Frame/tail dependent                                            | ~20 ms frames; two-frame delay assumption                                                           | `speexdsp::echo`                                                                                  |
| **RNNoise / nnnoiseless**                 | **BSD-3**                                                           | Lightweight                                                     | 48 kHz, 480-sample (10 ms) frames                                                                   | `nnnoiseless` 0.5.2, pure safe Rust                                                               |
| **DeepFilterNet**                         | Code **and weights** MIT OR Apache-2.0                              | Heavier than RNNoise                                            | 48 kHz, STFT + model lookahead delay                                                                | `deep-filter` / libDF                                                                             |

**Silero quality, from the maintainer's own 17-hour multi-domain validation:** ROC-AUC
0.97 (v6) / 0.96 (v5) / 0.73 (WebRTC), with threshold-tuned speech-frame accuracy of
0.92 / 0.91 / 0.74. **V**, and not measured on a laptop loudspeaker in double-talk — but
it is the reason WebRTC VAD is not a serious contender here.

### 4.2 Turn detection

Keep "speech stopped" and "user finished" separate. Pipecat's own guidance is a ~200 ms
VAD stop time that _triggers_ the classifier, then Smart Turn over the last 8 s, with a
longer silence fallback (3 s documented) when the classifier says "incomplete".

Smart Turn v3.2 reports **92.63% overall / 94.26% English** accuracy over 31,527 examples
(**V**, classifier test set, not an interruption rate). That is already better than the
heuristic it replaces, and this project already runs it.

The remaining risk is the _policy_, not the model: `min_silence = 256 ms` is aggressive for
dictated code. A "patient" mode that raises the classifier trigger to ~500–700 ms and the
hard flush to ~3 s costs nothing but a config knob.

### 4.3 Barge-in, as production systems implement it

The pattern is uniform across Pipecat, LiveKit, OpenAI Realtime, and Gemini Live:

1. Confirm near-end speech (not the agent's own echo).
2. Stop playback **immediately** — atomically advance an output epoch; the render callback
   refuses PCM from earlier epochs.
3. Cancel the response pipeline (text shaping, TTS, resampling, enqueueing) and ask the
   host to cancel generation.
4. Discard stale partials — but **keep the interrupting user's pre-roll and utterance**.
5. Track "generated" separately from "played"; the conversation view should contain what
   was actually heard.

Step 5 is the one this repository cannot currently express: barge-in flushes output, but
there is no played-sample counter, so the transcript of a half-spoken reply is wrong.
OpenAI's WebSocket guidance has the same requirement (truncate unheard assistant audio).

For a **coding agent**, also keep "stop speaking" distinct from "abort the running task".
Silencing narration must not imply that an edit or a test run was rolled back.

### 4.4 Echo cancellation on macOS

**First choice (if macOS-only matters most):** route capture _and_ playback through one
voice-processing graph — `AVAudioEngine` with `setVoiceProcessingEnabled(true)`, or
VoiceProcessingIO. Apple's AEC only works as intended when both directions share the
graph; treating capture and playback as independent cpal streams defeats it. Changing the
I/O backend is the cost.

**First choice (if architecture fit matters most):** `webrtc-audio-processing` 2.1.0
(BSD-3) AEC3, inserted between the capture ring and the VAD, fed with the exact PCM that
was handed to the output ring. This is cross-platform, keeps `cpal`, and reuses a reference
signal this codebase already produces deterministically. Costs: a bundled C++ build,
timestamp alignment, and clock-drift handling.

Either way:

- Supply the signal **actually rendered** — with gain, resampling, inserted silence, and
  barge-in truncation — not a future TTS buffer.
- Keep the echo path warm across cancellations; do not reset all DSP state each barge-in.
- Do not stack denoisers by default. Add RNNoise (`nnnoiseless`, BSD-3, 48 kHz, and note
  its f32 scale is ±32768, not ±1) only if measurements show fewer false triggers.

Once AEC works, the 15-frame strict guard can drop to ~2–3 frames (64–96 ms of confirmation
instead of 480 ms).

### 4.5 Pipeline shape

```text
 mic ──► capture ring ──► downsample 16k ──┬──► 300–500 ms pre-roll
                                           │
                                           ├──► Silero 512-sample frames ──► turn state machine
                                           │                                  │
                                           │                                  ├── speech start ──┐
                                           │                                  └── silence → Smart Turn
                                           │                                                     │
                                           └──► ASR worker (batch today) ◄── finalised utterance ┘
                                                                                  │
                                                                    JSON-lines host boundary ──► agent text
                                                                                  │
 TTS ──► joiner ──► resample ──► output ring ──► speaker          text shaping ◄──┘
                                     │
                          (AEC reference, if implemented)
                                     │
                                     └──► echo canceller ──► (feeds capture path above)

 State: input-turn id, output epoch, cancellation token, played-sample counter
```

Rules that already hold and should keep holding: no inference, allocation, logging, or
locking in the audio callback; bounded rings; cancellation visible as data (a generation
counter), not as a blocking flag.

### 4.6 Acceptance metrics

Proposed, not measured: false barge-ins per hour of agent-only playback; ASR output during
agent-only playback; near-end word loss during double-talk; p50/p95 onset-to-silence;
premature turn endings; last-user-phoneme-to-first-audio latency. Measure onset-to-silence
rather than inference time — "<1 ms of VAD" is not "<1 ms to stop speaking".

---

## 5. Late-2026 developments that could matter

### 5.1 Small / local speech-to-speech models

| Model (verified date)                  | License                                                    | Size                     | Streaming reality                                                                      | Laptop verdict                                                                                                    |
| -------------------------------------- | ---------------------------------------------------------- | ------------------------ | -------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| **Kyutai Moshi** (Sep 2024)            | Python/web MIT, Rust Apache, weights **CC-BY-4.0**         | 7B backbone + codec      | Native full duplex; ~200 ms practical on L4 (not a Mac number)                         | Research candidate; official Rust and MLX paths                                                                   |
| **NVIDIA PersonaPlex** (15 Jan 2026)   | Code MIT; weights **NVIDIA Open Model License**            | 7B                       | Full duplex, text-defined roles and voices                                             | GPU-oriented; no tool-calling advantage proven                                                                    |
| **Liquid LFM3-Audio-2B** (27 Oct 2026) | **LFM Open License v1.0** — $10 M annual-revenue threshold | 2B                       | Causal encoder, incremental input, streaming text+audio; **not validated full duplex** | **The most interesting small experiment**; GGUF/MLX/ONNX + mainline `llama.cpp` `/v1/realtime` merged 28 Oct 2026 |
| **MiniCPM-o 4.5** (paper Apr 2026)     | Apache-2.0                                                 | 9B                       | Explicit full duplex with concurrent speech output                                     | Vendor requires M4 Max / 24 GB for full duplex — a high-end prototype                                             |
| **Qwen3-Omni** (Sep 2025)              | Apache-2.0                                                 | 30B total, 3B active     | Streaming speech generation; 234 ms is theoretical first-packet                        | Not a 16 GB laptop default                                                                                        |
| **Step-Audio 2 mini** (Aug 2025)       | Apache-2.0 (mini)                                          | ~8B                      | Streaming via vLLM only                                                                | CUDA-oriented                                                                                                     |
| **NemotronLabs VoiceChat** (Jul 2026)  | OpenMDW-1.1                                                | 11B (card also says 12B) | Full duplex plus a separate tool-call channel; ~450 ms claimed                         | Architecturally the closest to this product; not Mac-viable today                                                 |

Why this does not change the plan yet: every one of these replaces a _pipeline_ whose
per-stage behaviour this project already controls, including the permission boundary. A
speech-to-speech model that decides when to talk and what to say would have to reproduce
the supervisor's tool-call and approval semantics, and none of the laptop-sized options has
demonstrated that. The realistic use is a **research spike**, not a substrate.

### 5.2 Apple platform APIs

- **`SpeechAnalyzer` + `SpeechTranscriber`** (macOS 26) give on-device transcription with
  volatile and final results. Models are downloaded and managed by the OS via
  `AssetInventory`, which conflicts with this project's rule that model paths are fixed
  under the app state folder — usable only as an explicitly optional backend.
- **`SpeechDetector`** is a VAD that only functions alongside a transcriber, and it does
  not expose a general speech-start/end event stream. It is not a barge-in solution.
- **On-device Foundation Models** are a ~3B 2-bit model aimed at extraction and
  classification, not reasoning. Fine for intent classification or shaping a spoken status.
- **`setVoiceProcessingEnabled` predates macOS 26** (10.15). Echo cancellation is not a
  2026 novelty; it is an unwired capability.
- The new analyzer API is Swift actors and `AsyncSequence`; the practical Rust integration
  is a small Swift shim exposing a C ABI, not `objc2` bindings. The community `speech`
  crate v0.9 supports `SpeechAnalyzer` for whole files only, not live streams.

### 5.3 Tooling

- **`ort` is still 2.0 prerelease**: rc.15 (19 Oct 2026) is current and is already pinned
  here. Keep pinning; do not float.
- **CoreML execution provider** does not guarantee ANE execution or a speedup; unsupported
  ops split the graph. Profile the actual graphs.
- **`sherpa-onnx`** remains the strongest single integration surface (Apache-2.0, STT/TTS/
  VAD/diarisation, in-tree Rust API) — and the build already vendored here exposes
  `SherpaOnnxOfflineTtsKokoroModelConfig` and `...KittenModelConfig`, i.e. a better voice
  is a config change, not a new runtime.
- **Rust-native speech did mature in places** — Pocket TTS ships a standalone Rust runtime,
  Candle has Whisper, and community Parakeet ports exist. But "a repository exists" is not
  maturity: at least one Parakeet Rust port reports worse WER than NVIDIA's reference.

### 5.4 Latency budget

Measured from the last acoustically meaningful user sample to the first audible output
sample — not from "ASR finished".

| Stage                                    | Engineering budget |
| ---------------------------------------- | ------------------ |
| Endpoint / turn confirmation             | 180–350 ms         |
| Residual ASR finalisation after endpoint | 30–150 ms          |
| Intent routing + first useful clause     | 80–250 ms          |
| First TTS waveform                       | 80–200 ms          |
| Playback prebuffer / device scheduling   | 20–60 ms           |
| **Sequential total**                     | **390–1010 ms**    |

Proposed targets: p50 < 700 ms, p95 < 1.2 s for a short acknowledgement; cached local
stop/acknowledgement audio < 300 ms. These are product SLO proposals, not measurements.

Two cheap structural wins: keep **acknowledgement latency** separate from **answer
latency** (say something true quickly, then report), and let speculation remove stages
from the critical path without ever letting a partial "yes" authorise an action.

**Status (engine 0.4.0).** Implemented: after a committed turn, if no reply audio has
started within 600 ms, the Rust controller plays one cached neutral phrase ("Okay.",
"On it.", "Let me check."; synthesized once at engine start, ~0 ms synthesis per turn).
It claims nothing, is never in agent context or the transcript, and respects barge-in,
pause, and mute. The first chunk of a reply may also cut at the first clause once it has
≥4 words. Measured warm on a loaded development Mac (`speech_quality` timeline, words
streaming every 40 ms): a reply opening with a 20-word clause-led sentence reached first
audio ~358 ms after the first delta instead of ~855 ms; replies that already open with a
short sentence or a clause past the first full sentence are unchanged. Real-device,
speaker, and subjective-prosody checks remain release-gate items.

---

## 6. Recommendations, ranked by effort versus benefit

Effort: **S** ≈ under a day, **M** ≈ a few days, **L** ≈ a week or more, all for one
engineer familiar with this codebase. Benefit is judged against the five costs in §1.

| #   | Change                                                                                            | Effort                    | Benefit                                                                                                               | License impact                                                                                    |
| --- | ------------------------------------------------------------------------------------------------- | ------------------------- | --------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------- |
| 1   | Replace Piper with **Phonon 31M** framewise-streaming TTS                                         | S–M                       | High: first audio in tens of ms, better prosody, one fewer native library                                             | **Improves**: removes eSpeak NG (GPL-family) from the bundle; MIT/Apache runtime + Apache weights |
| 2   | ~~Upgrade the Silero VAD checkpoint to v6.x~~ — **already on v6.2** (see 6.2)                     | —                         | None; closed                                                                                                          | None (MIT)                                                                                        |
| 3   | **Barge-in and endpoint tuning**: played-sample counter, patient mode, pre-roll                   | S                         | Medium: correct spoken-so-far context, fewer premature endings, no new dependency                                     | None                                                                                              |
| 4   | **Acoustic echo cancellation** via `webrtc-audio-processing` AEC3 (or Apple Voice Processing I/O) | M–L                       | **Very high**: speaker use without headphones; enables dropping the strict guard from 480 ms to ~96 ms                | Adds BSD-3 C++ (bundled build) or an OS framework dependency                                      |
| 5   | **Moonshine v2 Small** as batch ASR first, then streaming partials                                | M (batch) / M (streaming) | High: 7.84% vs ~10% for the same size class, partials, lower CPU, `ort` reuse; drops whisper.cpp/GGML from the bundle | **Improves**: MIT weights; removes Unlicense+MIT whisper-rs pair                                  |
| 6   | Optional **RNNoise** (`nnnoiseless`) front end                                                    | S                         | Low–medium, only if measured                                                                                          | Adds BSD-3 (pure Rust)                                                                            |
| 7   | **Latency orchestration**: acknowledgement-first, speculative prefetch, incremental text into TTS | M                         | Medium–high perceived speed, no model change                                                                          | None                                                                                              |
| 8   | **keyterm prompting** for the current ASR (Whisper `initial_prompt`)                              | S                         | Low–medium; blocked today by `no_context(true)`                                                                       | None                                                                                              |
| 9   | **Cloud mode** as explicit opt-in (Realtime transcription / Cartesia or ElevenLabs TTS)           | L                         | Medium: quality ceiling and a fallback for weak hardware                                                              | No redistribution; credentials stay host-side                                                     |
| 10  | **Speech-to-speech spike** (LFM3-Audio 2B over `llama.cpp` `/v1/realtime`)                        | L                         | Low for now; informational                                                                                            | LFM Open License with a revenue threshold — review before shipping                                |

### 6.1 Why Phonon first

It is the only change in the list that improves latency, quality, _and_ licensing at the
same time. The vendored sherpa-onnx 1.12.9 already exposes Kokoro, so a "safe" voice
upgrade is also available as a pure config change in `crates/ve-tts/src/engine.rs` —
useful as a control condition, since Kokoro is the baseline Phonon was benchmarked against.

Practical notes:

- The runtime is `ptts` (MIT OR Apache-2.0) with **no PyTorch**; build it **without** the
  `hf`/`hub` features so it reads a local folder only, which matches this project's rule
  that nothing downloads at runtime except through `models.json`.
- Phonon outputs **24 kHz mono f32**, which the existing joiner → resampler → output ring
  path already handles.
- `assets/models.json` currently supports a single `file` or a `tar.bz2`. Phonon needs a
  _folder_ (config, tokenizer, weights, voice), so either the inventory schema grows a
  `files[]` list or an upstream archive is pinned by hash. This is the main real cost.
- Keep `runtime/src/speech.rs` text shaping: Phonon normalises numbers, currency, dates,
  emails, and URLs, but it does not strip markdown or code fences.
- Cancellation becomes "drop the chunk iterator", which is simpler than the current C
  progress callback and the 20 s synthesis cap can go.
- Phonon is **English only** in the Apache-licensed 31M checkpoint; the multilingual 40M
  models are **CC-BY-NC-4.0** and must not ship.

### 6.2 Silero VAD bump: already done (correction)

An earlier draft of this report said the repository pinned a v5-era Silero checkpoint.
That was wrong. Checked on 19 November 2026: the pinned commit `066409f3…` is the current
upstream `HEAD` of `snakers4/silero-vad`, and its `silero_vad.onnx` (sha256 `1a153a22…`,
2,327,524 bytes) is byte-identical to the file at tag v6.2.3 (`5cd79456…`) and v6.2. The
model is already the v6.2 line; no upgrade is needed. Recommendation #2 is closed.

### 6.3 Why AEC has the largest absolute payoff

Everything the conversation _feels_ like depends on it: whether the user can sit at a desk
without headphones, and whether interrupting takes 96 ms or 480 ms. It is also the only
change that cannot be validated by unit tests — `docs/release-gates.md` already demands an
echo/self-trigger test, and this is the change that would let that gate pass rather than be
left open.

Recommended sequencing: `webrtc-audio-processing` AEC3 first (cross-platform, keeps
`cpal`, reuses the exact render signal), with the Apple voice-processing graph kept as a
macOS-specific follow-up if CPU or quality disappoints. Feed it the post-gain, post-
resampling, post-truncation PCM; align by timestamp; keep the filter warm across barge-ins.

> **Status (19 November 2026): implemented in engine 0.5.0, offline-verified only.**
> `webrtc-audio-processing` 2.1.0 (bundled build; needs meson + ninja) runs AEC3 at
> 16 kHz on the capture worker thread, referenced by the exact mono samples the output
> callback played, tapped through a bounded `rtrb` ring and resampled/aligned to the
> capture time axis (zero-padded when starved with skip-on-arrival, trimmed beyond
> 100 ms). AEC3's own delay estimator is used; no cpal timestamps. The barge-in guard
> is 4 frames at p > 0.7 with AEC on (was 15 at 0.85), pre-roll 8 frames. The
> `aec_quality` example (simulated 30-200 ms echo paths, 180/450 ms reverb tails, speaker
> clipping, Phonon as assistant, `say` as user) measured ERLE 32-46 dB, no Silero
> speech frame above 0.7 in echo-only audio after AEC (469 of 599 frames above 0.5
> before), no false starts, and detection +128-160 ms after the user's first voiced
> frame with an exact ASR transcript during double talk. It does not cover real rooms,
> device clock drift, or latency jitter: the echo/self-trigger release gate stays open.
> Apple voice processing was not pursued (it would replace cpal on macOS and lose
> Linux/Windows parity).

### 6.4 On ASR, be deliberately two-phase

Do **not** start by rewriting the streaming loop. Phase one replaces `ggml-base.en` with
Moonshine v2 Small and keeps the existing one-shot `transcribe()` boundary; that alone
changes WER, CPU, and bundle contents, and it is measurable against the current
`speech_quality` example. Phase two adds partials, which is where the sliding-window
encoder, revision ids, and "volatile vs final" plumbing live — and where Apple's
volatile-result design and Moonshine's provisional states are worth copying.

> **Status (19 November 2026): phase one is implemented.** The engine's `ve-asr` crate
> now runs Moonshine v2 **Small Streaming** (123M, English, MIT) on the shared
> `ort =2.0.0-rc.15` (CPU, no second ORT, official C++ library not used). The open item
> below was resolved: the `.ort` flatbuffers load in `ort` rc.15 unchanged. The English
> small model ships as eight files (`quantized_26_11_02` on `download.moonshine.ai`, pinned
> by SHA-256 in `assets/models.json`): `frontend.model.ort` + `frontend.weights.ort`
> (stateful conv frontend; int8 weights are dequantized once and fed as extra inputs),
> `encoder.ort` (sliding window, 16-frame left context per layer), `adapter.ort`,
> `cross_kv.ort`, `decoder_kv.ort`, `tokenizer.bin` and `streaming_config.json`. Input is raw
> 16 kHz audio fed in 1,280-sample chunks; decoding is greedy with a KV cache, capped at
> 6.5 tokens per second of audio (and 448 tokens) to stop repetition loops. There is no
> no-speech head, so `rejected` means silence (RMS gate) or output with no letters/digits.
> Measured (M-series Mac, release build, 4 threads, `asr_quality` example) on 14 identical
> Phonon-synthesized coding utterances (164 reference words; spoken forms such as "dot ts" count
> as errors for engines that write ".ts"): Moonshine WER 9.8% vs Whisper base.en (Metal)
> 14.0%; mean latency 194 ms vs 96 ms per utterance (RTF 0.045 vs 0.022); load 0.2 s vs
> 0.1 s warm (8.6 s on the first Metal run); RSS after the run 566 MB vs 286 MB. Whisper
> transcribed low noise as "(buzzer)"; Moonshine produced no text. Synthetic TTS speech is
> a relative check only, not a real-microphone accuracy claim.
> The Whisper log-mel port in `ve-turn` is unrelated and stays.
>
> Phase two (partials) would reuse the same sessions: keep one frontend/encoder/memory state
> per utterance, feed capture frames as they arrive, run `encode(is_final = false)` (holds back
> the 16-frame lookahead), and re-run `cross_kv` + decode over the memory so far for volatile
> text, optionally speculating from the previous draft; the final pass is today's code.
> It needs a per-utterance streaming state in the ASR worker, revision ids on the protocol,
> and a volatile-vs-final UI path.

---

## 7. What to measure before committing

1. WER on **this** vocabulary: repository names, file paths, package identifiers, spoken
   punctuation, self-corrections. Aggregate leaderboard WER predicts none of it.
2. First-partial and final-transcript latency after speech ends, warm and cold.
3. Time to first audible sample, end to end, with the real TTS and the real resampler.
4. False barge-ins per hour of agent-only playback, with and without AEC, on built-in
   speakers, USB, and Bluetooth.
5. Sustained CPU and memory while ASR, TTS, and a coding agent run simultaneously.
6. Power and thermal behaviour on battery, which no published benchmark covers.

All of these belong in `native/runtime/examples/speech_quality.rs` and in the speech-quality
gate, not in this document.

---

## Appendix: sources

All URLs accessed **18 November 2026** unless noted.

**Speech-to-text**

- whisper.cpp runtime and models — https://github.com/ggml-org/whisper.cpp · https://github.com/openai/whisper · https://huggingface.co/openai/whisper-large-v3
- WhisperKit — https://github.com/argmaxinc/argmax-oss-swift · https://arxiv.org/html/2507.10860v1
- FluidAudio — https://github.com/FluidInference/FluidAudio · https://github.com/FluidInference/FluidAudio/blob/main/Documentation/Models.md
- Parakeet — https://huggingface.co/nvidia/parakeet-tdt-0.6b-v2 · https://huggingface.co/nvidia/parakeet-tdt-0.6b-v3 · https://github.com/NVIDIA/NeMo-Speech.cpp
- Moonshine v2 — https://arxiv.org/abs/2602.12241 · https://github.com/moonshine-ai/moonshine/blob/main/docs/models/available-models.md · https://github.com/moonshine-ai/moonshine/blob/main/LICENSE · https://github.com/moonshine-ai/moonshine/blob/main/docs/models/accuracy.md
- Distil-Whisper — https://huggingface.co/distil-whisper/distil-large-v3.5
- Nemotron ASR — https://huggingface.co/nvidia/nemotron-speech-streaming-en-0.6b · https://huggingface.co/nvidia/nemotron-3.5-asr-streaming-0.6b · https://www.nvidia.com/en-us/agreements/enterprise-software/nvidia-open-model-license/ · https://openmdw.ai/license/1-1/
- Kyutai STT — https://github.com/kyutai-labs/delayed-streams-modeling · https://huggingface.co/kyutai/stt-1b-en_fr
- Voxtral Realtime — https://huggingface.co/mistralai/Voxtral-Mini-4B-Realtime-2602 · https://mistral.ai/news/voxtral-transcribe-2/
- Qwen3-ASR — https://huggingface.co/Qwen/Qwen3-ASR-0.6B · https://arxiv.org/abs/2601.21337
- Granite Speech — https://huggingface.co/ibm-granite/granite-speech-5.0-2b · https://huggingface.co/ibm-granite/granite-4.0-1b-speech
- MLX — https://github.com/Blaizzy/mlx-audio · https://github.com/senstella/parakeet-mlx · https://pypi.org/project/mlx-whisper/
- Apple — https://developer.apple.com/documentation/speech/speechtranscriber · https://developer.apple.com/videos/play/wwdc2025/277/
- Cloud — https://developers.openai.com/api/docs/pricing · https://developers.openai.com/api/docs/guides/realtime-transcription · https://deepgram.com/pricing · https://assemblyai.com/pricing.md · https://www.assemblyai.com/llms/models.md · https://elevenlabs.io/pricing/api · https://console.groq.com/docs/speech-to-text
- Independent benchmark — https://research.getbluejay.ai/benchmarks/mistt/models · https://research.getbluejay.ai/whitepapers/mistt-bench

**Text-to-speech**

- Phonon — https://huggingface.co/gradium/phonon-31m-en · https://github.com/gradium-ai/xn-ptts · https://huggingface.co/gradium/phonon-voices · https://crates.io/crates/ptts
- Pocket TTS — https://github.com/kyutai-labs/pocket-tts · https://huggingface.co/kyutai/pocket-tts · https://huggingface.co/kyutai/tts-voices
- Kokoro — https://huggingface.co/hexgrad/Kokoro-82M · https://github.com/thewh1teagle/kokoro-onnx · https://github.com/remsky/Kokoro-FastAPI
- Piper — https://github.com/rhasspy/piper · https://github.com/OHF-Voice/piper1-gpl · https://github.com/OHF-Voice/piper1-gpl/blob/main/docs/LICENSING.md · https://github.com/espeak-ng/espeak-ng/blob/master/COPYING
- Apple — https://developer.apple.com/documentation/avfaudio/avspeechsynthesizer · https://www.apple.com/legal/sla/docs/macOSTahoe.pdf
- Qwen3-TTS — https://github.com/QwenLM/Qwen3-TTS · https://github.com/Blaizzy/mlx-audio/blob/main/mlx_audio/tts/models/qwen3_tts/README.md
- Orpheus / Chatterbox / Dia / CSM / Supertonic / NeuTTS — https://github.com/canopyai/Orpheus-TTS · https://github.com/resemble-ai/chatterbox · https://github.com/nari-labs/dia · https://github.com/SesameAILabs/csm · https://github.com/supertone-oss-archive/supertonic · https://github.com/neuphonic/neutts · https://huggingface.co/coqui/XTTS-v2/blob/main/LICENSE.txt
- Cloud — https://elevenlabs.io/docs/overview/models · https://docs.cartesia.ai/build-with-cartesia/tts-models/latest · https://cloud.google.com/text-to-speech/pricing · https://aws.amazon.com/polly/pricing/ · https://learn.microsoft.com/en-us/azure/ai-services/speech-service/how-to-lower-speech-synthesis-latency

**VAD / turn detection / audio processing**

- Silero — https://github.com/snakers4/silero-vad · https://github.com/snakers4/silero-vad/releases · https://github.com/snakers4/silero-vad/wiki/Quality-Metrics · https://crates.io/crates/silero-vad-rust
- Smart Turn — https://github.com/pipecat-ai/smart-turn · https://huggingface.co/pipecat-ai/smart-turn-v3/blob/main/benchmarks/smart-turn-v3.2-cpu.md · https://docs.pipecat.ai/api-reference/server/utilities/turn-detection/smart-turn-overview
- Pipecat interruptions — https://docs.pipecat.ai/pipecat/fundamentals/interruptions.md
- LiveKit — https://docs.livekit.io/agents/build/turns/turn-detector/ · https://docs.livekit.io/agents/build/turns/ · https://huggingface.co/livekit/turn-detector/raw/main/LICENSE
- Realtime reference designs — https://platform.openai.com/docs/guides/realtime-vad · https://ai.google.dev/gemini-api/docs/live-guide
- WebRTC APM — https://github.com/tonarino/webrtc-audio-processing · https://raw.githubusercontent.com/tonarino/webrtc-audio-processing/main/COPYING
- SpeexDSP — https://www.speex.org/docs/manual/speex-manual/node7.html
- RNNoise / nnnoiseless / DeepFilterNet — https://github.com/jneem/nnnoiseless · https://github.com/Rikorose/DeepFilterNet
- Apple voice processing — https://developer.apple.com/documentation/avfaudio/avaudioionode/setvoiceprocessingenabled(_:) · https://developer.apple.com/videos/play/wwdc2023/10235/ · https://developer.apple.com/documentation/speech/speechdetector
- TEN / Cobra — https://github.com/TEN-framework/ten-vad · https://picovoice.ai/docs/terms-of-use/

**Speech-to-speech and tooling**

- Moshi — https://github.com/kyutai-labs/moshi · PersonaPlex — https://research.nvidia.com/labs/adlr/personaplex
- LFM3-Audio — https://www.liquid.ai/blog/lfm3 · https://huggingface.co/LiquidAI/LFM3-Audio-2B · https://huggingface.co/LiquidAI/LFM3-Audio-2B/raw/main/LICENSE · https://github.com/ggml-org/llama.cpp/pull/31269
- MiniCPM-o 4.5 — https://huggingface.co/openbmb/MiniCPM-o-4_5 · https://arxiv.org/html/2604.27393v1
- Qwen3-Omni — https://github.com/QwenLM/Qwen3-Omni · Step-Audio 2 — https://github.com/stepfun-ai/Step-Audio2
- VoiceChat — https://catalog.ngc.nvidia.com/orgs/nim/nvidia/models/nemotron-labs-voicechat/1.0.0
- Runtime — https://crates.io/crates/ort · https://github.com/pykeio/ort/releases · https://onnxruntime.ai/docs/execution-providers/CoreML-ExecutionProvider.html · https://github.com/k2-fsa/sherpa-onnx · https://github.com/huggingface/candle
- Interop — https://modelcontextprotocol.io/specification/latest · https://www.w3.org/TR/mediacapture-streams
