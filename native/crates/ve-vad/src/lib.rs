//! Silero VAD (ONNX Runtime), including recurrent state and 64-sample context.
//!
//! # Usage
//!
//! ```no_run
//! use std::path::Path;
//! use ve_vad::{vad_probs, SileroVad};
//!
//! # fn main() -> anyhow::Result<()> {
//! let mut vad = SileroVad::from_model(Path::new("models/silero_vad.onnx"))?;
//!
//! // Streaming: exactly 512 mono f32 samples at 16 kHz (32 ms) per call.
//! let frame = vec![0.0f32; 512];
//! let p = vad.prob(&frame)?;
//! assert!((0.0..=1.0).contains(&p));
//!
//! // Or batch-wise, over a whole utterance.
//! let _probs = vad_probs(&mut vad, &vec![0.0f32; 16_000])?;
//! # Ok(())
//! # }
//! ```
//!
//! # The model path is a parameter
//!
//! [`SileroVad::from_model`] takes the ONNX file's path; nothing is hardcoded and nothing
//! is downloaded at runtime.
//!
//! # Threading
//!
//! [`SileroVad::from_model`] uses [`DEFAULT_INTRA_THREADS`] (4) intra-op threads.
//!
//! # The `ort` feature-set trap
//!
//! Never inherit `ort`'s default features for this workspace. The manifest explicitly
//! selects API 17 and a TLS feature for binary downloads. API/optimization changes
//! can change the turn model's numerical decisions.

use anyhow::{ensure, Context, Result};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use std::path::{Path, PathBuf};

/// Mono f32 samples consumed per [`SileroVad::prob`] call (32 ms at 16 kHz).
pub const FRAME: usize = 512;
/// Samples of the *previous* frame carried over as context (Silero's 64-sample tail).
pub const CTX: usize = 64;
/// Sample rate reported to the model. The engine only ever uses 16 kHz.
pub const SAMPLE_RATE: i64 = 16_000;
/// Recurrent state size of the model: `(2, 1, 128)` f32.
const STATE_LEN: usize = 2 * 128;
/// Intra-op thread count used by `from_model` in both model crates.
pub const DEFAULT_INTRA_THREADS: usize = 4;

/// ONNX Runtime plumbing shared by `ve-vad` and (through it) `ve-turn`.
///
/// This is an implementation detail of the model crates, not part of their public API.
#[doc(hidden)]
pub mod ort_support {
    use anyhow::Result;
    use ort::environment::Environment;
    use std::sync::Mutex;

    static ENV: Mutex<Option<Environment>> = Mutex::new(None);

    /// Process-wide ONNX Runtime [`Environment`].
    ///
    /// ONNX Runtime permits exactly one environment per process — `ort`'s builder returns
    /// *"only one environment is allowed per process"* on a second `build()`, and it does
    /// not expose the environment globally — so the engine creates one here, lazily, and
    /// hands out cheap clones.
    pub fn shared_env() -> Result<Environment> {
        // A poisoned lock only means some other thread panicked while *reading* the
        // environment; the value itself is still perfectly usable.
        let mut guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(env) = guard.as_ref() {
            return Ok(env.clone());
        }
        let env = ort::init().with_name("opencode-converse").build()?;
        *guard = Some(env.clone());
        Ok(env)
    }

    /// Converts an `ort` error into `anyhow`, via its message.
    ///
    /// `ort::Error<R>` is generic over the value that failed, and its `Send`/`Sync`ness
    /// follows `R`'s. `ort::session::builder::SessionBuilder` is *not* `Send + Sync`, so a
    /// bare `?` on the session-builder chain cannot convert into `anyhow::Error`; going
    /// through the message can.
    pub fn err(e: impl std::fmt::Display) -> anyhow::Error {
        anyhow::anyhow!("{e}")
    }
}

/// Silero VAD, holding one ONNX Runtime session plus the model's recurrent state.
pub struct SileroVad {
    session: Session,
    /// Kept so [`SileroVad::with_intra_threads`] can rebuild the session.
    model_path: PathBuf,
    intra_threads: usize,
    state: Vec<f32>,
    ctx: Vec<f32>,
}

impl SileroVad {
    /// Load the Silero VAD ONNX model, using [`DEFAULT_INTRA_THREADS`] intra-op threads.
    pub fn from_model(path: &Path) -> Result<Self> {
        Self::from_model_with_threads(path, DEFAULT_INTRA_THREADS)
    }

    /// Load the Silero VAD ONNX model with an explicit intra-op thread count.
    pub fn from_model_with_threads(path: &Path, intra_threads: usize) -> Result<Self> {
        let env = ort_support::shared_env()?;
        let session = Session::builder(&env)
            .map_err(ort_support::err)?
            .with_intra_threads(intra_threads)
            .map_err(ort_support::err)?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(ort_support::err)?
            .commit_from_file(path)?;
        Ok(Self {
            session,
            model_path: path.to_path_buf(),
            intra_threads,
            state: vec![0.0; STATE_LEN],
            ctx: vec![0.0; CTX],
        })
    }

