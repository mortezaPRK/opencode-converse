//! Offline listening comparison. No microphone or playback device is opened.
//! Run with --features native-audio and an output directory argument.
use anyhow::{ensure, Context, Result};
use std::path::Path;
use std::sync::atomic::AtomicBool;

// Reuse production inventory/path validation; this tool needs only a subset.
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
    println!("Compare phrase-aware against forced-word-cuts and whole-input. Sampling is stochastic; these are listening diagnostics, not a controlled model-quality benchmark.");
    Ok(())
}
