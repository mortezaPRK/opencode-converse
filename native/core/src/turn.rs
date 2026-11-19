//! Turn detection: VAD-gated utterance assembly + Smart Turn end-of-turn decision.
//!
//! A stricter barge-in guard reduces triggering on the engine's own playback.

use std::collections::VecDeque;

/// 16 kHz mono frame size the VAD consumes (32 ms).
pub const VAD_FRAME: usize = 512;
const SR: usize = 16_000;
/// Strict barge-in guard (also the pre-roll length, ~480 ms): consecutive frames at
/// p > 0.85 while we are speaking. Deliberately conservative until acoustic echo
/// cancellation exists; do not lower it before AEC lands, or playback bleeding into
/// the mic will interrupt the reply. Pre-roll is exactly the trigger window, so a
/// quieter onset that preceded it (p <= 0.85) is not kept.
const BARGE_FRAMES: usize = 15;
const MAX_CAPTURE_SECONDS: usize = 120;

fn capture_limit_reached(buf: &mut Vec<f32>, frame: &[f32]) -> bool {
    buf.extend_from_slice(frame);
    buf.len() >= MAX_CAPTURE_SECONDS * SR
}

fn finish_utterance(buf: &mut Vec<f32>, p_complete: f32) -> TurnEvent {
    TurnEvent::Utterance {
        audio: std::mem::take(buf),
        p_complete,
    }
}

fn triggering_voiced_samples(normal_run: u32, strict_run: u32, strict: bool) -> usize {
    (if strict { strict_run } else { normal_run }) as usize * VAD_FRAME
}

fn meets_speech_minimum(voiced_samples: usize, minimum: f32) -> bool {
    voiced_samples as f32 / SR as f32 >= minimum
}

fn push_pre(pre: &mut VecDeque<Vec<f32>>, frame: Vec<f32>) {
    if pre.len() == BARGE_FRAMES {
        pre.pop_front();
    }
    pre.push_back(frame);
}

fn candidate_start(
    run: &mut u32,
    strict_run: &mut u32,
    pre: &mut VecDeque<Vec<f32>>,
    frame: &[f32],
    p: f32,
    strict: bool,
    start_frames: u32,
) -> bool {
    push_pre(pre, frame.to_vec());
    *run = if p > 0.5 { run.saturating_add(1) } else { 0 };
    *strict_run = if p > 0.85 {
        strict_run.saturating_add(1)
    } else {
        0
    };
    if strict {
        *strict_run >= BARGE_FRAMES as u32
    } else {
        *run >= start_frames
    }
}

/// How long the detector waits in silence before ending the user's turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Endpointing {
    /// ~256 ms of silence before consulting Smart Turn; hard stop at ~1.9 s.
    #[default]
    Responsive,
    /// ~640 ms before consulting Smart Turn; hard stop at ~3 s.
    Patient,
}

impl Endpointing {
    /// (min_silence, max_silence) in 32 ms VAD frames.
    fn silence_frames(self) -> (u32, u32) {
        match self {
            Self::Responsive => (8, 60),
            Self::Patient => (20, 94),
        }
    }
}

/// What the turn detector decided after consuming a frame.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEvent {
    /// The user started speaking.
    SpeechStart,
    /// Speech ended and the turn looks complete. Carries the utterance audio.
    Utterance { audio: Vec<f32>, p_complete: f32 },
}

/// VAD + Smart Turn state machine.
pub struct TurnDetector {
    vad: ve_vad::SileroVad,
    turn: ve_turn::SmartTurn,
    state: TurnState,
}

// The same transitions run with native model scores and test-only scripted
// scores. This is private Rust state, not a runtime model/configuration override.
trait Scores {
    fn voice(&mut self, frame: &[f32]) -> anyhow::Result<f32>;
    fn completion(&mut self, audio: &[f32]) -> anyhow::Result<f32>;
    fn reset_voice(&mut self);
}

struct NativeScores<'a> {
    vad: &'a mut ve_vad::SileroVad,
    turn: &'a ve_turn::SmartTurn,
}

