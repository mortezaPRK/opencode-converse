//! Smart Turn v3 (ONNX Runtime) end-of-turn detection, plus the
//! hand-written Whisper log-mel front end it needs.
//!
//! The frontend follows Whisper's reference normalization, windowing and mel scaling.
//!
//! # Usage
//!
//! ```no_run
//! use std::path::Path;
//! use ve_turn::SmartTurn;
//!
//! # fn main() -> anyhow::Result<()> {
//! let turn = SmartTurn::from_model(Path::new("models/smart-turn-v3.2-cpu.onnx"))?;
//!
//! // One utterance of 16 kHz mono audio; the last 8 s are used, left-padded with zeros.
//! let audio = vec![0.0f32; 3 * 16_000];
//! let p = turn.prob(&audio)?;
//! if p > 0.5 {
//!     // the speaker has finished
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # The model path is a parameter
//!
//! [`SmartTurn::from_model`] takes the ONNX file's path; nothing is hardcoded and nothing is
//! downloaded at runtime.
//!
//! # Read this before adding a tolerance to a Smart Turn assertion
//!
//! The CPU model is int8 dynamically quantised (`78x QuantizeLinear`, `79x
//! DequantizeLinear`), so its output is effectively a **step function** of its input: a
//! 1e-7 perturbation of the features — below the f32 floor of *any* implementation — moves
//! the scalar by ~0.026, and changing only ONNX Runtime's graph-optimization level moves
//! the reference output materially. Assert on the `> 0.5` **decision**, not on the raw
//! probability with a tight tolerance.
//!
//! # Threading
//!
//! [`SmartTurn::from_model`] uses [`DEFAULT_INTRA_THREADS`] (4) intra-op threads. Override with
//! [`SmartTurn::with_intra_threads`].

mod mel;

use anyhow::{ensure, Context, Result};
use mel::{LogMel, CHUNK, N_FRAMES, N_MELS};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

pub use ve_vad::{vad_probs, SileroVad, DEFAULT_INTRA_THREADS};

/// `ort::Session::run` needs `&mut Session`, but this crate's `prob` takes `&self` (so the
/// detector can be shared), hence the interior mutability. `Session` is `Send + Sync`, so
/// the whole type stays `Send + Sync`.
struct Inner {
    session: Session,
    mel: LogMel,
    /// The 8 s window, reused between calls.
    chunk: Vec<f32>,
}

impl Inner {
    /// Truncate to the last 8 s and left-zero-pad, in place.
    ///
    /// Split out as an associated function so the caller can borrow `chunk` and `mel`
    /// disjointly.
    fn prepare_into(chunk: &mut [f32], audio: &[f32]) {
        chunk.fill(0.0);
        let a = if audio.len() > CHUNK {
            &audio[audio.len() - CHUNK..]
        } else {
            audio
        };
        chunk[CHUNK - a.len()..].copy_from_slice(a);
    }
}

/// Smart Turn v3 end-of-turn detector.
pub struct SmartTurn {
    inner: Mutex<Inner>,
    /// Kept so [`SmartTurn::with_intra_threads`] can rebuild the session.
    model_path: PathBuf,
    intra_threads: usize,
}

impl SmartTurn {
    /// Load the Smart Turn ONNX model, using [`DEFAULT_INTRA_THREADS`] intra-op threads.
    pub fn from_model(path: &Path) -> Result<Self> {
        Self::from_model_with_threads(path, DEFAULT_INTRA_THREADS)
    }

    /// Load the Smart Turn ONNX model with an explicit intra-op thread count.
    pub fn from_model_with_threads(path: &Path, intra_threads: usize) -> Result<Self> {
        let session = Self::build_session(path, intra_threads)?;
        Ok(Self {
            inner: Mutex::new(Inner {
                session,
                mel: LogMel::new(),
                chunk: vec![0.0f32; CHUNK],
            }),
            model_path: path.to_path_buf(),
            intra_threads,
        })
    }

