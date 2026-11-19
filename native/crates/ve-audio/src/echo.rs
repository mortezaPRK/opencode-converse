//! Acoustic echo cancellation (WebRTC AEC3) between the output callback and capture.
//!
//! The reference signal is the exact PCM the output callback hands to the device,
//! mono, at the device rate: post-resample, post-pause/flush silence, and zeros
//! when starved. The callback only pushes those samples into a bounded `rtrb` ring
//! ([`tap_block`]); everything else happens on the capture worker thread:
//!
//! ```text
//! output callback ──► ring (device rate) ──► resample 16 kHz ──► RefAligner ─┐
//! capture worker: mic 16 kHz ────────────────────────────────────────────────┴► AEC3 ──► VAD/ASR
//! ```
//!
//! Capture and playback are separate cpal streams on independent callbacks, so the
//! worker keeps the two streams on a common time axis: one 10 ms reference frame is
//! consumed per 10 ms capture frame. A starved reference is zero-padded and the
//! missing amount is skipped when the late samples arrive (so later audio stays
//! aligned); a reference that runs far ahead is trimmed. AEC3 estimates the
//! remaining delay (output latency + acoustic path + input latency) itself.
//!
//! The worker never pads a capture block to a frame: samples that do not yet fill
//! a 10 ms frame wait for the next call.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use webrtc_audio_processing::config::{Config, EchoCanceller as Aec3, HighPassFilter};
use webrtc_audio_processing::Processor;

use crate::resample::StreamResampler;

/// AEC3 runs at the pipeline's 16 kHz capture rate; ASR and VAD need nothing more.
pub const AEC_RATE: u32 = 16_000;
/// WebRTC processes fixed 10 ms frames.
pub const AEC_FRAME: usize = AEC_RATE as usize / 100;
/// The reference may run at most this far ahead of the capture before it is trimmed.
const MAX_REF_BACKLOG: usize = AEC_RATE as usize / 10;

/// Reference ring capacity in seconds of device-rate audio. Bounded; when nothing
/// reads it (microphone muted) the callback's pushes fail harmlessly.
const TAP_SECONDS: usize = 1;

/// Consumer end of the playback tap. Cloneable so a capture stream can be reopened
/// (unmute) while the output keeps feeding the same ring.
#[derive(Clone)]
pub struct EchoReference {
    ring: Arc<Mutex<rtrb::Consumer<f32>>>,
    rate: u32,
}

impl EchoReference {
    pub(crate) fn pair(rate: u32) -> (rtrb::Producer<f32>, Self) {
        let (producer, consumer) =
            rtrb::RingBuffer::<f32>::new((rate as usize * TAP_SECONDS).max(AEC_FRAME));
        (
            producer,
            Self {
                ring: Arc::new(Mutex::new(consumer)),
                rate,
            },
        )
    }

    /// Moves everything currently queued into `out`; locks only on a worker thread.
    fn read_all(&self, out: &mut Vec<f32>) -> Result<()> {
        let mut ring = self
            .ring
            .lock()
            .map_err(|_| anyhow!("Echo reference lock poisoned"))?;
        let available = ring.slots();
        if available == 0 {
            return Ok(());
        }
        let chunk = ring
            .read_chunk(available)
            .map_err(|e| anyhow!("Echo reference read failed: {e}"))?;
        let (first, second) = chunk.as_slices();
        out.extend_from_slice(first);
        out.extend_from_slice(second);
        chunk.commit_all();
        Ok(())
    }

    fn discard(&self) -> Result<()> {
        let mut sink = Vec::new();
        self.read_all(&mut sink)
    }
}

/// Audio callback side: publish the first channel of what was just written to the
/// device (all channels carry the same mono sample). No allocation, no locking.
pub(crate) fn tap_block(producer: &mut rtrb::Producer<f32>, data: &[f32], channels: usize) {
    for frame in data.chunks_exact(channels) {
        if producer.push(frame[0]).is_err() {
            break;
        }
    }
}

