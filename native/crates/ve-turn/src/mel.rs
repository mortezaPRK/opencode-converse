//! Whisper log-mel spectrogram, ported to match `transformers`'
//! `WhisperFeatureExtractor` (`feature_extraction_whisper.py`) +
//! `audio_utils.spectrogram` + `audio_utils.mel_filter_bank` (Slaney mel scale, Slaney
//! area norm).
//!
//! `whisper-rs` exposes no mel/FFT API, so this frontend implements the reference math:
//!
//! 1. last 8 s of audio, left-zero-padded (done by the caller) -> 128 000 f32 samples
//! 2. `do_normalize=True` -> zero-mean / unit-variance over the WHOLE chunk
//!    (this is `zero_mean_unit_var_norm`, applied to the raw waveform *before* the STFT —
//!    it is *not* log-mel normalisation)
//! 3. reflect-pad by `n_fft / 2 = 200`
//! 4. 801 frames of 400 samples, hop 160, *periodic* Hann window, rfft, `|X|^2`
//! 5. mel filterbank (201 x 80), clamp to 1e-10, `log10`
//! 6. drop the last frame -> (80, 800)
//! 7. `np.maximum(x, x.max() - 8.0)`, then `(x + 4.0) / 4.0`
//!
//! `realfft` (a rustfft wrapper doing the real-input optimisation) replaces numpy's
//! `np.fft.rfft`; the mel projection is parallelised over mel bins with
//! `std::thread::scope`, so no extra dependency. Both are numerically equivalent to the
//! reference within floating-point rounding error.
//!
//! Modified Rust port of Hugging Face Transformers' Whisper/audio utilities.
//! Upstream notices: Copyright 2022 The HuggingFace Inc. team; Copyright 2023
//! The HuggingFace Inc. team and the librosa & torchaudio authors. Apache-2.0;
//! see native/THIRD_PARTY.md for pinned sources and redistribution gates.

use realfft::{RealFftPlanner, RealToComplex};
use rustfft::num_complex::Complex;
use std::sync::Arc;

pub const SAMPLE_RATE: usize = 16_000;
pub const N_FFT: usize = 400;
pub const HOP: usize = 160;
pub const N_MELS: usize = 80;
pub const N_FREQ_BINS: usize = N_FFT / 2 + 1; // 201
pub const CHUNK: usize = 8 * SAMPLE_RATE; // 128_000 = 8 s
pub const N_FRAMES: usize = CHUNK / HOP; // 800

const MEL_FLOOR: f64 = 1e-10;
const NEG_LOG_BIAS: f64 = 8.0;
const LOG_OFFSET: f64 = 4.0;
const LOG_SCALE: f64 = 4.0;

/// `audio_utils.hertz_to_mel(.., mel_scale="slaney")`
fn hz_to_mel(freq: f64) -> f64 {
    const MIN_LOG_HZ: f64 = 1000.0;
    const MIN_LOG_MEL: f64 = 15.0;
    let logstep = 27.0 / 6.4f64.ln();
    if freq >= MIN_LOG_HZ {
        MIN_LOG_MEL + (freq / MIN_LOG_HZ).ln() * logstep
    } else {
        3.0 * freq / 200.0
    }
}

/// `audio_utils.mel_to_hertz(.., mel_scale="slaney")`
fn mel_to_hz(mels: f64) -> f64 {
    const MIN_LOG_HZ: f64 = 1000.0;
    const MIN_LOG_MEL: f64 = 15.0;
    let logstep = 6.4f64.ln() / 27.0;
    if mels >= MIN_LOG_MEL {
        MIN_LOG_HZ * (logstep * (mels - MIN_LOG_MEL)).exp()
    } else {
        200.0 * mels / 3.0
    }
}