impl Scores for NativeScores<'_> {
    fn voice(&mut self, frame: &[f32]) -> anyhow::Result<f32> {
        self.vad.prob(frame)
    }
    fn completion(&mut self, audio: &[f32]) -> anyhow::Result<f32> {
        self.turn.prob(audio)
    }
    fn reset_voice(&mut self) {
        self.vad.reset();
    }
}

struct TurnState {
    // frame assembly
    pending: Vec<f32>,

    // end-of-turn bookkeeping
    in_speech: bool,
    buf: Vec<f32>,
    pre: VecDeque<Vec<f32>>,

    run: u32,
    strict_run: u32,
    silence: u32,
    checked: bool,
    frames_since_check: u32,
    voiced_samples: usize,

    // config
    start_frames: u32,
    min_silence: u32,
    max_silence: u32,
    min_speech: f32,
    turn_threshold: f32,
}

impl TurnDetector {
    pub fn new(
        vad_model: &std::path::Path,
        turn_model: &std::path::Path,
        endpointing: Endpointing,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            vad: ve_vad::SileroVad::from_model(vad_model)?,
            turn: ve_turn::SmartTurn::from_model(turn_model)?,
            state: TurnState::with_endpointing(endpointing),
        })
    }

    pub fn reset(&mut self) {
        self.state.reset(&mut NativeScores {
            vad: &mut self.vad,
            turn: &self.turn,
        });
    }

    /// Feed 16 kHz mono PCM; native and scripted scores share these transitions.
    /// Strict playback detection requires fifteen consecutive louder VAD frames.
    pub fn feed(&mut self, samples: &[f32], strict: bool) -> anyhow::Result<Vec<TurnEvent>> {
        self.state.feed(
            samples,
            strict,
            &mut NativeScores {
                vad: &mut self.vad,
                turn: &self.turn,
            },
        )
    }
}

impl Default for TurnState {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            in_speech: false,
            buf: Vec::new(),
            pre: VecDeque::with_capacity(BARGE_FRAMES),
            run: 0,
            strict_run: 0,
            silence: 0,
            checked: false,
            frames_since_check: 0,
            voiced_samples: 0,
            start_frames: 5,
            // 8 frames ~= 256 ms of silence before consulting Smart Turn.
            min_silence: 8,
            // Hard stop: give up waiting and flush at ~1.9 s of silence.
            max_silence: 60,
            min_speech: 0.3,
            turn_threshold: 0.5,
        }
    }
}

impl TurnState {
    fn with_endpointing(endpointing: Endpointing) -> Self {
        let (min_silence, max_silence) = endpointing.silence_frames();
        Self {
            min_silence,
            max_silence,
            ..Self::default()
        }
    }

    fn reset(&mut self, scores: &mut impl Scores) {
        self.in_speech = false;
        self.buf.clear();
        self.run = 0;
        self.strict_run = 0;
        self.silence = 0;
        self.checked = false;
        self.frames_since_check = 0;
        self.voiced_samples = 0;
        self.pre.clear();
        self.pending.clear();
        scores.reset_voice();
    }

    /// Feed 16 kHz mono samples of any length. Internally re-frames to
    /// [`VAD_FRAME`] and returns one event per state change.
    ///
    /// `strict` enables the barge-in guard: while the engine is speaking we
    /// require `barge_frames` louder frames before believing the user really
    /// spoke, so playback bleeding into the mic cannot interrupt us.
    fn feed(
        &mut self,
        samples: &[f32],
        strict: bool,
        scores: &mut impl Scores,
    ) -> anyhow::Result<Vec<TurnEvent>> {
        self.pending.extend_from_slice(samples);
        let mut out = Vec::new();

        while self.pending.len() >= VAD_FRAME {
            let frame: Vec<f32> = self.pending.drain(..VAD_FRAME).collect();
            if let Some(ev) = self.feed_frame(&frame, strict, scores)? {
                out.push(ev);
            }
        }
        Ok(out)
    }