    /// Rebuild the ONNX Runtime session with `intra_threads` intra-op threads.
    ///
    /// The model path is reused and the recurrent state is carried over, so this is safe
    /// to call mid-stream. Thread count does not change the model's numerics.
    pub fn with_intra_threads(self, intra_threads: usize) -> Result<Self> {
        let mut rebuilt = Self::from_model_with_threads(&self.model_path, intra_threads)?;
        rebuilt.state.copy_from_slice(&self.state);
        rebuilt.ctx.copy_from_slice(&self.ctx);
        Ok(rebuilt)
    }

    /// The intra-op thread count this session was built with.
    pub fn intra_threads(&self) -> usize {
        self.intra_threads
    }

    /// Feed exactly 512 mono f32 samples at 16 kHz; returns speech probability `0..=1`.
    ///
    /// Per call, exactly as the Python reference does:
    ///
    /// ```text
    /// x     = concat(ctx[64], frame[512])          // (1, 576)
    /// out   = model(input=x, state=(2,1,128), sr=16000)
    /// prob  = out.output[0]
    /// state = out.stateN
    /// ctx   = frame[-64:]
    /// ```
    pub fn prob(&mut self, frame_16k_512: &[f32]) -> Result<f32> {
        ensure!(
            frame_16k_512.len() == FRAME,
            "Silero VAD expects exactly {FRAME} mono f32 samples per frame, got {}",
            frame_16k_512.len()
        );

        let mut x = Vec::with_capacity(CTX + FRAME);
        x.extend_from_slice(&self.ctx);
        x.extend_from_slice(frame_16k_512);

        let input = Tensor::from_array(([1usize, CTX + FRAME], x))?;
        let state = Tensor::from_array(([2usize, 1, 128], self.state.clone()))?;
        let sr = Tensor::from_array(((), vec![SAMPLE_RATE]))?;

        let outputs = self.session.run(ort::inputs![
            "input" => input,
            "state" => state,
            "sr"    => sr,
        ])?;

        commit_outputs(&mut self.state, &mut self.ctx, frame_16k_512, |name| {
            outputs
                .get(name)
                .map(|value| {
                    value
                        .try_extract_tensor::<f32>()
                        .map(|(shape, values)| (&shape[..], values))
                        .with_context(|| {
                            format!("Silero VAD output '{name}' must be an f32 tensor")
                        })
                })
                .transpose()
        })
    }

    /// Drop the recurrent state (`state` and the 64-sample context), as `Listener.reset()`
    /// does in the reference implementation.
    pub fn reset(&mut self) {
        self.state.fill(0.0);
        self.ctx.fill(0.0);
    }
}

// Tensor views keep output validation model-free and make the state commit one
// operation after every required named output has been checked.
fn commit_outputs<'a>(
    state: &mut Vec<f32>,
    ctx: &mut [f32],
    frame: &[f32],
    mut output: impl FnMut(&str) -> Result<Option<(&'a [i64], &'a [f32])>>,
) -> Result<f32> {
    let (state_shape, state_n) =
        output("stateN")?.context("Silero VAD is missing named output 'stateN'")?;
    let (probability_shape, probability_values) =
        output("output")?.context("Silero VAD is missing named output 'output'")?;
    let probability = *probability_values
        .first()
        .context("Silero VAD output 'output' is empty")?;
    ensure!(
        state_shape == [2, 1, 128] && state_n.len() == STATE_LEN,
        "Silero VAD output 'stateN' must have shape [2, 1, 128] and {STATE_LEN} values"
    );
    ensure!(
        state_n.iter().all(|value| value.is_finite()),
        "Silero VAD output 'stateN' must contain only finite values"
    );
    ensure!(
        probability_values.len() == 1 && probability_shape.iter().all(|&size| size == 1),
        "Silero VAD output 'output' must contain one scalar probability"
    );
    ensure!(
        probability.is_finite() && (0.0..=1.0).contains(&probability),
        "Silero VAD output 'output' must be a finite probability in 0..=1"
    );
    // Nothing above mutates either recurrent buffer. All model outputs must be
    // valid before committing the new state and the input frame's context tail.
    state.clear();
    state.extend_from_slice(state_n);
    ctx.copy_from_slice(&frame[FRAME - CTX..]);
    Ok(probability)
}

