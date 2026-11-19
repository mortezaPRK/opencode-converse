//! Voice listening and latency spike. Local files only.
//! Usage: voice_spike <phonon dir> <voices dir> <out dir> [voice id ...]
use anyhow::Result;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

fn wav(path: &Path, rate: u32, samples: &[f32]) -> Result<()> {
    let bytes = (samples.len() * 2) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + bytes).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&bytes.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    std::fs::write(path, out)?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut tts = ve_tts::Tts::new(ve_tts::TtsConfig {
        dir: args[0].clone().into(),
        voices_dir: Some(args[1].clone().into()),
    })?;
    let out = Path::new(&args[2]);
    std::fs::create_dir_all(out)?;
    let voices: Vec<String> = if args.len() > 3 {
        args[3..].to_vec()
    } else {
        tts.voices()
    };
    let never = AtomicBool::new(false);
    let text = "Hi, I checked the build. Two tests failed, and I can fix them if you like.";
    let t = Instant::now();
    tts.synth("Warm up.", &never)?;
    println!("warmup {:?}", t.elapsed());
    let rate = tts.sample_rate();
    let mut prev: Option<String> = None;
    for round in 0..2 {
        for v in &voices {
            let t = Instant::now();
            let mut first = None;
            let mut all = Vec::new();
            tts.synth_stream_voice(text, Some(v), &never, |c| {
                if first.is_none() {
                    first = Some(t.elapsed());
                }
                all.extend_from_slice(c);
                Ok(())
            })?;
            println!(
                "round {round} voice {v} (after {:?}) ttfa {:?} total {:?} audio {:.2}s",
                prev,
                first.unwrap(),
                t.elapsed(),
                all.len() as f32 / rate as f32
            );
            if round == 0 {
                wav(&out.join(format!("{v}.wav")), rate, &all)?;
            }
            prev = Some(v.clone());
        }
    }
    // same-voice repeat
    for _ in 0..2 {
        let v = &voices[0];
        let t = Instant::now();
        let mut first = None;
        tts.synth_stream_voice(text, Some(v), &never, |_| {
            first.get_or_insert(t.elapsed());
            Ok(())
        })?;
        println!("same voice {v} ttfa {:?}", first.unwrap());
    }
    Ok(())
}
