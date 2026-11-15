//! Streaming, stateful sample-rate conversion.
//!
//! The device gives us arbitrary block sizes at its own clock, so a batch
//! `process_all`-style API is unusable on the live path. [`StreamResampler`] buffers
//! input internally and runs rubato's FFT resampler one fixed chunk at a time, which
//! makes it usable both by the capture worker (48 kHz → 16 kHz) and by `ve-tts`
//! (24 kHz → 48 kHz) over a long-running stream.
//!
//! It is deliberately *not* allocation-free: it is called from worker threads, never
//! from an audio callback, so it may allocate per call. The audio callbacks only push
//! and pop raw frames.
//!
//! The resampler has a fixed internal latency (the chunk plus half an FFT block of
//! group delay — about 15 ms for 48 kHz → 16 kHz). Initial delay samples are discarded
//! and flush recovers the signal tail, preserving alignment and duration.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

/// Internal chunk target: about 10 ms of input audio.
const CHUNK_MS_DIVISOR: u32 = 100;

/// A stateful resampler fed one arbitrary-length slice at a time.
pub struct StreamResampler {
    inner: Option<Fft<f32>>,
    /// Input frames accepted from the caller but not yet consumed by `inner`.
    pending: Vec<f32>,
    /// Reused output scratch buffer.
    scratch: Vec<f32>,
    /// Total input frames handed in through [`StreamResampler::process`].
    fed: u64,
    /// Total output frames returned so far.
    produced: u64,
    /// Remaining initial filter delay to discard, in output frames.
    delay: usize,
}

impl StreamResampler {
    /// Creates a resampler from `in_rate` to `out_rate`.
    ///
    /// Equal rates are a supported fast path that copies input to output unchanged.
    pub fn new(in_rate: u32, out_rate: u32) -> anyhow::Result<Self> {
        if in_rate == 0 || out_rate == 0 {
            anyhow::bail!("sample rates must be non-zero (got {in_rate} -> {out_rate})");
        }
        let inner = if in_rate == out_rate {
            None
        } else {
            let chunk = (in_rate / CHUNK_MS_DIVISOR).max(32) as usize;
            let rs = Fft::<f32>::new(
                in_rate as usize,
                out_rate as usize,
                chunk,
                1,
                FixedSync::Both,
            )
            .map_err(|e| {
                anyhow::anyhow!("cannot build a {in_rate} -> {out_rate} Hz resampler: {e}")
            })?;
            Some(rs)
        };
        let delay = inner.as_ref().map_or(0, |rs| rs.output_delay());
        Ok(Self {
            inner,
            pending: Vec::new(),
            scratch: Vec::new(),
            fed: 0,
            produced: 0,
            delay,
        })
    }

    /// Resamples `input`, returning whatever output frames are ready.
    ///
    /// Any input that does not yet fill an internal chunk is buffered and used by the
    /// next call, so the caller may pass blocks of any size.
    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let Some(rs) = self.inner.as_mut() else {
            return input.to_vec();
        };
        self.fed += input.len() as u64;
        self.pending.extend_from_slice(input);

        let need = rs.input_frames_next();
        let mut out = Vec::new();
        let mut consumed = 0;
        while self.pending.len() - consumed >= need {
            convert_chunk(
                rs,
                &self.pending[consumed..consumed + need],
                &mut self.scratch,
                &mut out,
                &mut self.delay,
            );
            consumed += need;
        }
        self.pending.drain(..consumed);
        self.produced += out.len() as u64;
        out
    }

    /// Drains the buffered tail and returns the remaining output frames.
    ///
    /// The output of `process` followed by `flush` is `round(input_frames * ratio)`
    /// long. Call it once, when the input stream ends; the resampler is not usable for
    /// further input afterwards without a [`Self::reset`].
    pub fn flush(&mut self) -> Vec<f32> {
        let Some(rs) = self.inner.as_mut() else {
            return Vec::new();
        };
        let expected = (self.fed as f64 * rs.resample_ratio()).round() as u64;
        let mut out = Vec::new();
        if self.produced >= expected {
            self.pending.clear();
            return out;
        }
        // Feed the partial chunk and enough zero chunks to release the filter's delayed
        // tail. Padding does not count as input; truncate only after delay removal.
        let need = rs.input_frames_next();
        let mut padded = vec![0.0f32; need];
        let leftover = self.pending.len().min(need);
        padded[..leftover].copy_from_slice(&self.pending[..leftover]);
        self.pending.clear();
        while self.produced + (out.len() as u64) < expected {
            convert_chunk(rs, &padded, &mut self.scratch, &mut out, &mut self.delay);
            padded.fill(0.0);
        }
        out.truncate((expected - self.produced) as usize);
        self.produced += out.len() as u64;
        out
    }

    /// Drops all buffered state and restarts the stream.
    ///
    /// Used by the playback worker when barge-in discards queued audio, so the
    /// stale input tail cannot leak into the next utterance.
    pub fn reset(&mut self) {
        if let Some(rs) = self.inner.as_mut() {
            rs.reset();
            self.delay = rs.output_delay();
        }
        self.pending.clear();
        self.fed = 0;
        self.produced = 0;
    }
}