/// Keeps playback reference samples on the capture time axis.
#[derive(Debug, Default)]
pub(crate) struct RefAligner {
    queue: VecDeque<f32>,
    /// Reference samples that were missing for an earlier frame and must be skipped
    /// when they arrive, so subsequent audio stays aligned.
    debt: usize,
}

impl RefAligner {
    pub(crate) fn push(&mut self, samples: &[f32]) {
        let skip = self.debt.min(samples.len());
        self.debt -= skip;
        self.queue.extend(&samples[skip..]);
        if self.queue.len() > MAX_REF_BACKLOG {
            let excess = self.queue.len() - MAX_REF_BACKLOG;
            self.queue.drain(..excess);
        }
    }

    /// Fills `out` with the next reference samples; any shortfall is silence.
    pub(crate) fn take(&mut self, out: &mut [f32]) {
        let have = self.queue.len().min(out.len());
        for (slot, sample) in out.iter_mut().zip(self.queue.drain(..have)) {
            *slot = sample;
        }
        out[have..].fill(0.0);
        self.debt += out.len() - have;
    }

    pub(crate) fn reset(&mut self) {
        self.queue.clear();
        self.debt = 0;
    }

    #[cfg(test)]
    fn backlog(&self) -> usize {
        self.queue.len()
    }
}

/// WebRTC AEC3 on 10 ms, 16 kHz mono frames with a caller-supplied aligned reference.
///
/// AEC3's own delay estimator is used (no external stream delay). The high-pass
/// filter is enabled as WebRTC recommends with AEC; no other denoiser is stacked.
pub struct EchoCanceller {
    processor: Processor,
}

impl EchoCanceller {
    pub fn new() -> Result<Self> {
        let processor = Processor::new(AEC_RATE)
            .map_err(|e| anyhow!("Cannot create the echo canceller: {e}"))?;
        processor.set_config(Config {
            echo_canceller: Some(Aec3::Full {
                stream_delay_ms: None,
            }),
            high_pass_filter: Some(HighPassFilter::default()),
            ..Default::default()
        });
        Ok(Self { processor })
    }

    /// Removes the echo of `reference` from `capture` in place. Both must be exactly
    /// one [`AEC_FRAME`] and time-aligned.
    pub fn process_frame(&mut self, reference: &[f32], capture: &mut [f32]) -> Result<()> {
        anyhow::ensure!(
            reference.len() == AEC_FRAME && capture.len() == AEC_FRAME,
            "Echo cancellation frames must be {AEC_FRAME} samples"
        );
        self.processor
            .analyze_render_frame([reference])
            .map_err(|e| anyhow!("{e}"))?;
        self.processor
            .process_capture_frame([&mut *capture])
            .map_err(|e| anyhow!("{e}"))
    }
}

/// Capture-worker stage: reference ring → 16 kHz → aligner, and 10 ms framing.
pub(crate) struct EchoStage {
    canceller: EchoCanceller,
    reference: EchoReference,
    resampler: StreamResampler,
    aligner: RefAligner,
    raw: Vec<f32>,
    pending: Vec<f32>,
    ref_frame: Vec<f32>,
}

impl EchoStage {
    pub(crate) fn new(reference: EchoReference) -> Result<Self> {
        // Whatever the output produced while the microphone was closed is stale.
        reference.discard()?;
        Ok(Self {
            canceller: EchoCanceller::new()?,
            resampler: StreamResampler::new(reference.rate, AEC_RATE)?,
            reference,
            aligner: RefAligner::default(),
            raw: Vec::new(),
            pending: Vec::with_capacity(AEC_FRAME * 4),
            ref_frame: vec![0.0; AEC_FRAME],
        })
    }

