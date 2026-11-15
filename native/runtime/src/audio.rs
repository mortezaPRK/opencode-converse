//! Real local audio/inference backend. Model constructors run off the stdio loop;
//! cancellation generations gate the final playback commit under one mutex.
use crate::{protocol::Settings, Action, Input};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, SyncSender},
    Arc, Mutex,
};
use std::time::Duration;

#[path = "../../core/src/turn.rs"]
mod turn;

enum Job {
    Speak {
        epoch: u64,
        text: String,
        cancel: Arc<AtomicBool>,
    },
    Finish(u64),
}
struct Playback {
    audio: Option<Arc<ve_audio::AudioIo>>,
    generation: crate::generation::Generation,
}
impl Default for Playback {
    fn default() -> Self {
        Self {
            audio: None,
            generation: Default::default(),
        }
    }
}
pub struct Backend {
    jobs: SyncSender<Job>,
    playback: Arc<Mutex<Playback>>,
    muted: Arc<AtomicBool>,
    capture_epoch: Arc<AtomicU64>,
    health: Arc<AtomicBool>,
    pending_finish: Arc<AtomicU64>,
}

// Every worker exit (including unwinding) is observable without relying on a
// possibly-full stdio event queue. During shutdown main already prefers EOF.
struct WorkerHealth(Arc<AtomicBool>);
impl Drop for WorkerHealth {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn update_capture<T>(
    capture: &mut Option<T>,
    muted: bool,
    open: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<()> {
    if muted {
        drop(capture.take());
    } else if capture.is_none() {
        *capture = Some(open()?);
    }
    Ok(())
}

/// Explicit test-only model smoke test. Never opens audio devices or logs recognized text.
pub fn test_only_inference() -> anyhow::Result<()> {
    let _lock = crate::paths::lock()?;
    let models = crate::paths::check_models()?;
    ve_asr::silence_logs();
    let mut vad = ve_vad::SileroVad::from_model(&models.join("silero_vad.onnx"))?;
    anyhow::ensure!(
        vad.prob(&[0.0; 512])?.is_finite(),
        "VAD produced a non-finite probability"
    );
    let turn = ve_turn::SmartTurn::from_model(&models.join("smart-turn-v3.2-cpu.onnx"))?;
    anyhow::ensure!(
        turn.prob(&[0.0; 16_000])?.is_finite(),
        "Turn model produced a non-finite probability"
    );
    let mut tts = ve_tts::Tts::new(models.join("tts"))?;
    let samples = tts.synth(
        "Hello. The local speech test is complete.",
        Arc::new(AtomicBool::new(false)),
    )?;
    anyhow::ensure!(!samples.is_empty(), "Piper produced no samples");
    let mut resampler = ve_audio::StreamResampler::new(tts.sample_rate(), 16_000)?;
    let mut samples = resampler.process(&samples);
    samples.extend(resampler.flush());
    let mut asr = ve_asr::WhisperAsr::from_config(ve_asr::AsrConfig {
        model_path: models.join("ggml-base.en.bin"),
        n_threads: 4,
        use_gpu: cfg!(target_os = "macos"),
    })?;
    let recognized = asr.transcribe(&samples)?;
    anyhow::ensure!(
        !recognized.rejected && !recognized.text.trim().is_empty(),
        "Whisper rejected synthesized test speech"
    );
    eprintln!("Test-only headless VAD/turn/Piper/resampling/Whisper inference passed. No device capture/playback was tested.");
    Ok(())
}

fn send(tx: &SyncSender<Input>, event: Input, health: &AtomicBool) -> anyhow::Result<()> {
    // Called only by model/control workers, never a capture callback. Apply
    // bounded backpressure to normal events; actual worker failures are also
    // signaled out-of-band by WorkerHealth even if their error event cannot fit.
    tx.send(event).map_err(|_| {
        health.store(true, Ordering::Release);
        anyhow::anyhow!("Runtime event queue exceeded its fixed capacity")
    })
}

fn capture_ready_before_open<T>(
    ready: &mut bool,
    events: &SyncSender<Input>,
    health: &AtomicBool,
    open: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    if !*ready {
        send(events, Input::Ready, health)?;
        *ready = true;
    }
    open()
}

fn finish_playback(
    state: std::sync::MutexGuard<'_, Playback>,
    events: &SyncSender<Input>,
    epoch: u64,
    health: &AtomicBool,
) -> anyhow::Result<()> {
    state.generation.finish();
    drop(state);
    send(events, Input::Drained(epoch), health)
}

#[derive(Debug)]
struct AsrJob {
    epoch: u64,
    id: u64,
    audio: Vec<f32>,
}

fn capture_gap(last: &mut u64, sequence: u64) -> bool {
    let gap = *last != sequence;
    *last = sequence;
    gap
}

fn capture_event(
    tx: &SyncSender<(u64, u64, Vec<f32>)>,
    epoch: u64,
    continuity: &AtomicU64,
    event: ve_audio::CaptureEvent<'_>,
) {
    match event {
        ve_audio::CaptureEvent::Gap => {
            continuity.fetch_add(1, Ordering::AcqRel);
        }
        ve_audio::CaptureEvent::Frame(frame) => {
            let generation = continuity.load(Ordering::Acquire);
            if tx.try_send((epoch, generation, frame.to_vec())).is_err() {
                // A full channel cannot swallow its own discontinuity marker.
                continuity.fetch_add(1, Ordering::AcqRel);
            }
        }
    }
}

fn new_speech_token(
    previous: &mut Option<(u64, Arc<AtomicBool>)>,
    epoch: u64,
    token: &Arc<AtomicBool>,
) -> bool {
    let changed = previous
        .as_ref()
        .is_none_or(|(old_epoch, old_token)| *old_epoch != epoch || !Arc::ptr_eq(old_token, token));
    if changed {
        *previous = Some((epoch, token.clone()));
    }
    changed
}

fn abandon_capture(
    open: &mut Option<u64>,
    epoch: u64,
    events: &SyncSender<Input>,
    health: &AtomicBool,
) -> anyhow::Result<()> {
    if let Some(id) = open.take() {
        send(events, Input::Transcript(epoch, id, String::new()), health)?;
    }
    Ok(())
}

fn submit_utterance(
    open: &mut Option<u64>,
    epoch: u64,
    audio: Vec<f32>,
    jobs: &SyncSender<AsrJob>,
    events: &SyncSender<Input>,
    health: &AtomicBool,
) -> anyhow::Result<()> {
    if let Some(id) = open.take() {
        if jobs.try_send(AsrJob { epoch, id, audio }).is_err() {
            send(events, Input::Transcript(epoch, id, String::new()), health)?;
        }
    }
    Ok(())
}

fn recognize(
    rx: mpsc::Receiver<AsrJob>,
    events: SyncSender<Input>,
    health: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    capture_epoch: Arc<AtomicU64>,
    mut transcribe: impl FnMut(&[f32]) -> anyhow::Result<String>,
) -> anyhow::Result<()> {
    for job in rx {
        if muted.load(Ordering::Acquire) || capture_epoch.load(Ordering::Acquire) != job.epoch {
            continue;
        }
        let text = transcribe(&job.audio)?;
        // Main checks again at consumption: mute may race even this final check.
        if !muted.load(Ordering::Acquire) && capture_epoch.load(Ordering::Acquire) == job.epoch {
            send(&events, Input::Transcript(job.epoch, job.id, text), &health)?;
        }
    }
    Ok(())
}

impl Backend {
    pub fn start(settings: Settings, events: SyncSender<Input>) -> Self {
        let (jobs, rx) = mpsc::sync_channel::<Job>(crate::MAX_SPEECH_JOBS);
        let playback = Arc::new(Mutex::new(Playback::default()));
        let muted = Arc::new(AtomicBool::new(false));
        let capture_epoch = Arc::new(AtomicU64::new(0));
        let capture_continuity = Arc::new(AtomicU64::new(0));
        let health = Arc::new(AtomicBool::new(false));
        let thread_health = health.clone();
        let pending_finish = Arc::new(AtomicU64::new(0));
        let worker_finish = pending_finish.clone();
        let state = playback.clone();
        let mute = muted.clone();
        let input_epoch = capture_epoch.clone();
        std::thread::spawn(move || {
            let _health_guard = WorkerHealth(thread_health.clone());
            let result = (|| -> anyhow::Result<()> {
                let models = crate::paths::check_models()?;
                ve_asr::silence_logs();
                let mut detector = turn::TurnDetector::new(
                    &models.join("silero_vad.onnx"),
                    &models.join("smart-turn-v3.2-cpu.onnx"),
                )?;
                let mut asr = ve_asr::WhisperAsr::from_config(ve_asr::AsrConfig {
                    model_path: models.join("ggml-base.en.bin"),
                    n_threads: 4,
                    use_gpu: cfg!(target_os = "macos"),
                })?;
                let mut tts = ve_tts::Tts::new(models.join("tts"))?;
                let mut resampler = ve_audio::StreamResampler::new(tts.sample_rate(), 48_000)?;
                let (capture_tx, capture_rx) = mpsc::sync_channel::<(u64, u64, Vec<f32>)>(64);
                let (asr_tx, asr_rx) = mpsc::sync_channel::<AsrJob>(1);
                let asr_events = events.clone();
                let asr_health = thread_health.clone();
                let asr_mute = mute.clone();
                let asr_epoch = input_epoch.clone();
                std::thread::spawn(move || {
                    let _health_guard = WorkerHealth(asr_health.clone());
                    let result = recognize(
                        asr_rx,
                        asr_events.clone(),
                        asr_health,
                        asr_mute,
                        asr_epoch,
                        |audio| {
                            let transcript = asr.transcribe(audio)?;
                            Ok(if transcript.rejected {
                                String::new()
                            } else {
                                transcript.text
                            })
                        },
                    );
                    if let Err(e) = result {
                        let _ = asr_events.try_send(Input::Failure("asr_error", e.to_string()));
                    }
                });
                let config = ve_audio::AudioConfig {
                    input_device: settings.input_device,
                    output_device: settings.output_device,
                    ..Default::default()
                };
                let audio = Arc::new(ve_audio::AudioIo::start(config.clone())?);
                state.lock().unwrap().audio = Some(audio.clone());
                // Only this manager owns the microphone stream. Mute drops it;
                // unmute creates a fresh stream, independently of output/models.
                let manager_mute = mute.clone();
                let manager_epoch = input_epoch.clone();
                let manager_health = thread_health.clone();
                let manager_events = events.clone();
                let manager_continuity = capture_continuity.clone();
                std::thread::spawn(move || {
                    let _health_guard = WorkerHealth(manager_health.clone());
                    let result = (|| -> anyhow::Result<()> {
                        let mut capture: Option<ve_audio::Capture> = None;
                        let mut ready = false;
                        let mut stream_epoch = 0;
                        loop {
                            let generation = manager_epoch.load(Ordering::Acquire);
                            if generation != stream_epoch {
                                // Observe even a rapid mute/unmute between manager ticks.
                                drop(capture.take());
                                stream_epoch = generation;
                            }
                            capture_ready_before_open(
                                &mut ready,
                                &manager_events,
                                &manager_health,
                                || {
                                    update_capture(
                                        &mut capture,
                                        manager_mute.load(Ordering::Acquire),
                                        || {
                                            let tx = capture_tx.clone();
                                            let epoch = manager_epoch.clone();
                                            let muted = manager_mute.clone();
                                            let continuity = manager_continuity.clone();
                                            Ok(ve_audio::Capture::start(
                                                &config,
                                                Box::new(move |event| {
                                                    if epoch.load(Ordering::Acquire) == generation
                                                        && !muted.load(Ordering::Acquire)
                                                    {
                                                        capture_event(
                                                            &tx,
                                                            generation,
                                                            &continuity,
                                                            event,
                                                        );
                                                    }
                                                }),
                                            )?)
                                        },
                                    )
                                },
                            )?;
                            if capture.as_ref().is_some_and(ve_audio::Capture::failed) {
                                anyhow::bail!("Input audio stream failed");
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    })();
                    if let Err(e) = result {
                        let _ =
                            manager_events.try_send(Input::Failure("capture_error", e.to_string()));
                    }
                });
                // Input inference remains independent of TTS so barge-in can cancel synthesis.
                let input_events = events.clone();
                let input_state = state.clone();
                let input_audio = audio.clone();
                let input_health = thread_health.clone();
                std::thread::spawn(move || {
                    let _health_guard = WorkerHealth(input_health.clone());
                    let result = (|| -> anyhow::Result<()> {
                        let mut capture_id = 0;
                        let mut open_hearing = None;
                        let mut last_epoch = 0;
                        let mut last_sequence = 0;
                        loop {
                            let epoch = input_epoch.load(Ordering::Acquire);
                            if epoch != last_epoch {
                                detector.reset();
                                last_epoch = epoch;
                                last_sequence = capture_continuity.load(Ordering::Acquire);
                                open_hearing = None;
                            }
                            if capture_gap(
                                &mut last_sequence,
                                capture_continuity.load(Ordering::Acquire),
                            ) {
                                detector.reset();
                                abandon_capture(
                                    &mut open_hearing,
                                    epoch,
                                    &input_events,
                                    &input_health,
                                )?;
                            }
                            let (frame_epoch, sequence, frame) =
                                match capture_rx.recv_timeout(Duration::from_millis(20)) {
                                    Ok(frame) => frame,
                                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                                    Err(_) => break,
                                };
                            let is_muted = mute.load(Ordering::Acquire);
                            let epoch = input_epoch.load(Ordering::Acquire);
                            if epoch != last_epoch {
                                detector.reset();
                                last_epoch = epoch;
                                last_sequence = 0;
                                open_hearing = None;
                            }
                            if is_muted || epoch != frame_epoch {
                                continue;
                            }
                            let continuity = capture_continuity.load(Ordering::Acquire);
                            if capture_gap(&mut last_sequence, continuity) {
                                // Never join noncontiguous captured audio. Abandon an
                                // incomplete hearing and start detection from a fresh frame.
                                detector.reset();
                                abandon_capture(
                                    &mut open_hearing,
                                    epoch,
                                    &input_events,
                                    &input_health,
                                )?;
                            }
                            if sequence != continuity {
                                continue;
                            }
                            let strict = input_audio.output_queued_frames() > 0
                                || input_state.lock().unwrap().generation.busy();
                            for event in detector.feed(&frame, strict)? {
                                match event {
                                    turn::TurnEvent::SpeechStart => {
                                        capture_id += 1;
                                        // Invalidate synthesis immediately, before the stdio controller wakes.
                                        let mut state = input_state.lock().unwrap();
                                        // Serialize with mute's epoch change/reset so a
                                        // discarded Hearing can never leave playback blocked.
                                        if mute.load(Ordering::Acquire)
                                            || input_epoch.load(Ordering::Acquire) != epoch
                                        {
                                            continue;
                                        }
                                        state.generation.invalidate(capture_id);
                                        state.audio.as_ref().unwrap().flush_output();
                                        drop(state);
                                        open_hearing = Some(capture_id);
                                        send(
                                            &input_events,
                                            Input::Hearing(epoch, capture_id),
                                            &input_health,
                                        )?;
                                    }
                                    turn::TurnEvent::Utterance { audio, .. } => {
                                        // Never block VAD/barge-in on Whisper or a full ASR
                                        // queue. Superseded/dropped utterances are abandoned.
                                        submit_utterance(
                                            &mut open_hearing,
                                            epoch,
                                            audio,
                                            &asr_tx,
                                            &input_events,
                                            &input_health,
                                        )?;
                                    }
                                }
                            }
                        }
                        Ok(())
                    })();
                    if let Err(e) = result {
                        let _ =
                            input_events.try_send(Input::Failure("capture_error", e.to_string()));
                    }
                });
                let mut last_speech = None;
                let mut joiner = ve_tts::Joiner::new(tts.sample_rate());
                loop {
                    let job = match rx.recv_timeout(Duration::from_millis(10)) {
                        Ok(job) => job,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            let epoch = worker_finish.swap(0, Ordering::AcqRel);
                            if epoch == 0 {
                                continue;
                            }
                            Job::Finish(epoch)
                        }
                        Err(_) => break,
                    };
                    match job {
                        Job::Speak {
                            epoch,
                            text,
                            cancel,
                        } => {
                            if cancel.load(Ordering::Acquire) {
                                continue;
                            }
                            if new_speech_token(&mut last_speech, epoch, &cancel) {
                                resampler.reset();
                                joiner.reset();
                            }
                            let samples = tts.synth(&text, cancel.clone())?;
                            if cancel.load(Ordering::Acquire) {
                                continue;
                            }
                            anyhow::ensure!(
                                samples.len() <= tts.sample_rate() as usize * 30,
                                "Synthesized chunk exceeded 30 seconds"
                            );
                            let samples = resampler.process(&joiner.push(&text, &samples));
                            anyhow::ensure!(
                                samples.len() <= 48_000 * 28,
                                "Joined audio chunk exceeds the bounded playback buffer"
                            );
                            // Bound ahead-of-device audio without dropping chunks. Cancellation
                            // remains immediate while waiting for the hardware queue to drain.
                            while audio.output_queued_frames() > 48_000 * 2
                                && !cancel.load(Ordering::Acquire)
                            {
                                std::thread::sleep(Duration::from_millis(10));
                            }
                            let state = state.lock().unwrap();
                            if state.generation.allows(epoch, &cancel) {
                                audio.push_output(&samples)?;
                            }
                        }
                        Job::Finish(epoch) => {
                            let finish_token = {
                                let state = state.lock().unwrap();
                                let token = state.generation.active_token(epoch);
                                if let Some(token) = &token {
                                    // A same-epoch replacement queued behind an old
                                    // Finish cannot own that old resampler tail.
                                    if !last_speech.as_ref().is_some_and(
                                        |(old_epoch, old_token)| {
                                            *old_epoch == epoch && Arc::ptr_eq(old_token, token)
                                        },
                                    ) {
                                        continue;
                                    }
                                    audio.push_output(&resampler.flush())?;
                                    audio.finish_output()?;
                                }
                                token
                            };
                            loop {
                                let state = state.lock().unwrap();
                                if !state.generation.can_drain(epoch)
                                    || finish_token
                                        .as_ref()
                                        .is_some_and(|token| token.load(Ordering::Acquire))
                                {
                                    break;
                                }
                                if audio.output_queued_frames() == 0 {
                                    finish_playback(state, &events, epoch, &thread_health)?;
                                    break;
                                }
                                drop(state);
                                std::thread::sleep(Duration::from_millis(10));
                            }
                        }
                    }
                }
                Ok(())
            })();
            if let Err(e) = result {
                let _ = events.try_send(Input::Failure("native_error", e.to_string()));
            }
        });
        Self {
            jobs,
            playback,
            muted,
            capture_epoch,
            health,
            pending_finish,
        }
    }

    pub fn action(&self, action: Action, epoch: u64) -> anyhow::Result<()> {
        match action {
            Action::Cancel => {
                self.pending_finish.store(0, Ordering::Release);
                let mut state = self.playback.lock().unwrap();
                state.generation.cancel(epoch);
                if let Some(audio) = &state.audio {
                    audio.flush_output();
                }
            }
            Action::UserActivity(id) => self
                .playback
                .lock()
                .unwrap()
                .generation
                .acknowledge_user_activity(id),
            Action::Mute(on) => {
                let mut state = self.playback.lock().unwrap();
                self.muted.store(on, Ordering::Release);
                self.capture_epoch.fetch_add(1, Ordering::AcqRel);
                // Controller mute cancels playback first: existing job tokens are
                // already canceled when the old capture fence is retired.
                state.generation = Default::default();
                state.generation.cancel(epoch);
            }
            Action::Speak(text) => {
                let mut state = self.playback.lock().unwrap();
                if let Some(cancel) = state.generation.token(epoch) {
                    if self
                        .jobs
                        .try_send(Job::Speak {
                            epoch,
                            text,
                            cancel,
                        })
                        .is_err()
                    {
                        // Speech saturation must never kill the conversation.
                        state.generation.finish();
                        if let Some(audio) = &state.audio {
                            audio.flush_output();
                        }
                    }
                }
            }
            Action::Finish => {
                if self.jobs.try_send(Job::Finish(epoch)).is_err() {
                    self.pending_finish.store(epoch, Ordering::Release);
                }
            }
        }
        Ok(())
    }
    pub fn healthy(&self) -> bool {
        !self.health.load(Ordering::Acquire)
            && self
                .playback
                .lock()
                .is_ok_and(|state| state.audio.as_ref().is_none_or(|a| !a.failed()))
    }
    pub fn can_drain(&self, epoch: u64) -> bool {
        self.playback
            .lock()
            .is_ok_and(|state| state.generation.can_drain(epoch))
    }
    pub fn capture_current(&self, epoch: u64) -> bool {
        !self.muted.load(Ordering::Acquire) && self.capture_epoch.load(Ordering::Acquire) == epoch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_ready_is_enqueued_before_a_synchronously_active_first_stream() {
        let (events, event_rx) = mpsc::sync_channel(4);
        let mut ready = false;
        capture_ready_before_open(&mut ready, &events, &AtomicBool::new(false), || {
            // Fake capture starts before the manager can execute another instruction.
            events.send(Input::Hearing(0, 1)).unwrap();
            events
                .send(Input::Transcript(0, 1, "first capture".into()))
                .unwrap();
            Ok(())
        })
        .unwrap();
        assert!(ready);
        let queued = [
            event_rx.recv().unwrap(),
            event_rx.recv().unwrap(),
            event_rx.recv().unwrap(),
        ];
        assert!(matches!(queued[0], Input::Ready));
        let mut controller = crate::controller::Controller::default();
        let mut out = vec![];
        let mut actions = vec![];
        for event in queued {
            match event {
                Input::Ready => controller.ready(&mut out, &mut actions),
                Input::Hearing(0, id) => controller.hearing(id, &mut out, &mut actions),
                Input::Transcript(0, id, text) => {
                    controller.transcript(id, text, &mut out, &mut actions)
                }
                _ => panic!("unexpected capture fixture event"),
            }
        }
        assert!(out.iter().any(|event| matches!(event, crate::protocol::Engine::Prompt { text, .. } if text == "first capture")));
        capture_ready_before_open(&mut ready, &events, &AtomicBool::new(false), || Ok(())).unwrap();
        assert!(
            event_rx.try_recv().is_err(),
            "readiness must only be published once"
        );
    }
    #[test]
    fn fresh_token_in_same_epoch_cannot_carry_canceled_dsp_tail() {
        let mut previous = None;
        let old = Arc::new(AtomicBool::new(false));
        let mut resampler = ve_audio::StreamResampler::new(24_000, 48_000).unwrap();
        let mut joiner = ve_tts::Joiner::new(24_000);
        assert!(new_speech_token(&mut previous, 1, &old));
        let _ = resampler.process(&joiner.push("Canceled sentence.", &[0.3; 77]));
        old.store(true, Ordering::Release);
        let fresh = Arc::new(AtomicBool::new(false));
        if new_speech_token(&mut previous, 1, &fresh) {
            resampler.reset();
            joiner.reset();
        }
        let mut actual = resampler.process(&joiner.push("Fresh speech", &[0.7; 41]));
        actual.extend(resampler.flush());
        let mut clean_resampler = ve_audio::StreamResampler::new(24_000, 48_000).unwrap();
        let mut expected =
            clean_resampler.process(&ve_tts::Joiner::new(24_000).push("Fresh speech", &[0.7; 41]));
        expected.extend(clean_resampler.flush());
        assert_eq!(
            actual, expected,
            "same-epoch token replacement leaked canceled DSP state"
        );
        assert!(!new_speech_token(&mut previous, 1, &fresh));
    }
    #[test]
    fn capture_gap_after_asr_handoff_preserves_the_valid_prompt() {
        let (jobs, rx) = mpsc::sync_channel(1);
        let (events, event_rx) = mpsc::sync_channel(4);
        let (started, start_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let worker_events = events.clone();
        let worker = std::thread::spawn(move || {
            recognize(
                rx,
                worker_events,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicU64::new(0)),
                |_| {
                    started.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok("valid complete utterance".into())
                },
            )
        });
        let health = AtomicBool::new(false);
        let mut open = Some(1);
        submit_utterance(&mut open, 0, vec![0.0], &jobs, &events, &health).unwrap();
        start_rx.recv().unwrap();
        let mut sequence = 1;
        assert!(capture_gap(&mut sequence, 3));
        abandon_capture(&mut open, 0, &events, &health).unwrap();
        let abandoned = event_rx.try_recv().ok();
        // Release/join even when the mutation emits an invalid abandonment.
        release.send(()).unwrap();
        drop(jobs);
        worker.join().unwrap().unwrap();
        assert!(
            abandoned.is_none(),
            "gap abandoned an already submitted utterance"
        );
        let mut controller = crate::controller::Controller::default();
        let mut out = vec![];
        let mut actions = vec![];
        controller.ready(&mut out, &mut actions);
        controller.hearing(1, &mut out, &mut actions);
        match event_rx.recv().unwrap() {
            Input::Transcript(0, id, text) => {
                controller.transcript(id, text, &mut out, &mut actions)
            }
            _ => panic!("expected completed ASR transcript"),
        }
        assert!(out.iter().any(|event| matches!(event, crate::protocol::Engine::Prompt { text, .. } if text == "valid complete utterance")));
    }
    #[test]
    fn capture_gap_abandons_only_incomplete_hearing_once() {
        let (events, rx) = mpsc::sync_channel(4);
        let health = AtomicBool::new(false);
        let mut open = Some(7);
        abandon_capture(&mut open, 0, &events, &health).unwrap();
        abandon_capture(&mut open, 0, &events, &health).unwrap();
        assert!(matches!(rx.try_recv(), Ok(Input::Transcript(0, 7, text)) if text.is_empty()));
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn controller_mute_cancels_live_tokens_before_retiring_capture() {
        let (jobs, rx) = mpsc::sync_channel(1);
        let backend = Backend {
            jobs,
            playback: Arc::new(Mutex::new(Playback::default())),
            muted: Arc::new(AtomicBool::new(false)),
            capture_epoch: Arc::new(AtomicU64::new(0)),
            health: Arc::new(AtomicBool::new(false)),
            pending_finish: Arc::new(AtomicU64::new(0)),
        };
        let mut epoch = 1;
        backend.action(Action::Cancel, epoch).unwrap();
        backend
            .action(Action::Speak("in-flight speech".into()), epoch)
            .unwrap();
        let Job::Speak { cancel, .. } = rx.recv().unwrap() else {
            panic!("expected live synthesis job");
        };
        assert!(!cancel.load(Ordering::Acquire));
        let mut controller = crate::controller::Controller::default();
        let mut actions = vec![];
        controller
            .host(
                crate::protocol::Host::Mute { on: true },
                &mut vec![],
                &mut actions,
            )
            .unwrap();
        assert_eq!(actions, [Action::Cancel, Action::Mute(true)]);
        for action in actions {
            if action == Action::Cancel {
                epoch += 1;
            }
            backend.action(action, epoch).unwrap();
        }
        assert!(cancel.load(Ordering::Acquire));
        let mut actions = vec![];
        controller
            .host(
                crate::protocol::Host::Mute { on: false },
                &mut vec![],
                &mut actions,
            )
            .unwrap();
        assert_eq!(actions, [Action::Mute(false)]);
        for action in actions {
            backend.action(action, epoch).unwrap();
        }
        assert!(cancel.load(Ordering::Acquire));
        assert!(backend.can_drain(epoch));
        assert!(!backend.playback.lock().unwrap().generation.busy());
    }
    #[test]
    fn dropped_capture_frames_are_detected_before_assembling_more_audio() {
        let mut last = 0;
        assert!(!capture_gap(&mut last, 0));
        assert!(!capture_gap(&mut last, 0));
        assert!(capture_gap(&mut last, 1));
        assert!(!capture_gap(&mut last, 1));
        assert!(capture_gap(&mut last, 3));
    }
    #[test]
    fn capture_ring_gap_and_full_runtime_channel_cannot_hide_discontinuity() {
        let (tx, rx) = mpsc::sync_channel(1);
        let continuity = AtomicU64::new(0);
        capture_event(&tx, 0, &continuity, ve_audio::CaptureEvent::Frame(&[0.2]));
        capture_event(&tx, 0, &continuity, ve_audio::CaptureEvent::Gap);
        assert_eq!(continuity.load(Ordering::Acquire), 1);
        capture_event(&tx, 0, &continuity, ve_audio::CaptureEvent::Frame(&[0.4]));
        assert_eq!(continuity.load(Ordering::Acquire), 2);
        let (_, stale, _) = rx.recv().unwrap();
        assert_ne!(stale, continuity.load(Ordering::Acquire));
        let mut last = 0;
        assert!(capture_gap(&mut last, continuity.load(Ordering::Acquire)));
        let (events, event_rx) = mpsc::sync_channel(1);
        let mut open = Some(7);
        abandon_capture(&mut open, 0, &events, &AtomicBool::new(false)).unwrap();
        assert!(
            matches!(event_rx.recv().unwrap(), Input::Transcript(0, 7, text) if text.is_empty())
        );
        capture_event(&tx, 0, &continuity, ve_audio::CaptureEvent::Frame(&[0.6]));
        assert_eq!(rx.recv().unwrap().1, continuity.load(Ordering::Acquire));
    }
    #[test]
    fn saturated_drain_delivery_releases_playback_for_cancel_and_health() {
        let playback = Arc::new(Mutex::new(Playback::default()));
        playback.lock().unwrap().generation.cancel(1);
        let state = playback.clone();
        let (events, rx) = mpsc::sync_channel(1);
        events.send(Input::Ready).unwrap();
        let (started, start_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let guard = state.lock().unwrap();
            started.send(()).unwrap();
            finish_playback(guard, &events, 1, &AtomicBool::new(false)).unwrap();
        });
        start_rx.recv().unwrap();
        // The event queue stays full while controller operations acquire the lock.
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            if let Ok(mut guard) = playback.try_lock() {
                guard.generation.cancel(2);
                assert!(!guard.generation.can_drain(1));
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "drain delivery held playback mutex"
            );
            std::thread::yield_now();
        }
        assert!(matches!(rx.recv().unwrap(), Input::Ready));
        assert!(matches!(rx.recv().unwrap(), Input::Drained(1)));
        worker.join().unwrap();
    }
    #[test]
    fn mute_retires_pending_capture_fence_and_queued_events() {
        let (jobs, _rx) = mpsc::sync_channel(1);
        let backend = Backend {
            jobs,
            playback: Arc::new(Mutex::new(Playback::default())),
            muted: Arc::new(AtomicBool::new(false)),
            capture_epoch: Arc::new(AtomicU64::new(0)),
            health: Arc::new(AtomicBool::new(false)),
            pending_finish: Arc::new(AtomicU64::new(0)),
        };
        backend.playback.lock().unwrap().generation.invalidate(7);
        backend.action(Action::Cancel, 1).unwrap();
        backend.action(Action::Mute(true), 1).unwrap();
        backend.action(Action::Mute(false), 1).unwrap();
        assert!(!backend.capture_current(0));
        assert!(!backend.capture_current(1));
        assert!(backend.capture_current(2));
        assert!(backend
            .playback
            .lock()
            .unwrap()
            .generation
            .token(1)
            .is_some());
    }
    #[test]
    fn blocked_asr_is_bounded_and_mute_discards_its_result() {
        let (jobs, rx) = mpsc::sync_channel(1);
        let (events, event_rx) = mpsc::sync_channel(4);
        let (started, start_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let epoch = Arc::new(AtomicU64::new(0));
        let worker_epoch = epoch.clone();
        let worker = std::thread::spawn(move || {
            recognize(
                rx,
                events,
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
                worker_epoch,
                |_| {
                    started.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok("retired transcript".into())
                },
            )
        });
        jobs.try_send(AsrJob {
            epoch: 0,
            id: 1,
            audio: vec![0.0],
        })
        .unwrap();
        start_rx.recv().unwrap();
        jobs.try_send(AsrJob {
            epoch: 0,
            id: 2,
            audio: vec![0.0],
        })
        .unwrap();
        assert!(jobs
            .try_send(AsrJob {
                epoch: 0,
                id: 3,
                audio: vec![0.0]
            })
            .is_err());
        // Capture/detection stays free to advance while inference is blocked.
        epoch.store(1, Ordering::Release);
        release.send(()).unwrap();
        drop(jobs);
        worker.join().unwrap().unwrap();
        assert!(event_rx.try_recv().is_err());
    }
    #[test]
    fn full_event_queue_cannot_hide_worker_failure() {
        let (tx, _rx) = mpsc::sync_channel(1);
        tx.try_send(Input::Ready).unwrap();
        let health = Arc::new(AtomicBool::new(false));
        let worker_health = health.clone();
        let _ = std::thread::spawn(move || {
            let _guard = WorkerHealth(worker_health);
            assert!(tx
                .try_send(Input::Failure("native_error", "synthetic failure".into()))
                .is_err());
            panic!("synthetic model worker panic");
        })
        .join();
        assert!(health.load(Ordering::Acquire));
    }
    #[test]
    fn mute_drops_capture_and_unmute_reopens_without_replacing_output() {
        use std::sync::atomic::AtomicUsize;
        struct Handle(Arc<AtomicUsize>);
        impl Drop for Handle {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let output = Arc::new(());
        let original_output = output.clone();
        let mut capture = None;
        update_capture(&mut capture, false, || Ok(Handle(drops.clone()))).unwrap();
        update_capture(&mut capture, true, || unreachable!()).unwrap();
        assert!(capture.is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        update_capture(&mut capture, false, || Ok(Handle(drops.clone()))).unwrap();
        update_capture(&mut capture, false, || unreachable!()).unwrap();
        assert!(capture.is_some());
        assert!(Arc::ptr_eq(&output, &original_output));
        drop(capture);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }
}
