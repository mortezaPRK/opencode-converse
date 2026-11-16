//! Joining independently synthesised chunks so the result sounds like one
//! utterance.
//!
//! Each TTS call is a complete synthesis: the model pads its output with
//! silence and renders utterance-final prosody. Concatenating those raw makes
//! audible seams — measured on one reply, the six-word chunking inserted ~170 ms
//! of dead air across three joins.
//!
//! [`Joiner`] fixes the *physical* half of that problem (the prosody half is the
//! chunker's job, by not cutting mid-phrase):
//!
//! * trims leading/trailing silence from every chunk, keeping a small guard
//!   band around the speech so quiet onsets and fricatives survive;
//! * inserts no gap at all where the text continues mid-sentence, and a short
//!   natural gap only where a sentence really ended;
//! * applies a couple of milliseconds of fade at each join so butted-together
//!   speech does not click.
//!
//! The joiner is stateful across chunks and must be used from one thread.

/// Quietest sample still considered speech, in the model's [-1, 1] range.
const SILENCE_THRESHOLD: f32 = 0.008;
/// Kept either side of the detected speech so onsets are not clipped.
const GUARD_MS: f32 = 12.0;
/// Fade applied at each cut so a hard splice cannot click. Short enough to be
/// inaudible, long enough to remove the discontinuity.
const FADE_MS: f32 = 1.0;
/// Default gap after a clause break (a comma and friends).
///
/// This is deliberately not zero: a comma *is* a pause in speech. Deleting it
/// makes the delivery sound rushed and mechanical — which is exactly what
/// "robotic" turns out to mean here.
pub const DEFAULT_CLAUSE_GAP_MS: f32 = 130.0;
/// Default gap after a sentence end.
pub const DEFAULT_SENTENCE_GAP_MS: f32 = 300.0;

/// How a chunk relates to the one before it, decided from the *text* of the
/// previous chunk so the pause matches what was actually said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Join {
    /// First chunk of the utterance: nothing before it.
    First,
    /// The previous chunk was cut mid-phrase (a word cap, or the LLM pausing
    /// between tokens). No pause — the speech simply continues.
    MidPhrase,
    /// The previous chunk ended on a comma or similar: a short pause.
    Clause,
    /// The previous chunk ended a sentence: a full pause.
    Sentence,
}

/// Stitches successive [`Chunk`]s into one continuous stream of audio.
///
/// ```no_run
/// # use ve_tts::Joiner;
/// let mut j = Joiner::new(22_050);
/// let first = j.push("Sure, I can help with that.", &[0.5f32; 100]);
/// let second = j.push("It may take a moment.", &[0.5; 100]);
/// let tail = j.flush();
/// assert!(!first.is_empty());
/// assert!(!second.is_empty());
/// assert!(tail.is_empty());
/// ```
pub struct Joiner {
    rate: u32,
    guard: usize,
    fade: usize,
    /// Silence inserted after a sentence end.
    sentence_gap: usize,
    /// Silence inserted after a clause break.
    clause_gap: usize,
    /// True once a chunk has been emitted, so the next one is not "First".
    started: bool,
    /// What kind of boundary the previous chunk ended on.
    prev_join: Join,
}

impl Joiner {
    /// `rate` is the sample rate the chunks arrive at (the TTS model's rate).
    pub fn new(rate: u32) -> Self {
        let ms = |m: f32| ((rate as f32) * m / 1000.0) as usize;
        Self {
            rate,
            guard: ms(GUARD_MS),
            fade: ms(FADE_MS).max(1),
            sentence_gap: ms(DEFAULT_SENTENCE_GAP_MS),
            clause_gap: ms(DEFAULT_CLAUSE_GAP_MS),
            started: false,
            prev_join: Join::First,
        }
    }

    /// Gap inserted after a sentence end, in milliseconds (default 300).
    pub fn with_sentence_gap_ms(mut self, ms: f32) -> Self {
        self.sentence_gap = ((self.rate as f32) * ms / 1000.0) as usize;
        self
    }