    fn build_session(path: &Path, intra_threads: usize) -> Result<Session> {
        let env = ve_vad::ort_support::shared_env()?;
        Session::builder(&env)
            .map_err(ve_vad::ort_support::err)?
            .with_intra_threads(intra_threads)
            .map_err(ve_vad::ort_support::err)?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(ve_vad::ort_support::err)?
            .commit_from_file(path)
            .map_err(ve_vad::ort_support::err)
    }

    /// Rebuild the ONNX Runtime session with `intra_threads` intra-op threads.
    ///
    /// The model path is reused. There is no cross-call state to preserve, so the rebuild
    /// is lossless; thread count does not change the model's numerics.
    pub fn with_intra_threads(self, intra_threads: usize) -> Result<Self> {
        Self::from_model_with_threads(&self.model_path, intra_threads)
    }

    /// The intra-op thread count this session was built with.
    pub fn intra_threads(&self) -> usize {
        self.intra_threads
    }

    /// Probability that the turn is complete; `> 0.5` means the speaker has finished.
    ///
    /// Takes the utterance audio at 16 kHz, keeps its last 8 s and left-pads with zeros,
    /// then runs the Whisper log-mel front end and the model. See the module docs about
    /// the int8 quantisation before asserting on the returned value numerically: the
    /// model returns a raw logit which the reference compares to `0.5` with no sigmoid.
    pub fn prob(&self, audio_16k: &[f32]) -> Result<f32> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let feats = prepare_features(&mut inner.mel, &mut inner.chunk, audio_16k);
        Self::run(&mut inner.session, feats)
    }

    /// Score a pre-computed `(N_MELS, N_FRAMES)` feature vector, as produced by
    /// [`log_mel_whisper`]. Useful for bisecting feature-vs-model differences.
    pub fn prob_from_features(&self, features: Vec<f32>) -> Result<f32> {
        Self::run(&mut self.lock().session, features)
    }

    fn run(session: &mut Session, features: Vec<f32>) -> Result<f32> {
        ensure!(
            features.len() == N_MELS * N_FRAMES,
            "expected a (1, {N_MELS}, {N_FRAMES}) feature vector, got {} values",
            features.len()
        );
        let input = Tensor::from_array(([1usize, N_MELS, N_FRAMES], features))?;
        let outputs = session.run(ort::inputs!["input_features" => input])?;
        let output = outputs
            .get("logits")
            .map(|value| {
                value
                    .try_extract_tensor::<f32>()
                    .map(|(shape, values)| (&shape[..], values))
                    .context("Smart Turn output 'logits' must be an f32 tensor")
            })
            .transpose()?;
        checked_logit(output)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A poisoned lock only means some other thread panicked while running the model;
        // the session itself is still usable.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// Shared by production scoring and model-free checks: prepare the retained input
// window and extract features using the same reusable buffers, without a session.
fn prepare_features(mel: &mut LogMel, chunk: &mut [f32], audio: &[f32]) -> Vec<f32> {
    Inner::prepare_into(chunk, audio);
    mel.extract(chunk)
}

fn checked_logit(output: Option<(&[i64], &[f32])>) -> Result<f32> {
    let (shape, logits) = output.context("Smart Turn is missing named output 'logits'")?;
    let logit = *logits
        .first()
        .context("Smart Turn output 'logits' is empty")?;
    ensure!(
        logits.len() == 1 && shape.iter().all(|&size| size == 1),
        "Smart Turn output 'logits' must contain one scalar score"
    );
    ensure!(
        logit.is_finite(),
        "Smart Turn output 'logits' must contain a finite score"
    );
    Ok(logit)
}

/// Hand-written Whisper log-mel front end, exposed for testing and parity assertions.
///
/// Keeps the last 8 s of `audio_16k`, left-pads with zeros to exactly 8 s, and returns the
/// normalised `(80, 800)` spectrogram flattened row-major — i.e. `[mel * 800 + frame]`,
/// which is the `(1, 80, 800)` tensor the model wants. Always returns exactly
/// `80 * 800 = 64 000` values.
pub fn log_mel_whisper(audio_16k: &[f32]) -> Result<Vec<f32>> {
    let mut mel = LogMel::new();
    let mut chunk = vec![0.0f32; CHUNK];
    Ok(prepare_features(&mut mel, &mut chunk, audio_16k))
}

#[cfg(test)]
mod output_tests {
    use super::*;

    #[test]
    fn production_preparation_retains_the_tail_and_reuses_a_clean_chunk() {
        let exact: Vec<f32> = (0..CHUNK).map(|i| (i % 257 + 1) as f32 / 258.0).collect();
        let mut oversized = vec![-0.9; 17];
        oversized.extend_from_slice(&exact);
        let short = [0.25, -0.5, 0.75, 0.125];
        let mut chunk = vec![9.0; CHUNK];
        let address = chunk.as_ptr();
        let mut mel = LogMel::with_threads(1);

        let tail_features = prepare_features(&mut mel, &mut chunk, &oversized);
        assert_eq!(
            chunk, exact,
            "only the final eight seconds may reach the frontend"
        );
        assert_eq!(tail_features.len(), N_MELS * N_FRAMES);
        assert_eq!(chunk.as_ptr(), address);

        let exact_features = prepare_features(&mut mel, &mut chunk, &exact);
        assert_eq!(chunk, exact);
        assert_eq!(
            exact_features, tail_features,
            "equivalent retained windows must produce identical features"
        );
        assert_eq!(chunk.as_ptr(), address);

        let short_features = prepare_features(&mut mel, &mut chunk, &short);
        assert_eq!(short_features.len(), N_MELS * N_FRAMES);
        assert!(chunk[..CHUNK - short.len()]
            .iter()
            .all(|&sample| sample == 0.0));
        assert_eq!(&chunk[CHUNK - short.len()..], &short);
        assert_eq!(chunk.as_ptr(), address);

        for _ in 0..2 {
            let silence = prepare_features(&mut mel, &mut chunk, &[]);
            assert_eq!(silence.len(), N_MELS * N_FRAMES);
            assert!(
                chunk.iter().all(|&sample| sample == 0.0),
                "empty input must clear the previous window"
            );
            assert!(silence.iter().all(|&feature| feature == -1.5));
            assert_eq!(chunk.as_ptr(), address);
            assert_eq!(
                prepare_features(&mut mel, &mut chunk, &exact),
                exact_features
            );
            assert_eq!(
                prepare_features(&mut mel, &mut chunk, &short),
                short_features
            );
            assert!(
                chunk[..CHUNK - short.len()]
                    .iter()
                    .all(|&sample| sample == 0.0),
                "short input retained stale samples after buffer reuse"
            );
            assert_eq!(&chunk[CHUNK - short.len()..], &short);
            assert_eq!(chunk.as_ptr(), address);
        }
    }

    #[test]
    fn incompatible_logit_outputs_return_errors_not_panics() {
        for output in [
            None,
            Some((&[1, 0][..], &[][..])),
            Some((&[1, 2][..], &[0.25, 0.5][..])),
            Some((&[1, 1][..], &[f32::NAN][..])),
        ] {
            let result = std::panic::catch_unwind(|| checked_logit(output));
            assert!(result.is_ok(), "incompatible logits panicked");
            assert!(result.unwrap().unwrap_err().to_string().contains("logits"));
        }
    }

    #[test]
    fn valid_logits_preserve_the_raw_score_without_a_sigmoid() {
        for score in [-2.0, 0.5, 3.0] {
            assert_eq!(checked_logit(Some((&[1, 1], &[score]))).unwrap(), score);
        }
    }
}