/// Run Silero VAD over a raw 16 kHz mono buffer, one probability per 512-sample frame.
///
/// `vad` is [`reset`](SileroVad::reset) first, so the result depends only on `audio_16k`
/// and repeated calls on the same `vad` are reproducible. For streaming use
/// [`SileroVad::prob`] directly instead.
///
/// Windowing matches the Python reference driver exactly — frames start at 0, 512, 1024,
/// … and a trailing partial frame is dropped (`while i + FRAME < len`). A buffer of `n`
/// samples therefore yields `floor((n - 1) / FRAME)` probabilities; anything shorter than
/// 513 samples yields none.
pub fn vad_probs(vad: &mut SileroVad, audio_16k: &[f32]) -> Result<Vec<f32>> {
    vad.reset();
    let mut probs = Vec::with_capacity(audio_16k.len() / FRAME);
    let mut i = 0;
    while i + FRAME < audio_16k.len() {
        probs.push(vad.prob(&audio_16k[i..i + FRAME])?);
        i += FRAME;
    }
    Ok(probs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejected_without_mutation(
        state_output: Option<(&[i64], &[f32])>,
        probability: Option<(&[i64], &[f32])>,
        expected_error: &str,
    ) {
        let mut state = vec![0.125; STATE_LEN];
        let mut ctx = vec![-0.25; CTX];
        let original_state = state.clone();
        let original_ctx = ctx.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            commit_outputs(&mut state, &mut ctx, &[0.75; FRAME], |name| {
                Ok(match name {
                    "stateN" => state_output,
                    "output" => probability,
                    _ => None,
                })
            })
        }));
        assert!(
            result.is_ok(),
            "incompatible output {expected_error} panicked"
        );
        let error = result
            .unwrap()
            .expect_err("incompatible outputs must return an error");
        assert!(error.to_string().contains(expected_error), "{error}");
        assert_eq!(
            state, original_state,
            "invalid output partially changed recurrent state"
        );
        assert_eq!(ctx, original_ctx, "invalid output changed capture context");
    }

    #[test]
    fn missing_named_outputs_are_errors_without_state_mutation() {
        rejected_without_mutation(None, Some((&[1, 1], &[0.5])), "stateN");
        rejected_without_mutation(Some((&[2, 1, 128], &[0.5; STATE_LEN])), None, "output");
    }

    #[test]
    fn empty_probability_is_an_error_without_state_mutation() {
        rejected_without_mutation(
            Some((&[2, 1, 128], &[0.5; STATE_LEN])),
            Some((&[1, 0], &[])),
            "output",
        );
    }

    #[test]
    fn invalid_shapes_are_errors_without_state_mutation() {
        rejected_without_mutation(Some((&[2, 1, 0], &[])), Some((&[1, 1], &[0.5])), "stateN");
        rejected_without_mutation(
            Some((&[1, 2, 128], &[0.5; STATE_LEN])),
            Some((&[1, 1], &[0.5])),
            "stateN",
        );
        rejected_without_mutation(
            Some((&[2, 1, 128], &[0.5; STATE_LEN - 1])),
            Some((&[1, 1], &[0.5])),
            "stateN",
        );
        rejected_without_mutation(
            Some((&[2, 1, 128], &[0.5; STATE_LEN])),
            Some((&[1, 2], &[0.25, 0.5])),
            "output",
        );
    }

    #[test]
    fn every_nonfinite_recurrent_element_is_rejected_without_mutation() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            for index in 0..STATE_LEN {
                let mut next_state = [0.5; STATE_LEN];
                next_state[index] = bad;
                rejected_without_mutation(
                    Some((&[2, 1, 128], &next_state)),
                    Some((&[1, 1], &[0.625])),
                    "stateN",
                );
            }
        }
    }

    #[test]
    fn invalid_probability_values_do_not_commit_recurrent_state() {
        for probability in [f32::NAN, f32::INFINITY, -0.25, 1.25] {
            rejected_without_mutation(
                Some((&[2, 1, 128], &[0.5; STATE_LEN])),
                Some((&[1, 1], &[probability])),
                "output",
            );
        }
    }

    #[test]
    fn extraction_error_does_not_commit_recurrent_state() {
        let mut state = vec![0.125; STATE_LEN];
        let mut ctx = vec![-0.25; CTX];
        let result = commit_outputs(&mut state, &mut ctx, &[0.75; FRAME], |name| {
            if name == "stateN" {
                Ok(Some((&[2, 1, 128], &[0.5; STATE_LEN])))
            } else {
                anyhow::bail!("output must be an f32 tensor")
            }
        });
        assert!(result.is_err());
        assert_eq!(state, [0.125; STATE_LEN]);
        assert_eq!(ctx, [-0.25; CTX]);
    }

    #[test]
    fn valid_outputs_commit_all_state_and_context() {
        let mut state = vec![0.0; STATE_LEN];
        let mut ctx = vec![0.0; CTX];
        let mut frame = vec![0.25; FRAME];
        frame[FRAME - CTX..].fill(-0.75);
        let probability = commit_outputs(&mut state, &mut ctx, &frame, |name| {
            Ok(match name {
                "stateN" => Some((&[2, 1, 128][..], &[0.5; STATE_LEN][..])),
                "output" => Some((&[1, 1][..], &[0.625][..])),
                _ => None,
            })
        })
        .unwrap();
        assert_eq!(probability, 0.625);
        assert_eq!(state, [0.5; STATE_LEN]);
        assert_eq!(ctx, [-0.75; CTX]);
    }
}