    /// Gap inserted after a clause break, in milliseconds (default 130).
    /// Set to `0.0` for the tightest possible delivery.
    pub fn with_clause_gap_ms(mut self, ms: f32) -> Self {
        self.clause_gap = ((self.rate as f32) * ms / 1000.0) as usize;
        self
    }

    /// Add one synthesised chunk, together with the text it was synthesised
    /// from. Returns the samples that can be played immediately.
    pub fn push(&mut self, text: &str, samples: &[f32]) -> Vec<f32> {
        let trimmed = self.trim(samples);
        if trimmed.is_empty() {
            // Nothing audible (blank text or pure silence); do not insert a gap.
            return Vec::new();
        }

        let join = if !self.started {
            Join::First
        } else {
            self.prev_join
        };

        let mut out = Vec::with_capacity(trimmed.len() + self.sentence_gap);
        let gap = match join {
            Join::First | Join::MidPhrase => 0,
            Join::Clause => self.clause_gap,
            Join::Sentence => self.sentence_gap,
        };
        out.extend(std::iter::repeat_n(0.0, gap));
        out.extend_from_slice(&trimmed);
        self.fade_edges(&mut out[gap..]);

        self.started = true;
        self.prev_join = boundary_of(text);
        out
    }

    /// Flush any rounding error. Kept for symmetry: the joiner holds nothing
    /// back, so this is normally empty.
    pub fn flush(&mut self) -> Vec<f32> {
        Vec::new()
    }

    /// Reset for a new utterance (after barge-in, for example).
    pub fn reset(&mut self) {
        self.started = false;
        self.prev_join = Join::First;
    }

    /// Strip leading and trailing silence, keeping a guard band of speech-level
    /// signal around the utterance.
    fn trim(&self, samples: &[f32]) -> Vec<f32> {
        let first = samples.iter().position(|s| s.abs() > SILENCE_THRESHOLD);
        let Some(first) = first else {
            return Vec::new();
        };
        let last = samples
            .iter()
            .rposition(|s| s.abs() > SILENCE_THRESHOLD)
            .unwrap_or(first);

        let start = first.saturating_sub(self.guard);
        let end = (last + 1 + self.guard).min(samples.len());
        samples[start..end].to_vec()
    }

    /// Fade the speech edges, excluding any inserted gap, so splices cannot click.
    fn fade_edges(&self, buf: &mut [f32]) {
        if buf.len() == 1 {
            buf[0] = 0.0;
            return;
        }
        let n = self.fade.min(buf.len() / 2);
        if n == 0 {
            return;
        }
        let len = buf.len();
        for (i, s) in buf[..n].iter_mut().enumerate() {
            *s *= i as f32 / n as f32;
        }
        for (i, s) in buf[len - n..].iter_mut().enumerate() {
            *s *= if n == 1 {
                0.0
            } else {
                (n - 1 - i) as f32 / (n - 1) as f32
            };
        }
    }
}

/// Whether `text` ends a sentence, ignoring trailing quotes/brackets.
fn ends_sentence(text: &str) -> bool {
    last_significant(text)
        .is_some_and(|c| matches!(c, '.' | '!' | '?' | '\u{2026}' | '。' | '！' | '？'))
}

/// Whether `text` ends a clause — a break a speaker would actually pause at.
fn ends_clause(text: &str) -> bool {
    last_significant(text)
        .is_some_and(|c| matches!(c, ',' | ';' | ':' | '\u{2014}' | '-' | '\u{2013}'))
}

/// Classify the boundary `text` leaves behind, for the *next* chunk to honour.
fn boundary_of(text: &str) -> Join {
    if ends_sentence(text) {
        Join::Sentence
    } else if ends_clause(text) {
        Join::Clause
    } else {
        Join::MidPhrase
    }
}

