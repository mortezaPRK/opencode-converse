//! English-only Whisper inference. All paths are explicit; no checkout/cache probing.
use anyhow::{Context, Result};
use std::path::PathBuf;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

pub fn silence_logs() {
    whisper_rs::install_logging_hooks();
}
pub struct AsrConfig {
    pub model_path: PathBuf,
    pub n_threads: i32,
    pub use_gpu: bool,
}
pub struct Transcript {
    pub text: String,
    pub rejected: bool,
}
pub struct WhisperAsr {
    state: WhisperState,
    threads: i32,
}
impl WhisperAsr {
    pub fn from_config(cfg: AsrConfig) -> Result<Self> {
        anyhow::ensure!(
            cfg.model_path.is_file(),
            "Whisper model not found at {}; run opencode-converse doctor",
            cfg.model_path.display()
        );
        let mut params = WhisperContextParameters::default();
        params.use_gpu(cfg.use_gpu);
        params.flash_attn(true);
        let context =
            WhisperContext::new_with_params(&cfg.model_path, params).context("Loading Whisper")?;
        // WhisperState holds the context's Arc for its full lifetime.
        let state = context.create_state().context("Creating Whisper state")?;
        Ok(Self {
            state,
            threads: cfg.n_threads.max(1),
        })
    }
    pub fn transcribe(&mut self, audio: &[f32]) -> Result<Transcript> {
        if audio.is_empty()
            || audio.iter().map(|s| s * s).sum::<f32>() / (audio.len() as f32) < 1e-10
        {
            return Ok(Transcript {
                text: String::new(),
                rejected: true,
            });
        }
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_n_threads(self.threads);
        params.set_temperature(0.0);
        params.set_temperature_inc(0.0);
        params.set_translate(false);
        params.set_language(Some("en"));
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_special(false);
        params.set_print_timestamps(false);
        params.set_no_context(true);
        params.set_single_segment(true);
        params.set_no_timestamps(true);
        // Keep Whisper's full encoder context; shortening this can corrupt transcripts.
        self.state
            .full(params, audio)
            .context("Whisper inference failed")?;
        let mut text = String::new();
        let mut no_speech = 1.0;
        for (i, segment) in self.state.as_iter().enumerate() {
            if i == 0 {
                no_speech = segment.no_speech_probability();
            }
            text.push_str(segment.to_str_lossy()?.as_ref());
        }
        let text = text.trim().to_owned();
        let rejected = text.is_empty() || no_speech > 0.6;
        Ok(Transcript { text, rejected })
    }
}
