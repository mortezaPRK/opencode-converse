//! Offline echo-cancellation measurement. No microphone or playback device is opened.
//!
//! `aec_quality [--quick]` synthesizes the "assistant" with Phonon, builds a simulated
//! microphone (delayed, attenuated, low-passed, mildly distorted, reverberant copy of what
//! the speaker played, plus noise and, optionally, a near-end user from macOS `say`), and
//! runs it closed-loop through the production pieces: WebRTC AEC3 (`ve_audio::EchoCanceller`),
//! the turn detector with the barge-in guard, and Moonshine. When the detector fires, the
//! simulated output is cut after a short flush latency, exactly as the engine flushes playback.
//!
//! It reports ERLE, Silero false-speech frames on echo-only audio (AEC off vs on), how fast
//! the near-end user is detected, and what ASR hears of the user during double talk.
//! This is a simulation: a measured room, real devices, clock drift and the output/input
//! latency jitter of cpal are NOT covered. Run with --features native-audio in release mode.
use anyhow::{ensure, Context, Result};
use std::sync::atomic::AtomicBool;

#[allow(dead_code)]
#[path = "../src/inventory.rs"]
mod inventory;
#[allow(dead_code)]
#[path = "../src/paths.rs"]
mod paths;
#[cfg(test)]
#[allow(dead_code)]
#[path = "../src/protocol.rs"]
mod protocol;
#[allow(dead_code)]
#[path = "../../core/src/turn.rs"]
mod turn;

use turn::{BargeGuard, Guard, TurnDetector, TurnEvent, VAD_FRAME};

const RATE: usize = 16_000;
const FRAME: usize = ve_audio::AEC_FRAME;
/// Output flush latency after the detector fires (callback block + device).
const FLUSH_LATENCY: usize = RATE * 40 / 1000;
const ASSISTANT: &[&str] = &[
    "Sure. I can check the current implementation and explain what needs to change before we start editing.",
    "The settings file lives in the client folder, and it exports a single function that reads the options.",
    "Once you confirm the approach, I will make the change, run the tests, and summarize the result for you.",
];
const USER_TEXT: &str = "Please stop and open the package file instead of that one";

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as f32 / (1u64 << 31) as f32) * 2.0 - 1.0
    }
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