/// Last character of `text` that is not whitespace or a closing quote/bracket.
fn last_significant(text: &str) -> Option<char> {
    text.trim_end()
        .chars()
        .rev()
        .find(|c| !matches!(c, '"' | '\'' | ')' | ']' | '\u{201d}' | '\u{2019}' | '}'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 1000; // 1 sample per ms keeps the arithmetic readable

    fn tone(ms: usize) -> Vec<f32> {
        vec![0.5; ms]
    }

    fn padded(lead: usize, speech: usize, trail: usize) -> Vec<f32> {
        let mut v = vec![0.0; lead];
        v.extend(tone(speech));
        v.extend(vec![0.0; trail]);
        v
    }

    #[test]
    fn unpadded_fade_out_reaches_zero_and_preserves_the_bounded_signal() {
        for amplitude in [0.5, -0.5] {
            let mut j = Joiner::new(22_050);
            let input = vec![amplitude; 1000];
            let out = j.push("One", &input);
            assert_eq!(out.len(), input.len());
            assert_eq!(out[0], 0.0);
            assert_eq!(out[out.len() - 1], 0.0, "unpadded speech must end at zero");
            assert_eq!(out[j.fade], amplitude);
            assert_eq!(out[out.len() - j.fade], amplitude);
            assert!(out
                .iter()
                .all(|sample| sample.is_finite() && sample.abs() <= amplitude.abs()));
            let tail = &out[out.len() - j.fade..];
            assert!(tail
                .windows(2)
                .all(|pair| (pair[1] - pair[0]).abs()
                    <= amplitude.abs() / (j.fade - 1) as f32 + 1e-6));
            let next = j.push("Two", &[-amplitude; 1000]);
            assert_eq!(
                out.last().unwrap() - next.first().unwrap(),
                0.0,
                "actual splice samples must both be zero"
            );
        }
    }

    #[test]
    fn single_sample_and_one_frame_fades_end_at_zero() {
        for rate in [1000, 22_050] {
            for length in [1, 2, 3, 4, 17, 44, 1000] {
                let mut j = Joiner::new(rate);
                let out = j.push("One", &vec![0.5; length]);
                assert_eq!(out.len(), length);
                assert_eq!(out[length - 1], 0.0, "rate={rate}, length={length}");
                assert_eq!(out[0], 0.0);
                assert!(out
                    .iter()
                    .all(|sample| sample.is_finite() && (0.0..=0.5).contains(sample)));
            }
        }
    }

    #[test]
    fn unicode_sentence_boundaries_keep_the_pause_before_actual_speech() {
        for end in ['。', '！', '？'] {
            let mut j = Joiner::new(22_050);
            j.push(&format!("Label{end}"), &[0.5; 1000]);
            let next = j.push("Next", &[-0.5; 1000]);
            assert_eq!(next.len(), j.sentence_gap + 1000);
            assert!(next[..j.sentence_gap].iter().all(|&s| s == 0.0));
            assert_eq!(next[j.sentence_gap + j.fade], -0.5);
        }
    }

    #[test]
    fn unpadded_speech_fades_after_gaps_and_on_first_chunk() {
        let mut j = Joiner::new(22_050);
        for (text, gap) in [
            ("First.", 0),
            ("Second,", j.sentence_gap),
            ("Third", j.clause_gap),
            ("Fourth", 0),
        ] {
            let out = j.push(text, &[0.5; 1000]);
            assert!(out[..gap].iter().all(|&s| s == 0.0));
            assert_eq!(
                out[gap], 0.0,
                "speech onset after {gap} silence samples must fade"
            );
            assert!(out[gap..gap + j.fade]
                .windows(2)
                .all(|w| (w[1] - w[0]).abs() < 0.03));
            assert_eq!(out[gap + j.fade], 0.5);
        }
    }

    #[test]
    fn trims_edge_silence() {
        let mut j = Joiner::new(RATE);
        let out = j.push("Hello.", &padded(100, 200, 150));
        // 200 ms of speech plus a 12 ms guard either side.
        assert!((out.len() as i64 - 224).abs() <= 2, "got {}", out.len());
    }

    #[test]
    fn mid_phrase_cut_gets_no_gap() {
        let mut j = Joiner::new(RATE);
        let _ = j.push("I started the task", &padded(50, 100, 80));
        let b = j.push("and it is running.", &padded(50, 100, 80));
        // No punctuation: the speaker was mid-phrase, so the speech just continues.
        assert!(b.len() < 200, "expected no gap, got {}", b.len());
    }

    #[test]
    fn clause_break_gets_a_short_pause() {
        let mut j = Joiner::new(RATE);
        let _ = j.push("I started the task,", &padded(50, 100, 80));
        let b = j.push("and it is running.", &padded(50, 100, 80));
        // 130 ms clause gap + ~124 ms of audio
        assert!((b.len() as i64 - 254).abs() <= 4, "got {}", b.len());
    }

    #[test]
    fn sentence_end_gets_a_full_pause() {
        let mut j = Joiner::new(RATE);
        let _ = j.push("All done.", &padded(20, 100, 20));
        let b = j.push("Next up.", &padded(20, 100, 20));
        // 300 ms sentence gap + ~124 ms of audio
        assert!((b.len() as i64 - 424).abs() <= 4, "got {}", b.len());
    }

    #[test]
    fn gaps_are_configurable() {
        let mut j = Joiner::new(RATE)
            .with_clause_gap_ms(0.0)
            .with_sentence_gap_ms(0.0);
        let _ = j.push("Done,", &padded(10, 100, 10));
        let b = j.push("more.", &padded(10, 100, 10));
        assert!(b.len() < 200, "expected no gap, got {}", b.len());
    }

    #[test]
    fn punctuation_classification() {
        assert_eq!(boundary_of("All done."), Join::Sentence);
        assert_eq!(boundary_of("Really?"), Join::Sentence);
        assert_eq!(boundary_of("wait,"), Join::Clause);
        assert_eq!(boundary_of("so:"), Join::Clause);
        assert_eq!(boundary_of("mid phrase"), Join::MidPhrase);
    }

    #[test]
    fn silent_chunk_is_dropped_without_a_gap() {
        let mut j = Joiner::new(RATE);
        let _ = j.push("Done.", &padded(10, 100, 10));
        let nothing = j.push("   ", &vec![0.0; 500]);
        assert!(nothing.is_empty());
        // "Done." ended a sentence, so this one is preceded by the sentence gap.
        let after = j.push("More.", &padded(10, 100, 10));
        assert!(
            after.len() > 300,
            "expected the sentence gap, got {}",
            after.len()
        );
    }

    #[test]
    fn no_clicks_at_the_join() {
        let mut j = Joiner::new(RATE);
        for text in ["One,", "two,", "three."] {
            let out = j.push(text, &padded(30, 200, 30));
            // The first and last samples must be quiet enough not to click.
            if let Some(first) = out.first() {
                assert!(first.abs() < 0.1, "loud start: {first}");
            }
            if let Some(last) = out.last() {
                assert!(last.abs() < 0.1, "loud end: {last}");
            }
        }
    }

    #[test]
    fn ends_sentence_handles_quotes() {
        assert!(ends_sentence("He said \"stop.\""));
        assert!(ends_sentence("Really?"));
        assert!(!ends_sentence("wait,"));
        assert!(!ends_sentence("no punctuation"));
        assert!(ends_clause("wait,"));
        assert!(!ends_clause("stop."));
    }

    #[test]
    fn reset_starts_a_fresh_utterance() {
        let mut j = Joiner::new(RATE);
        let _ = j.push("Done.", &padded(10, 100, 10));
        j.reset();
        // After reset the next chunk is "first" again, so no leading gap.
        let out = j.push("Fresh.", &padded(10, 100, 10));
        assert!(out.len() < 200);
    }
}
