//! Offline listening comparison. No microphone or playback device is opened.
//! Run with --features native-audio and an output directory argument.
use anyhow::{ensure, Context, Result};
use std::path::Path;
use std::sync::atomic::AtomicBool;

// Reuse production inventory/path validation; this tool needs only a subset.
#[allow(dead_code)]
#[path = "../src/addressing.rs"]
mod addressing;
#[allow(dead_code)]
#[path = "../src/chain.rs"]
mod chain;
#[allow(dead_code)]
#[path = "../src/controller.rs"]
mod controller;
#[allow(dead_code)]
#[path = "../src/inventory.rs"]
mod inventory;
#[allow(dead_code)]
#[path = "../src/paths.rs"]
mod paths;
#[allow(dead_code)]
#[path = "../src/protocol.rs"]
mod protocol;
#[path = "../src/speech.rs"]
mod speech;

const SENTENCES: &[&str] = &[
    "Sure. Would you like me to check that now?",
    "I can check the current implementation and explain what needs to change before we start editing, so you can choose the approach that best fits your project and your priorities.",
    "Please ask Dr. Jones and Mr. Smith about the U.S. team, then compare the results with version 1.2.3 before we continue.",
    "Before we change the speech model, we should compare the same voice using complete sentences and natural phrase boundaries, because a sentence that is cut in the middle can sound unnatural even when the individual words are clear and the voice itself is pleasant, especially when several separate fragments are joined together without their original context.",
];

