//! Offline ASR comparison. No microphone or playback device is opened.
//! `asr_quality synth DIR` synthesizes the fixed utterances with Phonon into a new directory
//! (16 kHz mono f32 little-endian `NN.f32` plus `refs.txt`); `asr_quality run DIR` transcribes
//! those exact files and reports word error rate, per-utterance latency, load time and RSS,
//! so two ASR builds can be compared on identical audio. Run with --features native-audio.
use anyhow::{ensure, Context, Result};
use std::path::Path;
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

const UTTERANCES: &[&str] = &[
    "Open src slash gui dot tsx.",
    "Run npm test.",
    "Rename the useEffect hook to useSessionEffect.",
    "Run cargo clippy with all features enabled.",
    "Install the package called typescript and then run npm run check.",
    "Check git status and commit the changes to the main branch.",
    "Add the serde and tokio crates to Cargo dot toml.",
    "Why does the React component render twice when the state changes?",
    "Open package dot json and bump the version to zero point three point zero.",
    "Please explain what this function does before we change anything.",
    "Create a new file called config dot ts in the client folder.",
    "Sure. Would you like me to check that now?",
    "I can check the current implementation and explain what needs to change before we start editing, so you can choose the approach that best fits your project and your priorities.",
    "Please ask Dr. Jones and Mr. Smith about the U.S. team, then compare the results with version 1.2.3 before we continue.",
];

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

fn rss_mb() -> f64 {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<f64>().ok())
        .map_or(f64::NAN, |kb| kb / 1024.0)
}

fn synth(dir: &Path) -> Result<()> {
    std::fs::create_dir(dir)?;
    let models = paths::check_models()?;
    let mut tts = ve_tts::Tts::new(ve_tts::TtsConfig {
        dir: models.tts.dir,
    })?;
    let never = AtomicBool::new(false);
    let mut refs = String::new();
    for (index, text) in UTTERANCES.iter().enumerate() {
        let samples = tts.synth(text, &never)?;
        let mut resampler = ve_audio::StreamResampler::new(tts.sample_rate(), 16_000)?;
        let mut audio = resampler.process(&samples);
        audio.extend(resampler.flush());
        ensure!(!audio.is_empty(), "Synthesis returned no audio");
        let bytes: Vec<u8> = audio.iter().flat_map(|s| s.to_le_bytes()).collect();
        std::fs::write(dir.join(format!("{:02}.f32", index + 1)), bytes)?;
        refs.push_str(text);
        refs.push('\n');
    }
    std::fs::write(dir.join("refs.txt"), refs)?;
    println!("Wrote {} utterances", UTTERANCES.len());
    Ok(())
}

// ASR_SETUP_BEGIN
fn load_asr(models: &paths::ModelPaths) -> Result<impl FnMut(&[f32]) -> Result<(String, bool)>> {
    let mut asr = ve_asr::Asr::from_config(ve_asr::AsrConfig {
        model_dir: models.asr.dir.clone(),
        n_threads: paths::DEFAULT_ASR_THREADS as usize,
    })?;
    Ok(move |audio: &[f32]| {
        let t = asr.transcribe(audio)?;
        Ok((t.text, t.rejected))
    })
}
// ASR_SETUP_END

fn run(dir: &Path) -> Result<()> {
    let models = paths::check_models()?;
    let rss_before = rss_mb();
    let started = std::time::Instant::now();
    let mut transcribe = load_asr(&models)?;
    println!(
        "ASR load: {:.2}s, RSS {:.0} -> {:.0} MB",
        started.elapsed().as_secs_f64(),
        rss_before,
        rss_mb()
    );
    let refs = std::fs::read_to_string(dir.join("refs.txt"))?;
    let (mut errors, mut total, mut latency, mut audio_seconds) = (0usize, 0usize, 0.0, 0.0);
    for (index, reference) in refs.lines().enumerate() {
        let bytes = std::fs::read(dir.join(format!("{:02}.f32", index + 1)))
            .context("Missing synthesized audio")?;
        let audio: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        let started = std::time::Instant::now();
        let (text, rejected) = transcribe(&audio)?;
        let elapsed = started.elapsed().as_secs_f64();
        let seconds = audio.len() as f64 / 16_000.0;
        let (r, h) = (words(reference), words(&text));
        let distance = edit_distance(&r, &h);
        errors += distance;
        total += r.len();
        latency += elapsed;
        audio_seconds += seconds;
        println!(
            "{:02} {:5.2}s audio {:5.0} ms  WE {distance}/{}{}  | {text}",
            index + 1,
            seconds,
            elapsed * 1000.0,
            r.len(),
            if rejected { " REJECTED" } else { "" },
        );
    }
    println!(
        "WER {:.1}% ({errors}/{total}, all utterances), mean latency {:.0} ms, RTF {:.3}, RSS {:.0} MB",
        100.0 * errors as f64 / total as f64,
        1000.0 * latency / UTTERANCES.len() as f64,
        latency / audio_seconds,
        rss_mb()
    );
    let silence = vec![0.0f32; 16_000];
    let noise: Vec<f32> = (0..32_000)
        .map(|i| (((i as u32).wrapping_mul(2_654_435_761) >> 16) as f32 / 65_535.0 - 0.5) * 0.02)
        .collect();
    for (name, audio) in [("silence", silence), ("low noise", noise)] {
        let (text, rejected) = transcribe(&audio)?;
        println!("{name}: rejected={rejected} text={text:?}");
    }
    Ok(())
}

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let mode = args.next().context("Pass `synth DIR` or `run DIR`")?;
    let dir = args.next().context("Pass a directory")?;
    match mode.to_str() {
        Some("synth") => synth(Path::new(&dir)),
        Some("run") => run(Path::new(&dir)),
        _ => anyhow::bail!("Pass `synth DIR` or `run DIR`"),
    }
}