/// `audio_utils.mel_filter_bank(201, 80, 0.0, 8000.0, 16000, norm="slaney", mel_scale="slaney")`
///
/// Returns the `(N_FREQ_BINS, N_MELS)` matrix, row-major (`[freq_bin * N_MELS + mel]`).
/// This is exactly the layout `transformers` builds; the parity test in this module checks
/// it against a filterbank dumped from Python (max |diff| = 0.0).
pub fn mel_filter_bank() -> Vec<f64> {
    let n = N_FREQ_BINS;
    let max_freq = (SAMPLE_RATE / 2) as f64; // 8000.0

    let mel_min = hz_to_mel(0.0);
    let mel_max = hz_to_mel(max_freq);
    let npts = N_MELS + 2;
    let filter_freqs: Vec<f64> = (0..npts)
        .map(|i| mel_to_hz(mel_min + (mel_max - mel_min) * i as f64 / (npts - 1) as f64))
        .collect();

    // np.linspace(0, sampling_rate // 2, num_frequency_bins)
    let fft_freqs: Vec<f64> = (0..n)
        .map(|j| max_freq * j as f64 / (n - 1) as f64)
        .collect();

    let filter_diff: Vec<f64> = (0..npts - 1)
        .map(|i| filter_freqs[i + 1] - filter_freqs[i])
        .collect();
    // Slaney area normalization
    let enorm: Vec<f64> = (0..N_MELS)
        .map(|k| 2.0 / (filter_freqs[k + 2] - filter_freqs[k]))
        .collect();

    let mut f = vec![0.0f64; n * N_MELS];
    for (j, &ff) in fft_freqs.iter().enumerate() {
        for k in 0..N_MELS {
            // _create_triangular_filter_bank
            let down = -(filter_freqs[k] - ff) / filter_diff[k];
            let up = (filter_freqs[k + 2] - ff) / filter_diff[k + 1];
            f[j * N_MELS + k] = down.min(up).max(0.0) * enorm[k];
        }
    }
    f
}

/// Reusable log-mel front end: FFT plan, window, filterbank and every scratch buffer are
/// built once, so a per-utterance call allocates only its output.
pub struct LogMel {
    window: Vec<f64>,
    /// The `(N_FREQ_BINS, N_MELS)` matrix transposed to `(N_MELS, N_FREQ_BINS)`, so the
    /// accumulation inner loop walks both operands contiguously and the compiler can
    /// auto-vectorise it. Purely a cache/ILP optimisation; numerically identical.
    filters_t: Vec<f64>,
    r2c: Arc<dyn RealToComplex<f64>>,
    /// reusable buffers (single-threaded FFT pass)
    buf: Vec<f64>,
    spectrum: Vec<Complex<f64>>,
    scratch: Vec<Complex<f64>>,
    padded: Vec<f64>,
    /// Power spectrogram, frame-major: `power[frame * N_FREQ_BINS + bin]`.
    power: Vec<f64>,
    /// How many worker threads the mel projection may use.
    threads: usize,
}

impl LogMel {
    pub fn new() -> Self {
        Self::with_threads(
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .clamp(1, 4),
        )
    }

    pub fn with_threads(threads: usize) -> Self {
        let mut planner = RealFftPlanner::<f64>::new();
        let r2c = planner.plan_fft_forward(N_FFT);

        // `window_function(400, "hann")` with periodic=True:
        //   length = 401, np.hanning(401) = 0.5 - 0.5*cos(2*pi*k/400), then truncated to 400.
        let window: Vec<f64> = (0..N_FFT)
            .map(|k| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * k as f64 / N_FFT as f64).cos())
            .collect();

        let filters = mel_filter_bank();
        let mut filters_t = vec![0.0f64; N_MELS * N_FREQ_BINS];
        for j in 0..N_FREQ_BINS {
            for k in 0..N_MELS {
                filters_t[k * N_FREQ_BINS + j] = filters[j * N_MELS + k];
            }
        }