/// Runs one fixed-size chunk through the resampler, appending the output to `out`.
fn convert_chunk(
    rs: &mut Fft<f32>,
    input: &[f32],
    scratch: &mut Vec<f32>,
    out: &mut Vec<f32>,
    delay: &mut usize,
) {
    let frames_out = rs.output_frames_next();
    scratch.resize(frames_out, 0.0);
    let in_adapter = InterleavedSlice::new(input, 1, input.len())
        .expect("mono interleaved input adapter over one chunk");
    let mut out_adapter = InterleavedSlice::new_mut(scratch.as_mut_slice(), 1, frames_out)
        .expect("mono interleaved output adapter sized to output_frames_next()");
    rs.process_into_buffer(&in_adapter, &mut out_adapter, None)
        .expect("chunk is exactly input_frames_next() frames long");
    let skip = (*delay).min(frames_out);
    *delay -= skip;
    out.extend_from_slice(&scratch[skip..frames_out]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: u32, secs: f64, amp: f32) -> Vec<f32> {
        let n = (rate as f64 * secs) as usize;
        (0..n)
            .map(|i| {
                amp * (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin() as f32
            })
            .collect()
    }

    /// Frequency estimate from interpolated ascending zero crossings, measured over the
    /// middle half of the signal so the resampler's fade-in/tail cannot skew it.
    fn estimate_freq(samples: &[f32], rate: u32) -> f64 {
        let lo = samples.len() / 4;
        let hi = samples.len() * 3 / 4;
        let s = &samples[lo..hi];
        let mut crossings = Vec::new();
        for i in 1..s.len() {
            if s[i - 1] < 0.0 && s[i] >= 0.0 {
                let a = s[i - 1] as f64;
                let b = s[i] as f64;
                let frac = if b != a { -a / (b - a) } else { 0.0 };
                crossings.push(i as f64 - 1.0 + frac);
            }
        }
        assert!(
            crossings.len() >= 2,
            "not enough zero crossings to measure a pitch"
        );
        let span = crossings[crossings.len() - 1] - crossings[0];
        (crossings.len() - 1) as f64 / span * rate as f64
    }

    fn rms(samples: &[f32]) -> f64 {
        (samples.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / samples.len() as f64).sqrt()
    }

    #[test]
    fn tail_impulses_are_not_replaced_by_filter_delay() {
        for (input_rate, output_rate) in [(48_000, 16_000), (24_000, 48_000), (44_100, 48_000)] {
            for partial in [0, 17] {
                let mut rs = StreamResampler::new(input_rate, output_rate).unwrap();
                let need = rs.inner.as_ref().unwrap().input_frames_next();
                let mut input = vec![0.0; need * 4 + partial];
                let at = input.len() - 12;
                input[at] = 1.0;
                let mut output = rs.process(&input);
                output.extend(rs.flush());
                let expected =
                    (input.len() as f64 * output_rate as f64 / input_rate as f64).round() as usize;
                assert_eq!(output.len(), expected);
                let peak = output
                    .iter()
                    .enumerate()
                    .max_by(|(_, a), (_, b)| a.abs().total_cmp(&b.abs()))
                    .unwrap();
                assert!(
                    peak.1.abs() > 0.15,
                    "tail impulse disappeared: {input_rate}->{output_rate}, partial={partial}"
                );
                let wanted = (at as f64 * output_rate as f64 / input_rate as f64).round() as usize;
                assert!(
                    peak.0.abs_diff(wanted) <= 1,
                    "misaligned peak {} != {wanted}",
                    peak.0
                );
                assert!(rs.flush().is_empty());
            }
        }
    }

    #[test]
    fn long_input_matches_arbitrary_streaming_blocks() {
        let input = sine(440.0, 48_000, 10.0, 0.5);
        let mut batch = StreamResampler::new(48_000, 16_000).unwrap();
        let mut expected = batch.process(&input);
        expected.extend(batch.flush());
        let mut stream = StreamResampler::new(48_000, 16_000).unwrap();
        let mut actual = Vec::new();
        for block in input.chunks(137) {
            actual.extend(stream.process(block));
        }
        actual.extend(stream.flush());
        assert_eq!(actual, expected);
        assert!(stream.pending.is_empty());
    }

    #[test]
    fn equal_rates_are_an_exact_passthrough() {
        let input = sine(440.0, 48_000, 0.05, 0.5);

        let mut rs = StreamResampler::new(48_000, 48_000).unwrap();
        assert_eq!(
            rs.process(&input),
            input,
            "identity must not touch a single sample"
        );
        assert!(rs.flush().is_empty());

        // Arbitrary block sizes must not change the result either.
        let mut rs = StreamResampler::new(48_000, 48_000).unwrap();
        let mut out = Vec::new();
        for block in input.chunks(37) {
            out.extend_from_slice(&rs.process(block));
        }
        assert_eq!(out, input);
    }

    #[test]
    fn rejects_zero_rates() {
        assert!(StreamResampler::new(0, 16_000).is_err());
        assert!(StreamResampler::new(48_000, 0).is_err());
    }

    #[test]
    fn downsample_48k_to_16k_preserves_pitch_and_duration() {
        let freq = 440.0;
        let input = sine(freq, 48_000, 0.5, 0.5);

        let mut rs = StreamResampler::new(48_000, 16_000).unwrap();
        let mut out = Vec::new();
        for block in input.chunks(512) {
            out.extend_from_slice(&rs.process(block));
        }
        out.extend_from_slice(&rs.flush());

        let expected = input.len() / 3;
        assert!(
            (out.len() as i64 - expected as i64).abs() <= 2,
            "expected ~{expected} output frames, got {}",
            out.len()
        );
        let measured = estimate_freq(&out, 16_000);
        assert!(
            (measured - freq).abs() < 4.0,
            "pitch drifted: measured {measured:.2} Hz for a {freq} Hz input"
        );
        // Anti-aliased sine keeps its amplitude (linear interpolation would not matter
        // for a pure tone, but a broken filter/ratio would show up here).
        assert!(rms(&out) > 0.2, "output too quiet: rms {}", rms(&out));
    }

    #[test]
    fn upsample_24k_to_48k_preserves_pitch_and_duration() {
        let freq = 300.0;
        let input = sine(freq, 24_000, 0.25, 0.5);

        let mut rs = StreamResampler::new(24_000, 48_000).unwrap();
        let mut out = Vec::new();
        for block in input.chunks(240) {
            out.extend_from_slice(&rs.process(block));
        }
        out.extend_from_slice(&rs.flush());

        assert!(
            (out.len() as i64 - input.len() as i64 * 2).abs() <= 2,
            "expected ~{} output frames, got {}",
            input.len() * 2,
            out.len()
        );
        let measured = estimate_freq(&out, 48_000);
        assert!(
            (measured - freq).abs() < 4.0,
            "pitch drifted: measured {measured:.2} Hz for a {freq} Hz input"
        );
    }

    #[test]
    fn non_integer_ratio_keeps_duration() {
        let input = sine(440.0, 44_100, 0.2, 0.5);
        let mut rs = StreamResampler::new(44_100, 48_000).unwrap();
        let mut out = Vec::new();
        for block in input.chunks(1024) {
            out.extend_from_slice(&rs.process(block));
        }
        out.extend_from_slice(&rs.flush());
        let expected = input.len() as f64 * 48_000.0 / 44_100.0;
        assert!(
            (out.len() as f64 - expected).abs() <= 2.0,
            "expected ~{expected:.0} frames, got {}",
            out.len()
        );
    }
}