fn wav(path: &Path, rate: u32, samples: &[f32]) -> Result<()> {
    let bytes = u32::try_from(samples.len().checked_mul(2).context("Audio too large")?)?;
    let mut out = Vec::with_capacity(bytes as usize + 44);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + bytes).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&bytes.to_le_bytes());
    for sample in samples {
        ensure!(sample.is_finite(), "Non-finite synthesized sample");
        out.extend_from_slice(&((sample.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    std::fs::write(path, out)?;
    Ok(())
}

/// Replies arrive word by word at this pace (about 33 tokens/s, a typical fast LLM stream).
const WORD_MS: f64 = 40.0;

const REPLIES: &[&str] = &[
    "Sure, I can look at the current implementation and explain what needs to change. Then we can decide together.",
    "Good question. The speech engine splits replies into sentences, so the first sentence decides when you hear anything.",
    "Right, the build passed on the first try, but two tests were skipped because the audio device was missing. Do you want me to rerun them?",
];

/// Time from a reply's first delta to its first audible sample: stream the words,
/// find when the first chunk is complete, then synthesize that chunk (warm model).
fn first_audio(
    tts: &mut ve_tts::Tts,
    text: &str,
    early: bool,
    never: &AtomicBool,
) -> Result<(f64, f64, String)> {
    let words: Vec<_> = text.split_whitespace().collect();
    let chunk = if early {
        let mut speech = speech::Speech::default();
        let mut found = None;
        for (i, word) in words.iter().enumerate() {
            if let Some(first) = speech.push(&format!("{word} "), false).into_iter().next() {
                found = Some((i + 1, first));
                break;
            }
        }
        found.context("No chunk produced")?
    } else {
        // Previous behaviour: wait for the first complete sentence.
        let mut found = None;
        for (i, word) in words.iter().enumerate() {
            if word.ends_with(['.', '!', '?']) {
                found = Some((i + 1, words[..=i].join(" ")));
                break;
            }
        }
        found.context("No sentence")?
    };
    let (arrived, chunk) = chunk;
    // Median of several warm runs: single TTFA samples vary by tens of ms.
    let mut runs = Vec::new();
    for _ in 0..7 {
        let started = std::time::Instant::now();
        let mut first = None;
        tts.synth_stream(&chunk, never, |part| {
            if first.is_none() && part.iter().any(|s| s.abs() > 0.0) {
                first = Some(started.elapsed().as_secs_f64() * 1000.0);
            }
            Ok(())
        })?;
        runs.push(first.context("No audio")?);
    }
    runs.sort_by(f64::total_cmp);
    Ok((arrived as f64 * WORD_MS, runs[runs.len() / 2], chunk))
}

fn latency_timeline(tts: &mut ve_tts::Tts) -> Result<()> {
    let never = AtomicBool::new(false);
    let _ = tts.synth_stream("Warm up.", &never, |_| Ok(()))?;
    println!("\n== Latency timeline (warm model, words stream every {WORD_MS:.0} ms) ==");
    let ack_delay = controller::ACK_DELAY.as_secs_f64() * 1000.0;
    for text in controller::ACK_PHRASES {
        let started = std::time::Instant::now();
        let mut samples = 0;
        let mut first = None;
        let mut joiner = ve_tts::Joiner::new(tts.sample_rate());
        tts.synth_stream(text, &never, |part| {
            samples += joiner.push_part(part).len();
            first.get_or_insert(started.elapsed().as_secs_f64() * 1000.0);
            Ok(())
        })?;
        samples += joiner.end_chunk(text).len();
        println!(
            "ack {text:?}: one-time synthesis {:.0} ms (first audio {:.0} ms), {:.2}s audio, {} KiB cached; per-turn synthesis 0 ms",
            started.elapsed().as_secs_f64() * 1000.0,
            first.unwrap_or(f64::NAN),
            samples as f64 / tts.sample_rate() as f64,
            samples * 4 / 1024
        );
    }
    println!(
        "turn commit -> ack audio start: {ack_delay:.0} ms delay + ~0 ms synthesis (cached); uncached would add the TTFA below"
    );
    for text in REPLIES {
        let (arrive_old, ttfa_old, old) = first_audio(tts, text, false, &never)?;
        let (arrive_new, ttfa_new, new) = first_audio(tts, text, true, &never)?;
        println!("reply: {text}");
        println!(
            "  before: first chunk {old:?} complete at {arrive_old:.0} ms, +TTFA {ttfa_old:.0} ms => first audio {:.0} ms after first delta",
            arrive_old + ttfa_old
        );
        println!(
            "  after:  first chunk {new:?} complete at {arrive_new:.0} ms, +TTFA {ttfa_new:.0} ms => first audio {:.0} ms after first delta",
            arrive_new + ttfa_new
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    let output = std::env::args_os()
        .nth(1)
        .context("Pass a new output directory for listening samples")?;
    let output = Path::new(&output);
    // Do not overwrite an earlier evaluation or user files.
    std::fs::create_dir(output)?;
    let models = paths::check_models()?;
    let loading = std::time::Instant::now();
    let mut tts = ve_tts::Tts::new(ve_tts::TtsConfig {
        dir: models.tts.dir,
        voices_dir: None,
    })?;
    println!("TTS load: {:.2}s", loading.elapsed().as_secs_f64());
    let rate = tts.sample_rate();
    let never = AtomicBool::new(false);
    for (index, text) in SENTENCES.iter().enumerate() {
        // Deliberately forced 24-word cuts are a diagnostic control, not an
        // alternate production chunker or an exact replay of historic behavior.
        let words: Vec<_> = text.split_whitespace().collect();
        let forced: Vec<_> = words.chunks(24).map(|part| part.join(" ")).collect();
        for (name, chunks) in [
            ("phrase-aware", speech::Speech::default().push(text, true)),
            ("whole-input", vec![text.to_string()]),
            ("forced-word-cuts", forced),
        ] {
            let started = std::time::Instant::now();
            let mut joiner = ve_tts::Joiner::new(rate);
            let mut audio = Vec::new();
            // Time to first audible sample: streamed through the same joiner as production.
            let mut first_audio = None;
            for chunk in &chunks {
                let before = audio.len();
                tts.synth_stream(chunk, &never, |part| {
                    audio.extend(joiner.push_part(part));
                    if first_audio.is_none() && audio.iter().any(|s| s.abs() > 0.0) {
                        first_audio = Some(started.elapsed().as_secs_f64());
                    }
                    Ok(())
                })?;
                audio.extend(joiner.end_chunk(chunk));
                ensure!(audio.len() > before, "Synthesis returned no audio");
            }
            let elapsed = started.elapsed().as_secs_f64();
            let duration = audio.len() as f64 / rate as f64;
            let file = format!("{:02}-{name}.wav", index + 1);
            wav(&output.join(&file), rate, &audio)?;
            println!(
                "{file}: {} chunks, {duration:.2}s audio, TTFA {:.0} ms, {elapsed:.2}s synthesis, RTF {:.3}",
                chunks.len(),
                first_audio.unwrap_or(f64::NAN) * 1000.0,
                elapsed / duration
            );
        }
    }
    // Cancellation: raise the flag from inside the first chunk and time the return.
    let cancel = AtomicBool::new(false);
    let mut stopped = None;
    let finish = tts.synth_stream(SENTENCES[3], &cancel, |_| {
        if stopped.is_none() {
            cancel.store(true, std::sync::atomic::Ordering::Release);
            stopped = Some(std::time::Instant::now());
        }
        Ok(())
    })?;
    println!(
        "Cancel: {finish:?}, returned {:.0} ms after the flag was raised",
        stopped
            .context("No audio before cancellation")?
            .elapsed()
            .as_secs_f64()
            * 1000.0
    );
    latency_timeline(&mut tts)?;
    println!("Compare phrase-aware against forced-word-cuts and whole-input. Sampling is stochastic; these are listening diagnostics, not a controlled model-quality benchmark.");
    Ok(())
}