        Self {
            window,
            filters_t,
            buf: r2c.make_input_vec(),
            spectrum: r2c.make_output_vec(),
            scratch: r2c.make_scratch_vec(),
            padded: Vec::with_capacity(CHUNK + N_FFT),
            power: vec![0.0; (N_FRAMES + 1) * N_FREQ_BINS],
            r2c,
            threads: threads.max(1),
        }
    }

    /// `audio_utils.zero_mean_unit_var_norm` for a single (already padded) chunk:
    /// `(x - x.mean()) / sqrt(x.var() + 1e-7)`, population variance.
    fn normalize(&self, audio: &[f32]) -> Vec<f64> {
        let n = audio.len() as f64;
        let mut sum = 0.0f64;
        for &v in audio {
            sum += v as f64;
        }
        let mean = sum / n;
        let mut sq = 0.0f64;
        for &v in audio {
            let d = v as f64 - mean;
            sq += d * d;
        }
        let var = sq / n;
        let inv = 1.0 / (var + 1e-7).sqrt();
        audio.iter().map(|&v| (v as f64 - mean) * inv).collect()
    }

    /// Reflect-pad `x` by `p` on both sides (`np.pad(.., mode="reflect")`).
    fn reflect_pad(&mut self, x: &[f64], p: usize) {
        let n = x.len();
        self.padded.clear();
        self.padded.reserve(n + 2 * p);
        for k in (1..=p).rev() {
            self.padded.push(x[k]);
        }
        self.padded.extend_from_slice(x);
        for k in 1..=p {
            self.padded.push(x[n - 1 - k]);
        }
    }

    /// Windowed rfft of every frame into `self.power` (frame-major). Returns the frame count.
    fn forward(&mut self) -> usize {
        let n_frames = 1 + (self.padded.len() - N_FFT) / HOP;
        for frame_idx in 0..n_frames {
            let start = frame_idx * HOP;
            for k in 0..N_FFT {
                self.buf[k] = self.padded[start + k] * self.window[k];
            }
            self.r2c
                .process_with_scratch(&mut self.buf, &mut self.spectrum, &mut self.scratch)
                .expect("rfft");
            let row = &mut self.power[frame_idx * N_FREQ_BINS..(frame_idx + 1) * N_FREQ_BINS];
            for (j, slot) in row.iter_mut().enumerate() {
                let c = self.spectrum[j];
                *slot = c.re * c.re + c.im * c.im;
            }
        }
        n_frames
    }

    /// Full pipeline. `chunk` must be exactly [`CHUNK`] samples (8 s, already left-padded).
    /// Returns `(N_MELS, N_FRAMES)` row-major normalised log-mel.
    pub fn extract(&mut self, chunk: &[f32]) -> Vec<f32> {
        assert_eq!(chunk.len(), CHUNK, "log-mel input must be exactly 8 s");
        let norm = self.normalize(chunk);
        self.extract_normalized(&norm)
    }

    /// Same, but on an already zero-mean-unit-var window.
    fn extract_normalized(&mut self, norm: &[f64]) -> Vec<f32> {
        let p = N_FFT / 2;
        self.reflect_pad(norm, p);
        let n_frames = self.forward();
        debug_assert_eq!(n_frames, N_FRAMES + 1);

        // mel_filters.T @ power -> (N_MELS, n_frames) mel-major, clamp to MEL_FLOOR, log10.
        // Frame-major power + mel-major filters => both operands of the inner loop contiguous.
        // Mel bins are independent, so split them across threads (each owns a disjoint
        // slice of `out` and reports its own maximum, reduced afterwards).
        let mut out = vec![0.0f32; N_MELS * N_FRAMES];
        let nthreads = self.threads.min(N_MELS);
        let bins_per = N_MELS.div_ceil(nthreads);
        let mut maxes = vec![f64::NEG_INFINITY; nthreads];

        let filters_t = &self.filters_t;
        let power = &self.power;
        std::thread::scope(|sc| {
            for (ti, (out_chunk, mx)) in out
                .chunks_mut(bins_per * N_FRAMES)
                .zip(maxes.iter_mut())
                .enumerate()
            {
                let k0 = ti * bins_per;
                sc.spawn(move || {
                    let nk = out_chunk.len() / N_FRAMES;
                    let mut best = f64::NEG_INFINITY;
                    for kk in 0..nk {
                        let k = k0 + kk;
                        let frow = &filters_t[k * N_FREQ_BINS..(k + 1) * N_FREQ_BINS];
                        let o = &mut out_chunk[kk * N_FRAMES..(kk + 1) * N_FRAMES];
                        for (frame_idx, slot) in o.iter_mut().enumerate() {
                            let prow =
                                &power[frame_idx * N_FREQ_BINS..(frame_idx + 1) * N_FREQ_BINS];
                            let mut acc = 0.0f64;
                            for j in 0..N_FREQ_BINS {
                                acc += frow[j] * prow[j];
                            }
                            let v = acc.max(MEL_FLOOR).log10();
                            if v > best {
                                best = v;
                            }
                            *slot = v as f32;
                        }
                    }
                    *mx = best;
                });
            }
        });

        let max_val = maxes.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let floor = max_val - NEG_LOG_BIAS;
        for v in out.iter_mut() {
            let clamped = (*v as f64).max(floor);
            *v = ((clamped + LOG_OFFSET) / LOG_SCALE) as f32;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_f32(path: &str) -> Vec<f32> {
        let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    }

    /// The Slaney filterbank is reproduced *exactly* — not approximately — because every
    /// step of `mel_filter_bank` is a plain f64 arithmetic sequence with no library call.
    #[test]
    fn extraction_matches_pinned_transformers_for_every_feature() {
        fn waveform(n: usize) -> Vec<f32> {
            (0..n)
                .map(|i| {
                    ((((i as u64 * 73 + i as u64 * i as u64 * 19) % 65521) as f64 - 32760.0)
                        / 65536.0
                        * (0.25 + (i % 997) as f64 / 1024.0)
                        + 0.125) as f32
                })
                .collect()
        }
        let mut short = vec![0.0; CHUNK - 1237];
        short.extend(waveform(1237));
        for (name, input) in [
            ("nonzero", waveform(CHUNK)),
            ("silence", vec![0.0; CHUNK]),
            ("left_padded", short),
        ] {
            let expected = read_f32(&format!(
                "{}/tests/data/logmel_{name}.f32",
                env!("CARGO_MANIFEST_DIR")
            ));
            assert_eq!(expected.len(), N_MELS * N_FRAMES);
            for threads in [1, 4] {
                let actual = LogMel::with_threads(threads).extract(&input);
                assert_eq!(actual.len(), expected.len());
                // Reference normalization and complex STFT storage use f32 while
                // Rust accumulates in f64. 2e-5 absolute feature units permits that
                // rounding, not layout/window/padding/log-scale regressions.
                for (i, (a, b)) in actual.iter().zip(&expected).enumerate() {
                    assert!(
                        a.is_finite() && (a - b).abs() <= 2e-5,
                        "{name}, threads={threads}, mel={}, frame={}: {a} != {b}",
                        i / N_FRAMES,
                        i % N_FRAMES
                    );
                }
            }
        }
    }

    #[test]
    fn mel_filterbank_matches_python_reference_exactly() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/mel_filters.f32");
        let expected = read_f32(path);
        assert_eq!(expected.len(), N_FREQ_BINS * N_MELS);

        let got: Vec<f32> = mel_filter_bank().iter().map(|&v| v as f32).collect();
        let worst = got
            .iter()
            .zip(&expected)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert_eq!(
            worst, 0.0,
            "filterbank must match Python bit-for-bit, worst |diff| = {worst:e}"
        );
    }

    /// The periodic Hann window is a classic off-by-one trap: numpy's window for
    /// `periodic=True, n_fft=400` is `np.hanning(401)[:-1]`, not `np.hanning(400)`.
    #[test]
    fn hann_window_is_periodic() {
        let mel = LogMel::new();
        assert_eq!(mel.window.len(), N_FFT);
        assert_eq!(mel.window[0], 0.0);
        // Periodic: the value that *would* be at index 400 is absent, so the last sample
        // is not the mirror of the first — with the symmetric window w[399] would be 0.
        let last = mel.window[N_FFT - 1];
        assert!(last > 0.0, "periodic Hann must not end on zero, got {last}");
        assert!((mel.window[N_FFT / 2] - 1.0).abs() < 1e-12);
    }
}
