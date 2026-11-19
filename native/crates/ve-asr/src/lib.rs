//! English-only Moonshine v2 Small Streaming (123M, MIT) inference on ONNX Runtime.
//!
//! All paths are explicit; no checkout/cache probing. [`Asr::transcribe`] is one-shot: the
//! whole utterance goes through the stateful streaming frontend in 80 ms-aligned chunks, the
//! sliding-window encoder, the adapter, and then a greedy KV-cached decode, exactly as the
//! official Moonshine C++ runtime's final pass does.
mod tokenizer;

use anyhow::{ensure, Context, Result};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use tokenizer::Tokenizer;

/// Files the model directory must contain.
pub const MODEL_FILES: [&str; 8] = [
    "adapter.ort",
    "cross_kv.ort",
    "decoder_kv.ort",
    "encoder.ort",
    "frontend.model.ort",
    "frontend.weights.ort",
    "streaming_config.json",
    "tokenizer.bin",
];

/// The frontend consumes whole 80 ms frames; the official runtime feeds 1280-sample chunks.
const CHUNK: usize = 1_280;
/// Encoder frames are 20 ms of audio each.
const SECONDS_PER_FRAME: f32 = 0.02;
/// Official default `max_tokens_per_second` for Latin-script languages. The only guard
/// against the characteristic repeat-the-last-words hallucination loop.
const MAX_TOKENS_PER_SECOND: f32 = 6.5;
/// Official decoder limit when `max_seq_len` is absent from the config.
const MAX_SEQ_LEN: usize = 448;
/// Mean-square energy below which audio is silence and is not sent to the model.
const MIN_ENERGY: f32 = 1e-10;

pub struct AsrConfig {
    /// Directory holding [`MODEL_FILES`].
    pub model_dir: PathBuf,
    pub n_threads: usize,
}
pub struct Transcript {
    pub text: String,
    /// Silence, empty output, or output with no letters or digits.
    pub rejected: bool,
}

#[derive(Deserialize)]
struct ModelConfig {
    encoder_dim: usize,
    decoder_dim: usize,
    depth: usize,
    nheads: usize,
    head_dim: usize,
    vocab_size: usize,
    bos_id: i64,
    eos_id: i64,
    frame_len: usize,
    d_model_frontend: usize,
    c1: usize,
}

struct Weight {
    name: String,
    shape: Vec<usize>,
    data: Vec<f32>,
}

pub struct Asr {
    cfg: ModelConfig,
    frontend: Session,
    encoder: Session,
    adapter: Session,
    cross_kv: Session,
    decoder: Session,
    weights: Vec<Weight>,
    tokenizer: Tokenizer,
}

fn session(path: &Path, threads: usize) -> Result<Session> {
    let env = ve_vad::ort_support::shared_env()?;
    use ve_vad::ort_support::err as e;
    Session::builder(&env)
        .map_err(e)?
        .with_intra_threads(threads)
        .map_err(e)?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(e)?
        .commit_from_file(path)
        .map_err(e)
}

fn f32_out(outputs: &ort::session::SessionOutputs, name: &str) -> Result<(Vec<usize>, Vec<f32>)> {
    let value = outputs
        .get(name)
        .with_context(|| format!("Moonshine output {name} is missing"))?;
    let (shape, data) = value
        .try_extract_tensor::<f32>()
        .with_context(|| format!("Moonshine output {name} must be f32"))?;
    Ok((shape.iter().map(|d| *d as usize).collect(), data.to_vec()))
}

impl Asr {
    pub fn from_config(cfg: AsrConfig) -> Result<Self> {
        for file in MODEL_FILES {
            ensure!(
                cfg.model_dir.join(file).is_file(),
                "Moonshine model file {file} not found in {}; run opencode-converse doctor",
                cfg.model_dir.display()
            );
        }
        let dir = &cfg.model_dir;
        let threads = cfg.n_threads.max(1);
        let model: ModelConfig =
            serde_json::from_slice(&std::fs::read(dir.join("streaming_config.json"))?)
                .context("Invalid Moonshine streaming_config.json")?;
        ensure!(
            model.frame_len * 16 == CHUNK
                && model.depth > 0
                && model.nheads * model.head_dim > 0
                && model.vocab_size > 0,
            "Unsupported Moonshine streaming configuration"
        );
        let tokenizer = Tokenizer::from_file(&dir.join("tokenizer.bin"), model.vocab_size)?;
        let frontend = session(&dir.join("frontend.model.ort"), threads)
            .context("Loading Moonshine frontend")?;
        // The int8 frontend weights ship as a graph whose outputs are the float tensors the
        // frontend takes as extra inputs; dequantize them once.
        let mut weights_session = session(&dir.join("frontend.weights.ort"), threads)
            .context("Loading Moonshine frontend weights")?;
        let names: Vec<String> = weights_session
            .outputs()
            .iter()
            .map(|o| o.name().to_owned())
            .collect();
        let outputs = weights_session
            .run(Vec::<(String, ort::session::SessionInputValue)>::new())
            .map_err(ve_vad::ort_support::err)?;
        let mut weights = Vec::new();
        for name in names {
            let (shape, data) = f32_out(&outputs, &name)?;
            weights.push(Weight { name, shape, data });
        }
        drop(outputs);
        Ok(Self {
            frontend,
            encoder: session(&dir.join("encoder.ort"), threads).context("Loading encoder")?,
            adapter: session(&dir.join("adapter.ort"), threads).context("Loading adapter")?,
            cross_kv: session(&dir.join("cross_kv.ort"), threads).context("Loading cross_kv")?,
            decoder: session(&dir.join("decoder_kv.ort"), threads).context("Loading decoder")?,
            cfg: model,
            weights,
            tokenizer,
        })
    }