    /// Appends the echo-cancelled samples of every complete 10 ms frame to `out`.
    /// A remainder shorter than a frame is kept for the next call (never padded).
    pub(crate) fn process(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<()> {
        self.pending.extend_from_slice(input);
        let mut done = 0;
        while self.pending.len() - done >= AEC_FRAME {
            self.raw.clear();
            self.reference.read_all(&mut self.raw)?;
            if !self.raw.is_empty() {
                let reference = self.resampler.process(&self.raw);
                self.aligner.push(&reference);
            }
            self.aligner.take(&mut self.ref_frame);
            let frame = &mut self.pending[done..done + AEC_FRAME];
            self.canceller.process_frame(&self.ref_frame, frame)?;
            out.extend_from_slice(frame);
            done += AEC_FRAME;
        }
        self.pending.drain(..done);
        Ok(())
    }

    /// Capture continuity was lost: drop partial frames and realign the reference.
    /// The adaptive filter stays warm; AEC3 re-estimates the delay.
    pub(crate) fn resync(&mut self) -> Result<()> {
        self.pending.clear();
        self.aligner.reset();
        self.resampler.reset();
        self.reference.discard()
    }

    /// Samples held back because they did not fill a frame (end of stream only).
    pub(crate) fn take_remainder(&mut self) -> Vec<f32> {
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_publishes_first_channel_and_drops_when_full() {
        let (mut producer, reference) = EchoReference::pair(AEC_FRAME as u32);
        tap_block(&mut producer, &[0.1, 0.1, 0.2, 0.2, 0.3, 0.3], 2);
        let mut out = Vec::new();
        reference.read_all(&mut out).unwrap();
        assert_eq!(out, [0.1, 0.2, 0.3]);
        // Bounded: a full ring discards instead of blocking or allocating.
        tap_block(&mut producer, &vec![0.5; AEC_FRAME * 3], 1);
        reference.read_all(&mut out).unwrap();
        assert_eq!(out.len(), 3 + AEC_FRAME);
    }

    #[test]
    fn aligner_zero_pads_a_short_reference_and_skips_it_when_it_arrives() {
        let mut aligner = RefAligner::default();
        let mut frame = [9.0f32; 4];
        aligner.push(&[1.0, 2.0]);
        aligner.take(&mut frame);
        assert_eq!(frame, [1.0, 2.0, 0.0, 0.0]);
        // The two missing samples belong to the past; sample 3 lines up with the next frame.
        aligner.push(&[7.0, 8.0, 3.0, 4.0, 5.0, 6.0]);
        aligner.take(&mut frame);
        assert_eq!(frame, [3.0, 4.0, 5.0, 6.0]);
        assert_eq!(aligner.backlog(), 0);
    }

    #[test]
    fn aligner_trims_a_reference_that_runs_far_ahead() {
        let mut aligner = RefAligner::default();
        let samples: Vec<f32> = (0..MAX_REF_BACKLOG * 2).map(|i| i as f32).collect();
        aligner.push(&samples);
        assert_eq!(aligner.backlog(), MAX_REF_BACKLOG);
        let mut frame = [0.0f32; 2];
        aligner.take(&mut frame);
        assert_eq!(
            frame[0], MAX_REF_BACKLOG as f32,
            "oldest samples are trimmed"
        );
    }

    #[test]
    fn aligner_reset_forgets_debt_and_queue() {
        let mut aligner = RefAligner::default();
        let mut frame = [0.0f32; 4];
        aligner.take(&mut frame);
        aligner.push(&[1.0; 2]);
        aligner.reset();
        aligner.push(&[5.0, 6.0]);
        aligner.take(&mut frame[..2]);
        assert_eq!(frame[..2], [5.0, 6.0]);
    }

    fn stage(rate: u32) -> (rtrb::Producer<f32>, EchoStage) {
        let (producer, reference) = EchoReference::pair(rate);
        (producer, EchoStage::new(reference).unwrap())
    }

    #[test]
    fn frames_are_ten_ms_and_partial_input_is_never_padded() {
        let (_tap, mut stage) = stage(AEC_RATE);
        let mut out = Vec::new();
        stage.process(&vec![0.0; AEC_FRAME - 1], &mut out).unwrap();
        assert!(out.is_empty(), "less than 10 ms must wait for more input");
        stage.process(&[0.0; 1], &mut out).unwrap();
        assert_eq!(out.len(), AEC_FRAME);
        stage
            .process(&vec![0.0; AEC_FRAME * 2 + 37], &mut out)
            .unwrap();
        assert_eq!(out.len(), AEC_FRAME * 3);
        assert_eq!(stage.take_remainder().len(), 37);
    }

    #[test]
    fn stale_reference_from_a_closed_microphone_is_discarded_at_start() {
        let (mut tap, reference) = EchoReference::pair(AEC_RATE);
        tap_block(&mut tap, &[0.9; 400], 1);
        let mut stage = EchoStage::new(reference.clone()).unwrap();
        let mut left = Vec::new();
        reference.read_all(&mut left).unwrap();
        assert!(left.is_empty());
        stage.resync().unwrap();
    }

    #[test]
    fn silent_reference_and_silent_capture_stay_silent() {
        let (mut tap, mut stage) = stage(48_000);
        let mut out = Vec::new();
        for _ in 0..20 {
            tap_block(&mut tap, &[0.0; 480], 1);
            stage.process(&[0.0; AEC_FRAME], &mut out).unwrap();
        }
        assert_eq!(out.len(), AEC_FRAME * 20);
        assert!(out.iter().all(|s| s.abs() < 1e-4));
    }

    #[test]
    fn stage_cancels_a_delayed_copy_of_the_played_reference_from_the_ring() {
        const DELAY: usize = AEC_RATE as usize * 60 / 1000;
        let mut seed = 12345u64;
        let mut noise = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) as f32 / (1u64 << 31) as f32 - 0.5) * 0.4
        };
        // 48 kHz "played" PCM; the echo is its 16 kHz rendition, 60 ms late and at -6 dB.
        let seconds = 8;
        // Syllable-like bursts (4 Hz envelope, 0.3 s pauses): stationary noise would be
        // treated as background by AEC3 and never adapts like it does to speech.
        let played: Vec<f32> = (0..48_000 * seconds)
            .map(|n| {
                let t = n as f32 / 48_000.0;
                let burst = (t * std::f32::consts::TAU * 4.0).sin().abs();
                let gate = if (t % 1.5) < 1.2 { 1.0 } else { 0.0 };
                noise() * burst * gate * 2.0
            })
            .collect();
        let mut down = StreamResampler::new(48_000, AEC_RATE).unwrap();
        let mut reference16 = down.process(&played);
        reference16.extend(down.flush());
        let mic: Vec<f32> = (0..reference16.len())
            .map(|n| {
                if n >= DELAY {
                    reference16[n - DELAY] * 0.5
                } else {
                    0.0
                }
            })
            .collect();

        let (mut tap, mut stage) = stage(48_000);
        let mut out = Vec::new();
        for (played, capture) in played.chunks(480).zip(mic.chunks(AEC_FRAME)) {
            tap_block(&mut tap, played, 1);
            stage.process(capture, &mut out).unwrap();
        }
        let energy = |x: &[f32]| x.iter().map(|s| s * s).sum::<f32>();
        let tail = out.len() - AEC_RATE as usize * 2..;
        let erle = 10.0 * (energy(&mic[tail.clone()]) / energy(&out[tail]).max(1e-12)).log10();
        assert!(erle > 20.0, "echo only reduced by {erle:.1} dB");
    }

    #[test]
    fn canceller_rejects_wrong_frame_sizes() {
        let mut aec = EchoCanceller::new().unwrap();
        let mut frame = vec![0.0; AEC_FRAME - 1];
        assert!(aec.process_frame(&frame.clone(), &mut frame).is_err());
    }
}