    fn feed_frame(
        &mut self,
        frame: &[f32],
        strict: bool,
        scores: &mut impl Scores,
    ) -> anyhow::Result<Option<TurnEvent>> {
        let p = scores.voice(frame)?;

        if !self.in_speech {
            if candidate_start(
                &mut self.run,
                &mut self.strict_run,
                &mut self.pre,
                frame,
                p,
                strict,
                self.start_frames,
            ) {
                self.in_speech = true;
                self.buf = self.pre.iter().flatten().copied().collect();
                // Credit the qualifying voiced trigger, never silent preroll.
                self.voiced_samples = triggering_voiced_samples(self.run, self.strict_run, strict);
                self.silence = 0;
                self.checked = false;
                self.frames_since_check = 0;
                return Ok(Some(TurnEvent::SpeechStart));
            }
            return Ok(None);
        }

        // Bound capture memory and ASR work even for continuous speech/noise.
        if capture_limit_reached(&mut self.buf, frame) {
            let event = finish_utterance(&mut self.buf, 1.0);
            self.reset_speech(scores);
            return Ok(Some(event));
        }
        self.frames_since_check += 1;

        if p > 0.5 {
            self.voiced_samples += frame.len();
            self.silence = 0;
            self.checked = false;
            return Ok(None);
        }
        if p < 0.35 {
            self.silence += 1;
        }

        // Hard flush is a *silence* timeout, not a speech-length limit: a long
        // utterance with no pauses must not be cut off mid-sentence.
        let too_long = self.silence >= self.max_silence;
        if (self.silence >= self.min_silence && !self.checked) || too_long {
            self.checked = true;
            if !meets_speech_minimum(self.voiced_samples, self.min_speech) && !too_long {
                return Ok(None);
            }
            let p_complete = scores.completion(&self.buf)?;
            if p_complete >= self.turn_threshold || too_long {
                let event = finish_utterance(&mut self.buf, p_complete);
                self.reset_speech(scores);
                return Ok(Some(event));
            }
        }
        Ok(None)
    }