    /// Streaming frontend over the whole utterance; returns `[frames, encoder_dim]` features.
    fn features(&mut self, audio: &[f32]) -> Result<(usize, Vec<f32>)> {
        let c = &self.cfg;
        use ve_vad::ort_support::err as e;
        let mut sample_buffer = vec![0.0f32; 79];
        let mut sample_len = 0i64;
        let mut conv1 = vec![0.0f32; c.d_model_frontend * 4];
        let mut conv2 = vec![0.0f32; c.c1 * 4];
        let mut frame_count = 0i64;
        let mut features = Vec::new();
        let mut frames = 0usize;
        // Whole 80 ms frames only: a ragged tail is zero-padded so no audio is dropped.
        let mut padded = audio.to_vec();
        padded.resize(audio.len().div_ceil(CHUNK) * CHUNK, 0.0);
        for chunk in padded.chunks(CHUNK) {
            let mut inputs: Vec<(String, Tensor<f32>)> = Vec::new();
            let mut ints: Vec<(String, Tensor<i64>)> = Vec::new();
            inputs.push((
                "audio_chunk".into(),
                Tensor::from_array(([1usize, CHUNK], chunk.to_vec())).map_err(e)?,
            ));
            inputs.push((
                "sample_buffer".into(),
                Tensor::from_array(([1usize, 79], sample_buffer.clone())).map_err(e)?,
            ));
            ints.push((
                "sample_len".into(),
                Tensor::from_array(([1usize], vec![sample_len])).map_err(e)?,
            ));
            inputs.push((
                "conv1_buffer".into(),
                Tensor::from_array(([1usize, c.d_model_frontend, 4], conv1.clone())).map_err(e)?,
            ));
            inputs.push((
                "conv2_buffer".into(),
                Tensor::from_array(([1usize, c.c1, 4], conv2.clone())).map_err(e)?,
            ));
            ints.push((
                "frame_count".into(),
                Tensor::from_array(([1usize], vec![frame_count])).map_err(e)?,
            ));
            for w in &self.weights {
                inputs.push((
                    w.name.clone(),
                    Tensor::from_array((w.shape.clone(), w.data.clone())).map_err(e)?,
                ));
            }
            let mut all: Vec<(String, ort::session::SessionInputValue)> = Vec::new();
            all.extend(inputs.into_iter().map(|(k, v)| (k, v.into())));
            all.extend(ints.into_iter().map(|(k, v)| (k, v.into())));
            let out = self.frontend.run(all).map_err(e)?;
            let (shape, feat) = f32_out(&out, "features")?;
            ensure!(shape.len() == 3, "Unexpected frontend feature rank");
            frames += shape[1];
            features.extend_from_slice(&feat);
            sample_buffer = f32_out(&out, "sample_buffer_out")?.1;
            conv1 = f32_out(&out, "conv1_buffer_out")?.1;
            conv2 = f32_out(&out, "conv2_buffer_out")?.1;
            sample_len = i64_scalar(&out, "sample_len_out")?;
            frame_count = i64_scalar(&out, "frame_count_out")?;
        }
        Ok((frames, features))
    }

    /// Final (non-streaming) encode: every frame is stable, the whole sequence is one window.
    fn memory(&mut self, frames: usize, features: Vec<f32>) -> Result<(usize, Vec<f32>)> {
        use ve_vad::ort_support::err as e;
        let encoded = {
            let input = Tensor::from_array(([1usize, frames, self.cfg.encoder_dim], features))
                .map_err(e)?;
            let out = self
                .encoder
                .run(ort::inputs!["features" => input])
                .map_err(e)?;
            f32_out(&out, "encoded")?
        };
        ensure!(
            encoded.0.get(1) == Some(&frames),
            "Moonshine encoder returned an unexpected frame count"
        );
        let encoded =
            Tensor::from_array(([1usize, frames, self.cfg.encoder_dim], encoded.1)).map_err(e)?;
        let pos = Tensor::from_array(([1usize], vec![0i64])).map_err(e)?;
        let out = self
            .adapter
            .run(ort::inputs!["encoded" => encoded, "pos_offset" => pos])
            .map_err(e)?;
        let (shape, memory) = f32_out(&out, "memory")?;
        ensure!(
            shape.get(1) == Some(&frames),
            "Moonshine adapter returned an unexpected frame count"
        );
        Ok((frames, memory))
    }