fn scale_to_rms(x: &mut [f32], target: f32) {
    let g = target / rms(x).max(1e-9);
    x.iter_mut().for_each(|s| *s *= g);
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

fn edit_distance(a: &[String], b: &[String]) -> usize {
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(x != y)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

fn to_16k(tts: &ve_tts::Tts, samples: &[f32]) -> Result<Vec<f32>> {
    let mut resampler = ve_audio::StreamResampler::new(tts.sample_rate(), RATE as u32)?;
    let mut out = resampler.process(samples);
    out.extend(resampler.flush());
    Ok(out)
}

fn say_16k(text: &str) -> Result<Vec<f32>> {
    let path = std::env::temp_dir().join("aec_quality_user.wav");
    let status = std::process::Command::new("say")
        .args(["-o"])
        .arg(&path)
        .args(["--data-format=LEI16@16000", text])
        .status()
        .context("macOS `say` is required for the near-end user utterance")?;
    ensure!(status.success(), "say failed");
    let bytes = std::fs::read(&path)?;
    let at = bytes
        .windows(4)
        .position(|w| w == b"data")
        .context("no data chunk")?;
    let pcm = &bytes[at + 8..];
    Ok(pcm
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect())
}

/// Sparse speaker→mic impulse response: a direct path plus decaying reflections.
struct EchoPath {
    taps: Vec<(usize, f32)>,
    lp: f32,
    state: f32,
}

impl EchoPath {
    fn new(delay_ms: usize, gain: f32, rt60_ms: usize, seed: u64) -> Self {
        let mut rng = Lcg(seed);
        let direct = RATE * delay_ms / 1000;
        let mut taps = vec![(direct, gain)];
        let tail = RATE * rt60_ms / 1000;
        for _ in 0..40 {
            let lag = ((rng.next().abs()) * tail as f32) as usize + 8;
            let decay = (-6.9 * lag as f32 / tail as f32).exp();
            taps.push((direct + lag, gain * 0.35 * rng.next() * decay));
        }
        Self {
            taps,
            lp: 0.55,
            state: 0.0,
        }
    }

    fn mic_sample(&mut self, speaker: &[f32], n: usize) -> f32 {
        let mut acc = 0.0;
        for &(lag, g) in &self.taps {
            if n >= lag {
                acc += g * speaker[n - lag];
            }
        }
        // Gentle low-pass (small speaker/mic bandwidth).
        self.state += self.lp * (acc - self.state);
        self.state
    }
}

/// Loudspeaker compression: mildly nonlinear, which a linear AEC cannot model.
fn speaker_nonlinearity(x: f32, drive: f32) -> f32 {
    (x * drive).tanh() / drive
}

struct Scenario {
    delay_ms: usize,
    echo_gain: f32,
    /// Near-end RMS relative to the assistant render RMS; `None` = echo only.
    near_gain: Option<f32>,
    aec: bool,
    guard: BargeGuard,
    /// Loudspeaker drive: 2 is mild compression, 6 is heavy clipping.
    drive: f32,
    rt60_ms: usize,
}

struct Outcome {
    /// 32 ms frame index of the first `SpeechStart`.
    start_frame: Option<usize>,
    /// Number of `SpeechStart` events (echo-only runs never cut playback).
    starts: usize,
    utterance: Option<Vec<f32>>,
    raw_mic: Vec<f32>,
    cleaned: Vec<f32>,
}

#[allow(clippy::too_many_arguments)]
fn closed_loop(
    sc: &Scenario,
    render_src: &[f32],
    near: &[f32],
    near_start: usize,
    detector: &mut TurnDetector,
    cut_on_start: bool,
) -> Result<Outcome> {
    detector.reset();
    let mut aec = ve_audio::EchoCanceller::new()?;
    let mut path = EchoPath::new(sc.delay_ms, sc.echo_gain, sc.rt60_ms, 7);
    let mut noise = Lcg(99);
    let total = render_src.len() + RATE * 4;
    let frames = total / FRAME;
    let mut speaker = Vec::with_capacity(frames * FRAME);
    let mut raw_mic = Vec::with_capacity(frames * FRAME);
    let mut cleaned = Vec::with_capacity(frames * FRAME);
    let mut cut_at: Option<usize> = None;
    let mut out = Outcome {
        start_frame: None,
        starts: 0,
        utterance: None,
        raw_mic: Vec::new(),
        cleaned: Vec::new(),
    };
    let mut fed = 0usize;
    for i in 0..frames {
        let base = i * FRAME;
        let mut reference = vec![0.0f32; FRAME];
        for (k, slot) in reference.iter_mut().enumerate() {
            let n = base + k;
            if cut_at.is_none_or(|c| n < c) {
                *slot = render_src.get(n).copied().unwrap_or(0.0);
            }
            speaker.push(speaker_nonlinearity(*slot, sc.drive));
        }
        let mut mic = vec![0.0f32; FRAME];
        for (k, slot) in mic.iter_mut().enumerate() {
            let n = base + k;
            let user = match sc.near_gain {
                Some(_) if n >= near_start => near.get(n - near_start).copied().unwrap_or(0.0),
                _ => 0.0,
            };
            *slot = path.mic_sample(&speaker, n) + user + 0.0003 * noise.next();
        }
        raw_mic.extend_from_slice(&mic);
        if sc.aec {
            aec.process_frame(&reference, &mut mic)?;
        }
        cleaned.extend_from_slice(&mic);
        // Playback is audible until the output is cut or the reply ended.
        let playing = cut_at.is_none() && base < render_src.len() + RATE / 4;
        let guard: Guard = playing.then_some(sc.guard);
        for event in detector.feed(&mic, guard)? {
            match event {
                TurnEvent::SpeechStart => {
                    out.starts += 1;
                    if out.start_frame.is_none() {
                        out.start_frame = Some((fed + FRAME).div_ceil(VAD_FRAME) - 1);
                        if cut_on_start {
                            cut_at = Some(base + FRAME + FLUSH_LATENCY);
                        }
                    }
                }
                TurnEvent::Utterance { audio, .. } => {
                    if out.utterance.is_none() {
                        out.utterance = Some(audio);
                    }
                }
            }
        }
        fed += FRAME;
        if out.utterance.is_some() && cut_on_start {
            break;
        }
    }
    out.raw_mic = raw_mic;
    out.cleaned = cleaned;
    Ok(out)
}

struct VadStats {
    frames: usize,
    over50: usize,
    over70: usize,
    over85: usize,
    run70: usize,
    run85: usize,
}

fn vad_stats(vad: &mut ve_vad::SileroVad, audio: &[f32]) -> Result<VadStats> {
    vad.reset();
    let probs = ve_vad::vad_probs(vad, audio)?;
    let longest = |threshold: f32| {
        let (mut best, mut run) = (0, 0);
        for p in &probs {
            run = if *p > threshold { run + 1 } else { 0 };
            best = best.max(run);
        }
        best
    };
    let count = |threshold: f32| probs.iter().filter(|p| **p > threshold).count();
    Ok(VadStats {
        frames: probs.len(),
        over50: count(0.5),
        over70: count(0.7),
        over85: count(0.85),
        run70: longest(0.7),
        run85: longest(0.85),
    })
}

fn erle_db(raw: &[f32], cleaned: &[f32]) -> f32 {
    let energy = |x: &[f32]| x.iter().map(|s| s * s).sum::<f32>().max(1e-12);
    10.0 * (energy(raw) / energy(cleaned)).log10()
}

fn main() -> Result<()> {
    let quick = std::env::args().any(|a| a == "--quick");
    // `--hard`: heavy loudspeaker clipping and a 450 ms reverberant tail, which a linear
    // canceller cannot fully remove. Used to see how much margin the guard has.
    let hard = std::env::args().any(|a| a == "--hard");
    let (drive, rt60_ms) = if hard { (6.0, 450) } else { (2.0, 180) };
    let models = paths::check_models()?;
    let mut tts = ve_tts::Tts::new(ve_tts::TtsConfig {
        dir: models.tts.dir.clone(),
    })?;
    let never = AtomicBool::new(false);
    let mut render = vec![0.0f32; RATE / 2];
    for text in ASSISTANT {
        let samples = tts.synth(text, &never)?;
        render.extend(to_16k(&tts, &samples)?);
        render.extend(std::iter::repeat_n(0.0, RATE / 5));
    }
    scale_to_rms(&mut render, 0.1);
    let user = say_16k(USER_TEXT)?;
    let user_voiced = {
        let mut vad = ve_vad::SileroVad::from_model(&models.vad)?;
        let probs = ve_vad::vad_probs(&mut vad, &user)?;
        probs.iter().position(|p| *p > 0.5).unwrap_or(0)
    };
    let mut vad = ve_vad::SileroVad::from_model(&models.vad)?;
    let mut detector = TurnDetector::new(&models.vad, &models.turn, turn::Endpointing::Responsive)?;
    let mut asr = ve_asr::Asr::from_config(ve_asr::AsrConfig {
        model_dir: models.asr.dir.clone(),
        n_threads: paths::DEFAULT_ASR_THREADS as usize,
    })?;
    let near_start = RATE * 6;
    println!(
        "assistant {:.1}s (RMS 0.1), user {:.1}s, user onset at {:.2}s (first voiced Silero frame +{} ms into the clip)",
        render.len() as f32 / RATE as f32,
        user.len() as f32 / RATE as f32,
        near_start as f32 / RATE as f32,
        user_voiced * 32
    );
    let delays: &[usize] = if quick { &[80] } else { &[30, 80, 120, 200] };
    let gains: &[f32] = if quick { &[0.5] } else { &[0.5, 0.15] };

    println!("\n== 1. Echo only (no user): ERLE and Silero false-speech frames ==");
    println!("gain delay  ERLE(dB)  | AEC off: >0.5 >0.7 >0.85 run0.7 run0.85 | AEC on: >0.5 >0.7 >0.85 run0.7 run0.85 | false starts: raw+15f/0.85 raw+4f/0.7 aec+4f/0.7");
    for (&echo_gain, &delay) in gains
        .iter()
        .flat_map(|g| delays.iter().map(move |d| (g, d)))
    {
        let echo_only = |aec, guard| Scenario {
            delay_ms: delay,
            echo_gain,
            near_gain: None,
            aec,
            guard,
            drive,
            rt60_ms,
        };
        let off = closed_loop(
            &echo_only(false, BargeGuard::UNCANCELLED),
            &render,
            &user,
            near_start,
            &mut detector,
            false,
        )?;
        let off_relaxed = closed_loop(
            &echo_only(false, BargeGuard::ECHO_CANCELLED),
            &render,
            &user,
            near_start,
            &mut detector,
            false,
        )?;
        let on = closed_loop(
            &echo_only(true, BargeGuard::ECHO_CANCELLED),
            &render,
            &user,
            near_start,
            &mut detector,
            false,
        )?;
        // ERLE after the filter has had 2 s to converge, over the remaining assistant speech.
        let (from, to) = (RATE * 2, render.len());
        let erle = erle_db(&off.raw_mic[from..to], &on.cleaned[from..to]);
        let a = vad_stats(&mut vad, &off.raw_mic[..render.len()])?;
        let b = vad_stats(&mut vad, &on.cleaned[..render.len()])?;
        println!(
            "{echo_gain:4} {delay:4}ms {erle:7.1}   | {:4} {:4} {:5} {:5} {:7} | {:4} {:4} {:5} {:5} {:7} | {} {} {}   ({} frames)",
            a.over50, a.over70, a.over85, a.run70, a.run85,
            b.over50, b.over70, b.over85, b.run70, b.run85,
            off.starts, off_relaxed.starts, on.starts, a.frames
        );
    }

    println!("\n== 2. Double talk: user starts at 6.00 s while the assistant keeps speaking ==");
    println!("gain delay  user/asst | AEC off, guard 15f@0.85: start(ms after onset) ASR WER | AEC on, guard 4f@0.7: start ASR WER | AEC on, guard 15f@0.85");
    let near_levels: &[f32] = if quick { &[0.5] } else { &[0.25, 0.5, 1.0] };
    let reference_words = words(USER_TEXT);
    let onset_frame = (near_start + user_voiced * VAD_FRAME) / VAD_FRAME;
    for (&echo_gain, &delay) in gains
        .iter()
        .flat_map(|g| delays.iter().map(move |d| (g, d)))
    {
        for &level in near_levels {
            let mut near = user.clone();
            scale_to_rms(&mut near, 0.1 * level * echo_gain);
            let mut cell = |aec, guard| -> Result<String> {
                let sc = Scenario {
                    delay_ms: delay,
                    echo_gain,
                    near_gain: Some(level),
                    aec,
                    guard,
                    drive,
                    rt60_ms,
                };
                let o = closed_loop(&sc, &render, &near, near_start, &mut detector, true)?;
                let Some(frame) = o.start_frame else {
                    return Ok("no start".into());
                };
                let latency = (frame as i64 - onset_frame as i64) * 32;
                let early = frame < onset_frame.saturating_sub(2);
                let text = match o.utterance {
                    Some(audio) => {
                        let t = asr.transcribe(&audio)?;
                        if t.rejected {
                            "(rejected)".to_owned()
                        } else {
                            t.text
                        }
                    }
                    None => "(no utterance)".to_owned(),
                };
                let wer = edit_distance(&reference_words, &words(&text)) as f32
                    / reference_words.len() as f32;
                Ok(format!(
                    "{latency:+5} ms{} WER {:.0}% \"{}\"",
                    if early { " FALSE" } else { "" },
                    wer * 100.0,
                    text
                ))
            };
            println!(
                "{echo_gain:4} {delay:4}ms {level:5.2}x   | {} | {} | {}",
                cell(false, BargeGuard::UNCANCELLED)?,
                cell(true, BargeGuard::ECHO_CANCELLED)?,
                cell(true, BargeGuard::UNCANCELLED)?
            );
        }
    }
    println!(
        "\nuser/asst = near-end RMS relative to the echo RMS at the mic (echo gain = speaker-to-mic amplitude). \
         'start' is measured from the first voiced Silero frame of the user clip; a start before it is FALSE."
    );
    Ok(())
}