    fn reset_speech(&mut self, scores: &mut impl Scores) {
        self.in_speech = false;
        self.run = 0;
        self.strict_run = 0;
        self.silence = 0;
        self.checked = false;
        self.frames_since_check = 0;
        self.voiced_samples = 0;
        self.pre.clear();
        scores.reset_voice();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct ScriptedScores {
        turns: VecDeque<f32>,
        frame_clock: usize,
        checks: Vec<usize>,
        resets: usize,
        last_voice: Option<f32>,
    }

    impl Scores for ScriptedScores {
        fn voice(&mut self, frame: &[f32]) -> anyhow::Result<f32> {
            self.frame_clock += 1;
            self.last_voice = Some(frame[0]);
            Ok(frame[0])
        }
        fn completion(&mut self, _audio: &[f32]) -> anyhow::Result<f32> {
            self.checks.push(self.frame_clock);
            Ok(self.turns.pop_front().unwrap_or(0.9))
        }
        fn reset_voice(&mut self) {
            self.resets += 1;
        }
    }

    struct Fixture {
        state: TurnState,
        scores: ScriptedScores,
    }

    impl Fixture {
        fn new(turns: &[f32]) -> Self {
            Self {
                state: TurnState::default(),
                scores: ScriptedScores {
                    turns: turns.iter().copied().collect(),
                    ..Default::default()
                },
            }
        }
        fn feed(&mut self, samples: &[f32], strict: bool) -> Vec<TurnEvent> {
            self.state.feed(samples, strict, &mut self.scores).unwrap()
        }
        fn frames(&mut self, count: usize, probability: f32, strict: bool) -> Vec<TurnEvent> {
            (0..count)
                .flat_map(|_| self.feed(&[probability; VAD_FRAME], strict))
                .collect()
        }
        fn reset(&mut self) {
            self.state.reset(&mut self.scores);
        }
    }

    #[test]
    fn continuous_45_second_request_has_one_hearing_and_one_true_end_utterance() {
        let mut f = Fixture::new(&[0.9]);
        let voiced = (45 * SR).div_ceil(VAD_FRAME);
        assert_eq!(f.frames(voiced, 0.9, false), [TurnEvent::SpeechStart]);
        assert!(f.scores.checks.is_empty());
        assert!(matches!(
            f.frames(8, 0.0, false).as_slice(),
            [TurnEvent::Utterance { .. }]
        ));
        assert_eq!(f.scores.checks, [voiced + 8]);
        assert_eq!(f.scores.resets, 1);
        assert!(!f.state.in_speech);
    }
    #[test]
    fn continuous_noise_capture_remains_bounded_by_the_fixed_limit() {
        let mut f = Fixture::new(&[]);
        let events = f.frames((MAX_CAPTURE_SECONDS * SR).div_ceil(VAD_FRAME), 0.9, false);
        let [TurnEvent::SpeechStart, TurnEvent::Utterance { audio, p_complete }] =
            events.as_slice()
        else {
            panic!("expected the bounded forced completion");
        };
        assert!(*p_complete == 1.0 && audio.len() <= MAX_CAPTURE_SECONDS * SR + VAD_FRAME);
        assert!(f.state.buf.is_empty() && !f.state.in_speech);
        assert!(f.scores.checks.is_empty());
        assert_eq!(f.scores.resets, 1);
    }
    #[test]
    fn silent_preroll_cannot_make_a_short_voice_burst_meet_the_minimum() {
        for idle in [0, 10, 100] {
            let mut f = Fixture::new(&[0.9]);
            assert!(f.frames(idle, 0.0, false).is_empty());
            assert_eq!(f.frames(5, 0.9, false), [TurnEvent::SpeechStart]);
            assert!(f.frames(8, 0.0, false).is_empty());
            assert!(
                f.scores.checks.is_empty(),
                "idle={idle}: silent preroll met the minimum"
            );
            assert!(f.frames(8, 0.9, false).is_empty());
            assert!(matches!(
                f.frames(8, 0.0, false).as_slice(),
                [TurnEvent::Utterance { .. }]
            ));
            assert_eq!(f.scores.checks.len(), 1);
        }
    }
    #[test]
    fn normal_and_strict_trigger_credit_allows_completion_at_first_turn_check() {
        for (strict, voiced_frames) in [(false, 13), (true, 15)] {
            let mut f = Fixture::new(&[0.9]);
            assert_eq!(
                f.frames(voiced_frames, 0.9, strict),
                [TurnEvent::SpeechStart]
            );
            assert!(f.frames(7, 0.0, strict).is_empty());
            assert!(f.scores.checks.is_empty());
            assert!(
                matches!(
                    f.frames(1, 0.0, strict).as_slice(),
                    [TurnEvent::Utterance { .. }]
                ),
                "strict={strict}: first check was deferred"
            );
            assert_eq!(f.scores.checks, [voiced_frames + 8]);
            assert_eq!(f.scores.resets, 1);
        }
    }
    #[test]
    fn incomplete_turn_resumes_and_two_true_utterances_reset_the_state() {
        let mut f = Fixture::new(&[0.1, 0.9, 0.9]);
        assert_eq!(f.frames(13, 0.9, false), [TurnEvent::SpeechStart]);
        assert!(f.frames(8, 0.0, false).is_empty());
        assert_eq!(f.scores.checks, [21]);
        assert!(f.frames(6, 0.0, false).is_empty());
        assert_eq!(f.scores.checks, [21]);
        assert!(f.frames(5, 0.9, false).is_empty());
        assert!(matches!(
            f.frames(8, 0.0, false).as_slice(),
            [TurnEvent::Utterance { .. }]
        ));
        assert_eq!(f.scores.checks, [21, 40]);
        assert!(!f.state.in_speech && f.state.buf.is_empty() && f.state.pre.is_empty());
        assert_eq!(f.frames(13, 0.9, false), [TurnEvent::SpeechStart]);
        assert!(matches!(
            f.frames(8, 0.0, false).as_slice(),
            [TurnEvent::Utterance { .. }]
        ));
        assert_eq!(f.scores.resets, 2);
    }
    #[test]
    fn incomplete_turn_forces_completion_at_the_sixty_frame_silence_timeout() {
        let mut f = Fixture::new(&[0.1, 0.1]);
        assert_eq!(f.frames(13, 0.9, false), [TurnEvent::SpeechStart]);
        assert!(f.frames(59, 0.0, false).is_empty());
        assert_eq!(f.scores.checks, [21]);
        assert!(
            matches!(f.frames(1, 0.0, false).as_slice(), [TurnEvent::Utterance { p_complete, .. }] if *p_complete == 0.1)
        );
        assert_eq!(f.scores.checks, [21, 73]);
        assert_eq!(f.scores.resets, 1);
    }
    #[test]
    fn reset_discards_partial_frame_and_allows_a_fresh_turn() {
        let mut f = Fixture::new(&[0.9]);
        assert!(f.feed(&[0.9; VAD_FRAME / 2], false).is_empty());
        assert_eq!(f.scores.frame_clock, 0);
        f.reset();
        assert!(f.state.pending.is_empty());
        assert!(f.feed(&[0.0; VAD_FRAME / 2], false).is_empty());
        assert!(f.feed(&[0.9; VAD_FRAME / 2], false).is_empty());
        assert_eq!(f.scores.last_voice, Some(0.0));
        assert!(f.frames(4, 0.9, false).is_empty());
        assert_eq!(f.frames(9, 0.9, false), [TurnEvent::SpeechStart]);
        assert!(matches!(
            f.frames(8, 0.0, false).as_slice(),
            [TurnEvent::Utterance { .. }]
        ));
        assert_eq!(f.scores.resets, 2);
    }
    #[test]
    fn partial_feeds_share_the_production_vad_frame_assembler() {
        let mut f = Fixture::new(&[0.9]);
        assert!(f.feed(&[0.9; 1], false).is_empty());
        assert_eq!(f.scores.frame_clock, 0);
        assert!(f.feed(&[0.9; VAD_FRAME - 1], false).is_empty());
        assert_eq!(f.scores.frame_clock, 1);
        assert!(f.frames(3, 0.9, false).is_empty());
        assert!(f.feed(&[0.9; VAD_FRAME / 2], false).is_empty());
        assert_eq!(
            f.feed(&[0.9; VAD_FRAME / 2], false),
            [TurnEvent::SpeechStart]
        );
        assert_eq!(f.scores.frame_clock, 5);
        assert!(f.frames(8, 0.9, false).is_empty());
        let events = f.frames(8, 0.0, false);
        assert!(
            matches!(events.as_slice(), [TurnEvent::Utterance { audio, .. }] if audio.len() == 21 * VAD_FRAME)
        );
    }
    #[test]
    fn production_transitions_use_independent_counters_across_strict_toggles() {
        let mut f = Fixture::new(&[0.9]);
        assert!(f.frames(4, 0.6, false).is_empty());
        assert!(f.frames(14, 0.9, true).is_empty());
        assert_eq!(f.frames(1, 0.9, true), [TurnEvent::SpeechStart]);
        assert_eq!(f.state.buf.len(), BARGE_FRAMES * VAD_FRAME);
        assert!(f.state.buf.iter().all(|sample| *sample == 0.9));
        assert!(matches!(
            f.frames(8, 0.0, true).as_slice(),
            [TurnEvent::Utterance { .. }]
        ));
        assert!(f.frames(4, 0.9, false).is_empty());
        assert!(f.frames(10, 0.9, true).is_empty());
        assert_eq!(f.frames(1, 0.9, true), [TurnEvent::SpeechStart]);
    }
    #[test]
    fn trigger_credit_uses_the_selected_threshold_counter() {
        assert_eq!(triggering_voiced_samples(5, 0, false), 5 * VAD_FRAME);
        assert_eq!(triggering_voiced_samples(100, 15, true), 15 * VAD_FRAME);
    }
    #[test]
    fn strict_guard_does_not_reuse_weak_normal_mode_candidates() {
        let mut run = 0;
        let mut strict_run = 0;
        let mut pre = VecDeque::new();
        let mut feed = |p, strict| {
            candidate_start(
                &mut run,
                &mut strict_run,
                &mut pre,
                &[p; VAD_FRAME],
                p,
                strict,
                5,
            )
        };
        for _ in 0..4 {
            assert!(!feed(0.6, false));
        }
        for _ in 0..14 {
            assert!(!feed(0.9, true));
        }
        assert!(feed(0.9, true));
        assert_eq!(pre.len(), BARGE_FRAMES);
        assert!(pre.iter().flatten().all(|sample| *sample == 0.9));
    }
    #[test]
    fn strict_guard_retains_frames_that_already_met_its_threshold() {
        let mut normal = 0;
        let mut strict_run = 0;
        let mut pre = VecDeque::new();
        let mut feed = |strict| {
            candidate_start(
                &mut normal,
                &mut strict_run,
                &mut pre,
                &[0.9; VAD_FRAME],
                0.9,
                strict,
                5,
            )
        };
        for _ in 0..4 {
            assert!(!feed(false));
        }
        for _ in 0..10 {
            assert!(!feed(true));
        }
        assert!(feed(true));
    }

    #[test]
    fn vad_frame_is_32ms() {
        assert_eq!(VAD_FRAME, 512);
        assert_eq!(VAD_FRAME as f32 / SR as f32, 0.032);
    }
    #[test]
    fn strict_preroll_retains_the_entire_detection_window() {
        let mut pre = VecDeque::new();
        for id in 0..BARGE_FRAMES {
            push_pre(&mut pre, vec![id as f32; VAD_FRAME]);
        }
        let audio: Vec<f32> = pre.iter().flatten().copied().collect();
        assert_eq!(audio.len(), BARGE_FRAMES * VAD_FRAME);
        assert_eq!(audio[0], 0.0);
        push_pre(&mut pre, vec![99.0; VAD_FRAME]);
        assert_eq!(pre.len(), BARGE_FRAMES);
        assert_eq!(pre.front().unwrap()[0], 1.0);
    }
    #[test]
    fn barge_in_utterance_keeps_the_whole_strict_trigger_window() {
        let mut f = Fixture::new(&[0.9]);
        assert!(f.frames(30, 0.0, true).is_empty());
        assert_eq!(f.frames(BARGE_FRAMES, 0.9, true), [TurnEvent::SpeechStart]);
        let events = f.frames(8, 0.0, true);
        let [TurnEvent::Utterance { audio, .. }] = events.as_slice() else {
            panic!("expected utterance");
        };
        // The ~480 ms that tripped the guard are all sent to ASR; earlier silence is not.
        assert!(audio[..BARGE_FRAMES * VAD_FRAME].iter().all(|s| *s == 0.9));
        assert_eq!(audio.len(), (BARGE_FRAMES + 8) * VAD_FRAME);
    }
    #[test]
    fn normal_start_preroll_keeps_quiet_audio_before_the_trigger() {
        let mut f = Fixture::new(&[0.9]);
        assert!(f.frames(6, 0.2, false).is_empty());
        assert_eq!(f.frames(5, 0.9, false), [TurnEvent::SpeechStart]);
        f.frames(5, 0.9, false);
        f.frames(7, 0.0, false);
        let events = f.frames(1, 0.0, false);
        let [TurnEvent::Utterance { audio, .. }] = events.as_slice() else {
            panic!("expected utterance");
        };
        assert_eq!(audio[0], 0.2);
        assert_eq!(audio.iter().filter(|s| **s == 0.2).count(), 6 * VAD_FRAME);
    }
    #[test]
    fn patient_endpointing_waits_longer_than_responsive() {
        let silence_until_check = |endpointing| {
            let mut f = Fixture::new(&[0.9]);
            f.state = TurnState::with_endpointing(endpointing);
            assert_eq!(f.frames(13, 0.9, false), [TurnEvent::SpeechStart]);
            f.frames(8, 0.9, false);
            let mut n = 0;
            while f.frames(1, 0.0, false).is_empty() {
                n += 1;
            }
            n + 1
        };
        assert_eq!(silence_until_check(Endpointing::Responsive), 8);
        assert_eq!(silence_until_check(Endpointing::Patient), 20);
        assert_eq!(Endpointing::default(), Endpointing::Responsive);
        assert_eq!(
            Endpointing::Patient.silence_frames().1 as usize * VAD_FRAME,
            94 * 512
        );
    }
}