    fn decode(&mut self, frames: usize, memory: Vec<f32>) -> Result<Vec<i64>> {
        use ve_vad::ort_support::err as e;
        let c = &self.cfg;
        let (depth, heads, hd, vocab) = (c.depth, c.nheads, c.head_dim, c.vocab_size);
        let (bos, eos) = (c.bos_id, c.eos_id);
        let memory = Tensor::from_array(([1usize, frames, c.decoder_dim], memory)).map_err(e)?;
        let out = self
            .cross_kv
            .run(ort::inputs!["memory" => memory])
            .map_err(e)?;
        let (cross_shape, k_cross) = f32_out(&out, "k_cross")?;
        let (_, v_cross) = f32_out(&out, "v_cross")?;
        drop(out);
        ensure!(cross_shape.len() == 5, "Unexpected cross-attention shape");
        let cross_len = cross_shape[3];

        let seconds = frames as f32 * SECONDS_PER_FRAME;
        let max_tokens = ((seconds * MAX_TOKENS_PER_SECOND).ceil() as usize).clamp(1, MAX_SEQ_LEN);
        let mut k_self: Vec<f32> = Vec::new();
        let mut v_self: Vec<f32> = Vec::new();
        let mut cache_len = 0usize;
        let mut tokens = Vec::new();
        let mut current = bos;
        loop {
            let out = self
                .decoder
                .run(ort::inputs![
                    "token" => Tensor::from_array(([1usize, 1], vec![current])).map_err(e)?,
                    "k_self" => Tensor::from_array(([depth, 1, heads, cache_len, hd], std::mem::take(&mut k_self))).map_err(e)?,
                    "v_self" => Tensor::from_array(([depth, 1, heads, cache_len, hd], std::mem::take(&mut v_self))).map_err(e)?,
                    "out_k_cross" => Tensor::from_array(([depth, 1, heads, cross_len, hd], k_cross.clone())).map_err(e)?,
                    "out_v_cross" => Tensor::from_array(([depth, 1, heads, cross_len, hd], v_cross.clone())).map_err(e)?,
                ])
                .map_err(e)?;
            let (_, logits) = out
                .get("logits")
                .context("Moonshine decoder logits are missing")?
                .try_extract_tensor::<f32>()
                .map_err(e)?;
            ensure!(logits.len() == vocab, "Unexpected Moonshine logits size");
            let next = logits
                .iter()
                .enumerate()
                .fold((0usize, f32::NEG_INFINITY), |best, (i, v)| {
                    if *v > best.1 {
                        (i, *v)
                    } else {
                        best
                    }
                })
                .0 as i64;
            let (k_shape, k) = f32_out(&out, "out_k_self")?;
            k_self = k;
            v_self = f32_out(&out, "out_v_self")?.1;
            cache_len = *k_shape.get(3).context("Unexpected self-attention shape")?;
            drop(out);
            if next == eos {
                break;
            }
            tokens.push(next);
            if tokens.len() >= max_tokens {
                break;
            }
            current = next;
        }
        Ok(tokens)
    }

    pub fn transcribe(&mut self, audio: &[f32]) -> Result<Transcript> {
        let silent = audio.is_empty()
            || audio.iter().map(|s| s * s).sum::<f32>() / (audio.len() as f32) < MIN_ENERGY;
        if silent {
            return Ok(Transcript {
                text: String::new(),
                rejected: true,
            });
        }
        let (frames, features) = self.features(audio)?;
        if frames == 0 {
            return Ok(Transcript {
                text: String::new(),
                rejected: true,
            });
        }
        let (frames, memory) = self.memory(frames, features)?;
        let tokens = self.decode(frames, memory)?;
        let text = self.tokenizer.decode(&tokens);
        // Moonshine has no no-speech head; empty or letterless output is the signal.
        let rejected = !text.chars().any(char::is_alphanumeric);
        Ok(Transcript { text, rejected })
    }
}

fn i64_scalar(outputs: &ort::session::SessionOutputs, name: &str) -> Result<i64> {
    let (_, data) = outputs
        .get(name)
        .with_context(|| format!("Moonshine output {name} is missing"))?
        .try_extract_tensor::<i64>()
        .map_err(ve_vad::ort_support::err)?;
    data.first()
        .copied()
        .with_context(|| format!("Moonshine output {name} is empty"))
}
