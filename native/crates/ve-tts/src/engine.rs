//! Gradium Phonon 31M through the `ptts` runtime: CPU, q8 weights, local folder only.
use anyhow::{ensure, Context, Result};
use ptts::checkpoint::{Checkpoint, ResolveOptions};
use ptts::preprocess::Lang;
use ptts::synth::{DeviceKind, Quant, Synth};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// Hard bound on the audio one request may produce. Longer speech is truncated, not an error.
const MAX_SECONDS: usize = 30;

pub struct TtsConfig {
    /// Checkpoint directory: `config.json`, `tokenizer.json`, `model.q8.gguf`,
    /// `default-voice.safetensors`. Nothing is ever downloaded.
    pub dir: PathBuf,
}

pub struct Tts {
    synth: Synth,
    sample_rate: u32,
}

/// How a streamed synthesis ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    Complete,
    Cancelled,
    /// The per-request audio bound was reached; the audio delivered so far is valid.
    Truncated,
}

impl Tts {
    pub fn new(config: TtsConfig) -> Result<Self> {
        ensure!(
            config.dir.is_dir(),
            "Phonon model directory not found at {}; run opencode-converse models install",
            config.dir.display()
        );
        let checkpoint = Checkpoint::resolve(
            &config.dir,
            ResolveOptions {
                quant: Quant::Q80,
                weights: Some("model.q8.gguf"),
            },
        )
        .with_context(|| format!("Invalid Phonon model at {}", config.dir.display()))?;
        let mut synth = checkpoint
            .builder(Lang::En)
            .device(DeviceKind::Cpu)
            .build()
            .with_context(|| {
                format!(
                    "Phonon failed to load {}; run opencode-converse doctor",
                    config.dir.display()
                )
            })?;
        checkpoint.register_voices(&mut synth);
        ensure!(
            synth.voices().iter().any(|voice| voice == "default"),
            "Phonon default voice is missing from {}",
            config.dir.display()
        );
        let sample_rate = synth.sample_rate();
        ensure!(sample_rate > 0, "Phonon returned an invalid sample rate");
        Ok(Self { synth, sample_rate })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Synthesize `text`, handing each PCM chunk to `on_chunk` as soon as it exists.
    /// Cancellation is checked before every pull and after every chunk; dropping the stream
    /// stops generation, so the worst-case delay is one frame of model work.
    pub fn synth_stream(
        &mut self,
        text: &str,
        cancel: &AtomicBool,
        mut on_chunk: impl FnMut(&[f32]) -> Result<()>,
    ) -> Result<Finish> {
        if text.trim().is_empty() {
            return Ok(Finish::Complete);
        }
        if cancel.load(Ordering::Acquire) {
            return Ok(Finish::Cancelled);
        }
        let limit = self.sample_rate as usize * MAX_SECONDS;
        let mut produced = 0usize;
        let stream = self.synth.stream(text)?;
        for chunk in stream {
            if cancel.load(Ordering::Acquire) {
                return Ok(Finish::Cancelled);
            }
            let chunk = chunk?;
            ensure!(
                chunk.iter().all(|sample| sample.is_finite()),
                "Phonon produced a non-finite sample"
            );
            let room = limit.saturating_sub(produced);
            let take = chunk.len().min(room);
            if take > 0 {
                on_chunk(&chunk[..take])?;
                produced += take;
            }
            if take < chunk.len() || produced >= limit {
                return Ok(Finish::Truncated);
            }
            if cancel.load(Ordering::Acquire) {
                return Ok(Finish::Cancelled);
            }
        }
        Ok(Finish::Complete)
    }

    /// Whole-chunk convenience built on [`Self::synth_stream`]. Cancelled synthesis returns
    /// no audio, so a previously buffered prefix can never be played afterwards.
    pub fn synth(&mut self, text: &str, cancel: &AtomicBool) -> Result<Vec<f32>> {
        let mut samples = Vec::new();
        let finish = self.synth_stream(text, cancel, |chunk| {
            samples.extend_from_slice(chunk);
            Ok(())
        })?;
        Ok(
            if finish == Finish::Cancelled || cancel.load(Ordering::Acquire) {
                Vec::new()
            } else {
                samples
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_model_directory_names_the_installer() {
        let dir = std::env::temp_dir().join("ve-tts-missing-model-directory");
        let error = Tts::new(TtsConfig { dir }).err().unwrap().to_string();
        assert!(error.contains("models install"), "{error}");
    }

    #[test]
    fn incomplete_model_directory_is_rejected_before_loading() {
        let dir = std::env::temp_dir().join(format!("ve-tts-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let error = format!(
            "{:#}",
            Tts::new(TtsConfig { dir: dir.clone() }).err().unwrap()
        );
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(error.contains("Invalid Phonon model"), "{error}");
    }
}
