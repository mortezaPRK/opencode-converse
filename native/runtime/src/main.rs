#[cfg(feature = "native-audio")]
mod audio;
mod controller;
mod generation;
mod models;
mod paths;
mod protocol;
mod speech;

use controller::{Action, Controller};
use protocol::{Engine, Host, Status, MAX_FRAME_BYTES};
use std::io::{BufRead, Write};
use std::sync::{
    atomic::{AtomicBool, AtomicU8, Ordering},
    mpsc::{self, SyncSender},
    Arc,
};
use std::time::Duration;
const MAX_SPEECH_JOBS: usize = 2048;
const MAX_EVENTS: usize = 64;

pub enum Input {
    Host(Host),
    Ready,
    Hearing(u64, u64),
    Transcript(u64, u64, String),
    Drained(u64),
    Failure(&'static str, String),
}

/// A bounded reader which never allocates an unbounded line or echoes invalid input.
fn frame(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, &'static str> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(|_| "Cannot read stdin")?;
        if available.is_empty() {
            if frame.is_empty() {
                return Ok(None);
            }
            return Err("Truncated JSON line at EOF");
        }
        let newline = available.iter().position(|b| *b == b'\n');
        let n = newline.map(|i| i + 1).unwrap_or(available.len());
        if frame.len() + n > MAX_FRAME_BYTES {
            return Err("Frame exceeds MAX_FRAME_BYTES");
        }
        frame.extend_from_slice(&available[..n]);
        reader.consume(n);
        if newline.is_some() {
            return Ok(Some(frame));
        }
    }
}

fn emit(messages: &[Engine]) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    for message in messages {
        write_message(&mut stdout, message)?;
    }
    stdout.flush()?;
    Ok(())
}

fn write_message(output: &mut impl Write, message: &Engine) -> anyhow::Result<()> {
    struct Frame(Vec<u8>);
    impl Write for Frame {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > MAX_FRAME_BYTES - 1 - self.0.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "Serialized output exceeds MAX_FRAME_BYTES",
                ));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut frame = Frame(Vec::new());
    serde_json::to_writer(&mut frame, message)?;
    frame.0.push(b'\n');
    output.write_all(&frame.0)?;
    Ok(())
}

fn retain_process_lock(file: std::fs::File) -> std::mem::ManuallyDrop<std::fs::File> {
    // Native workers are deliberately not joined on return. Only OS process
    // teardown may release this lock, after all their device handles disappear.
    std::mem::ManuallyDrop::new(file)
}

fn queued_failure(rx: &mpsc::Receiver<Input>) -> Option<(&'static str, String)> {
    for _ in 0..MAX_EVENTS {
        match rx.try_recv() {
            Ok(Input::Failure(code, message)) => return Some((code, message)),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    None
}

fn error(code: &'static str, message: impl Into<String>) {
    let _ = emit(&[Engine::Error {
        code,
        message: message.into(),
        fatal: true,
    }]);
}

fn reader(tx: SyncSender<Input>, stop: Arc<AtomicU8>) {
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    loop {
        let result = frame(&mut stdin);
        match result {
            Ok(None) => {
                stop.store(1, Ordering::Release);
                return;
            }
            Ok(Some(bytes)) => {
                let message = serde_json::from_slice::<Host>(&bytes);
                match message {
                    Ok(message) => {
                        if let Err(message) = message.validate() {
                            let _ = tx.try_send(Input::Failure("invalid_protocol", message.into()));
                            stop.store(2, Ordering::Release);
                            return;
                        }
                        let shutdown = matches!(message, Host::Shutdown {});
                        // Bounded backpressure for valid host bursts, not a fatal
                        // conversation error. This reader is never the stdio controller.
                        if tx.send(Input::Host(message)).is_err() {
                            stop.store(3, Ordering::Release);
                            return;
                        }
                        if shutdown {
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = tx.try_send(Input::Failure(
                            "invalid_protocol",
                            "Malformed or unknown protocol message".into(),
                        ));
                        stop.store(2, Ordering::Release);
                        return;
                    }
                }
            }
            Err(message) => {
                let _ = tx.try_send(Input::Failure("invalid_frame", message.into()));
                stop.store(2, Ordering::Release);
                return;
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Native,
    Fixture,
    LoadingFixture,
}

fn fixture_allowed(debug_assertions: bool) -> bool {
    debug_assertions
}

struct Backend {
    epoch: u64,
    fixture: FixtureSink,
    #[cfg(feature = "native-audio")]
    audio: Option<audio::Backend>,
}
#[derive(Default)]
struct FixtureSink {
    fence: generation::Generation,
    jobs: std::collections::VecDeque<(u64, Arc<AtomicBool>, String)>,
    finish: Option<u64>,
    trace: bool,
    #[cfg(test)]
    played: Vec<String>,
}
impl FixtureSink {
    fn action(&mut self, action: Action, epoch: u64) -> anyhow::Result<()> {
        match action {
            Action::Cancel => {
                self.fence.cancel(epoch);
                self.jobs.clear();
                self.finish = None;
            }
            Action::UserActivity(id) => self.fence.acknowledge_user_activity(id),
            Action::Speak(text) => {
                if self.jobs.len() < MAX_SPEECH_JOBS {
                    if let Some(token) = self.fence.token(epoch) {
                        self.jobs.push_back((epoch, token, text));
                    }
                }
            }
            Action::Finish => self.finish = Some(epoch),
            Action::Mute(_) => {}
        }
        Ok(())
    }
    fn tick(&mut self) -> Option<u64> {
        self.tick_with(|text| {
            let _ = writeln!(std::io::stderr().lock(), "{}", fixture_trace(text));
        })
    }
    fn tick_with(&mut self, mut trace: impl FnMut(&str)) -> Option<u64> {
        if let Some((epoch, token, text)) = self.jobs.pop_front() {
            if self.fence.allows(epoch, &token) {
                if self.trace {
                    trace(&text);
                }
                #[cfg(test)]
                self.played.push(text);
            }
        }
        if self.jobs.is_empty() {
            if let Some(epoch) = self.finish.take() {
                if self.fence.can_drain(epoch) {
                    self.fence.finish();
                    return Some(epoch);
                }
            }
        }
        None
    }
}

fn fixture_trace(text: &str) -> String {
    serde_json::json!({ "fixture": "played", "text": text }).to_string()
}
impl Backend {
    fn new() -> Self {
        Self {
            epoch: 0,
            fixture: Default::default(),
            #[cfg(feature = "native-audio")]
            audio: None,
        }
    }
    fn action(&mut self, action: Action, tx: &SyncSender<Input>) -> anyhow::Result<()> {
        if action == Action::Cancel {
            self.epoch += 1;
        }
        #[cfg(feature = "native-audio")]
        if let Some(audio) = &self.audio {
            return audio.action(action, self.epoch);
        }
        let _ = tx;
        self.fixture.action(action, self.epoch)
    }
    fn tick(&mut self, tx: &SyncSender<Input>) {
        #[cfg(feature = "native-audio")]
        if self.audio.is_some() {
            return;
        }
        if let Some(epoch) = self.fixture.tick() {
            if matches!(
                tx.try_send(Input::Drained(epoch)),
                Err(mpsc::TrySendError::Full(_))
            ) {
                self.fixture.finish = Some(epoch);
            }
        }
    }
    fn healthy(&self) -> bool {
        #[cfg(feature = "native-audio")]
        if let Some(audio) = &self.audio {
            return audio.healthy();
        }
        true
    }
    fn can_drain(&self, epoch: u64) -> bool {
        #[cfg(feature = "native-audio")]
        if let Some(audio) = &self.audio {
            return audio.can_drain(epoch);
        }
        self.fixture.fence.can_drain(epoch)
    }
    fn capture_current(&self, epoch: u64) -> bool {
        #[cfg(feature = "native-audio")]
        if let Some(audio) = &self.audio {
            return audio.capture_current(epoch);
        }
        epoch == 0
    }
}

fn run(mode: Mode) -> i32 {
    if emit(&[Engine::Hello {
        protocol: protocol::VERSION,
    }])
    .is_err()
    {
        return 1;
    }
    if mode != Mode::Native && !fixture_allowed(cfg!(debug_assertions)) {
        error("fixture_unavailable", "Test-only fixture modes require compiled debug assertions; local storage was not accessed.");
        return 1;
    }
    let _lock = match paths::lock() {
        Ok(lock) => retain_process_lock(lock),
        Err(e) => {
            error("engine_locked", e.to_string());
            return 1;
        }
    };
    let (tx, rx) = mpsc::sync_channel::<Input>(MAX_EVENTS);
    let stop = Arc::new(AtomicU8::new(0));
    let reader_stop = stop.clone();
    let reader_tx = tx.clone();
    std::thread::spawn(move || reader(reader_tx, reader_stop));
    let mut initialized = false;
    let mut controller = Controller::default();
    let mut backend = Backend::new();
    backend.fixture.trace = mode == Mode::Fixture;
    loop {
        // EOF/overflow are out-of-band: they cannot sit behind model loading or a full queue.
        let stopped = stop.load(Ordering::Acquire);
        if stopped == 1 {
            let _ = backend.action(Action::Cancel, &tx);
            return 0;
        }
        if stopped == 3 {
            error(
                "queue_overflow",
                "Host input queue exceeded its fixed capacity",
            );
            return 1;
        }
        if !backend.healthy() {
            if let Some((code, message)) = queued_failure(&rx) {
                error(code, message);
                return 1;
            }
            error("native_health", "A native audio/model worker stopped or its bounded event queue overflowed. Run opencode-converse doctor before restarting.");
            return 1;
        }
        backend.tick(&tx);
        let input = match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(input) => input,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if stopped == 2 {
                    error("invalid_protocol", "Invalid input frame");
                    return 1;
                }
                continue;
            }
            Err(_) => return 0,
        };
        let mut out = vec![];
        let mut actions = vec![];
        match input {
            Input::Host(Host::Init { settings, .. }) if !initialized => {
                initialized = true;
                out.push(Engine::Status {
                    state: Status::Loading,
                });
                if mode == Mode::Fixture {
                    controller.ready(&mut out, &mut actions);
                    controller.hearing(1, &mut out, &mut actions);
                    controller.transcript(1, "Fixture prompt.".into(), &mut out, &mut actions);
                } else if mode == Mode::Native {
                    #[cfg(feature = "native-audio")]
                    {
                        backend.audio = Some(audio::Backend::start(settings, tx.clone()));
                    }
                    #[cfg(not(feature = "native-audio"))]
                    {
                        let _ = settings;
                        error("native_unavailable", "This executable was built without native-audio. Build with --features native-audio; test fixtures require an explicit --test-fixture switch.");
                        return 1;
                    }
                }
            }
            Input::Host(Host::Shutdown {}) => {
                let _ = backend.action(Action::Cancel, &tx);
                return 0;
            }
            Input::Host(message) => {
                if !initialized {
                    error("init_required", "init must be the first message");
                    return 1;
                }
                if let Err(e) = controller.host(message, &mut out, &mut actions) {
                    error("invalid_state", e);
                    return 1;
                }
            }
            Input::Ready => controller.ready(&mut out, &mut actions),
            Input::Hearing(epoch, id) => {
                if backend.capture_current(epoch) {
                    controller.hearing(id, &mut out, &mut actions);
                }
            }
            Input::Transcript(epoch, id, text) => {
                if backend.capture_current(epoch) {
                    controller.transcript(id, text, &mut out, &mut actions);
                }
            }
            Input::Drained(epoch) => {
                if epoch == backend.epoch && backend.can_drain(epoch) {
                    controller.drained(&mut out, &mut actions);
                }
            }
            Input::Failure(code, message) => {
                let _ = backend.action(Action::Cancel, &tx);
                error(code, message);
                return 1;
            }
        }
        for action in actions {
            if let Err(e) = backend.action(action, &tx) {
                error("backend_error", e.to_string());
                return 1;
            }
        }
        if emit(&out).is_err() {
            return 1;
        }
    }
}

fn doctor() -> i32 {
    // Doctor is human-readable, never opens a device, loads a model, or needs a lock.
    eprintln!(
        "OpenCode Converse {} — protocol {}",
        env!("CARGO_PKG_VERSION"),
        protocol::VERSION
    );
    eprintln!("Native audio compiled: {}", cfg!(feature = "native-audio"));
    eprintln!("Device capture/playback: not tested (doctor does not open devices)");
    let models_ok = match paths::check_models() {
        Ok(path) => {
            eprintln!(
                "Required model files present at {} (integrity/inference not verified)",
                path.display()
            );
            true
        }
        Err(e) => {
            eprintln!("{e}");
            false
        }
    };
    if !cfg!(feature = "native-audio") {
        eprintln!("Build the real runtime using cargo build --release -p opencode-converse --features native-audio");
    }
    if models_ok && cfg!(feature = "native-audio") {
        0
    } else {
        1
    }
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let code = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["run"] => run(Mode::Native),
        ["run", "--test-fixture"] => run(Mode::Fixture),
        ["run", "--test-fixture-loading"] => run(Mode::LoadingFixture),
        ["doctor"] => doctor(),
        ["doctor", "--test-only-inference"] => {
            if !fixture_allowed(cfg!(debug_assertions)) {
                eprintln!("Test-only inference requires compiled debug assertions; local storage was not accessed.");
                std::process::exit(1);
            }
            #[cfg(feature = "native-audio")]
            {
                match audio::test_only_inference() {
                    Ok(()) => 0,
                    Err(e) => {
                        eprintln!("Test-only inference failed: {e:#}");
                        1
                    }
                }
            }
            #[cfg(not(feature = "native-audio"))]
            {
                eprintln!("Test-only inference requires --features native-audio");
                1
            }
        }
        ["models", "install"] => match models::install() {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("Model installation failed: {e:#}");
                1
            }
        },
        ["--version"] => {
            eprintln!("opencode-converse {}", env!("CARGO_PKG_VERSION"));
            0
        }
        _ => {
            eprintln!("Usage: opencode-converse run | doctor | models install\nTest-only: run --test-fixture | run --test-fixture-loading | doctor --test-only-inference");
            2
        }
    };
    // Do not join uncancellable third-party model constructors on EOF/shutdown.
    // Process teardown closes devices, terminates workers, and releases the OS lock.
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_only_storage_requires_effective_compiled_debug_assertions() {
        assert!(fixture_allowed(true));
        assert!(!fixture_allowed(false));
    }
    #[test]
    fn oversized_serialized_output_is_rejected_before_any_bytes_are_written() {
        let message = Engine::Prompt {
            turn: 1,
            text: "\u{1}".repeat(protocol::MAX_TEXT_BYTES),
        };
        let mut output = vec![];
        assert!(write_message(&mut output, &message).is_err());
        assert!(output.is_empty());
        let message = Engine::Prompt {
            turn: 1,
            text: "quotes \\\" ".repeat(10_000),
        };
        write_message(&mut output, &message).unwrap();
        assert!(output.len() <= MAX_FRAME_BYTES && output.ends_with(b"\n"));
    }
    #[test]
    fn failed_worker_detail_is_found_behind_a_bounded_event_burst() {
        let (tx, rx) = mpsc::sync_channel(MAX_EVENTS);
        for _ in 1..MAX_EVENTS {
            tx.send(Input::Ready).unwrap();
        }
        tx.send(Input::Failure(
            "native_error",
            "Models missing; run models install".into(),
        ))
        .unwrap();
        assert_eq!(
            queued_failure(&rx),
            Some(("native_error", "Models missing; run models install".into()))
        );
        assert!(queued_failure(&rx).is_none());
    }
    #[cfg(unix)]
    #[test]
    fn engine_lock_survives_the_run_scope_until_os_process_teardown() {
        use std::os::fd::{AsRawFd, FromRawFd};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("engine.lock");
        let file = std::fs::File::create(&path).unwrap();
        fs2::FileExt::try_lock_exclusive(&file).unwrap();
        let fd = file.as_raw_fd();
        {
            let _guard = retain_process_lock(file);
        }
        let second = std::fs::File::open(&path).unwrap();
        let retained = fs2::FileExt::try_lock_exclusive(&second).is_err();
        if retained {
            // SAFETY: the process-scoped helper retains this owned descriptor;
            // only this test reclaims it, rather than leaking it until test exit.
            drop(unsafe { std::fs::File::from_raw_fd(fd) });
        }
        assert!(
            retained,
            "returning from run released the audio engine lock"
        );
    }
    #[test]
    fn fixture_trace_records_only_allowed_consumed_jobs_once() {
        for enabled in [false, true] {
            let mut fixture = FixtureSink {
                trace: enabled,
                ..Default::default()
            };
            let mut traces = vec![];
            fixture.action(Action::Cancel, 1).unwrap();
            fixture
                .action(Action::Speak("Canceled chunk.".into()), 1)
                .unwrap();
            fixture.action(Action::Cancel, 2).unwrap();
            fixture
                .action(Action::Speak("Allowed \"chunk\".".into()), 2)
                .unwrap();
            fixture.action(Action::Finish, 2).unwrap();
            assert_eq!(
                fixture.tick_with(|text| traces.push(fixture_trace(text))),
                Some(2)
            );
            assert_eq!(
                fixture.tick_with(|text| traces.push(fixture_trace(text))),
                None
            );
            assert_eq!(fixture.played, ["Allowed \"chunk\"."]);
            if enabled {
                assert_eq!(traces.len(), 1);
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&traces[0]).unwrap(),
                    serde_json::json!({"fixture": "played", "text": "Allowed \"chunk\"."})
                );
            } else {
                assert!(traces.is_empty());
            }
            fixture
                .action(Action::Speak("Capture-fenced chunk.".into()), 2)
                .unwrap();
            fixture.fence.invalidate(1);
            assert!(fixture
                .tick_with(|text| traces.push(fixture_trace(text)))
                .is_none());
            assert_eq!(traces.len(), usize::from(enabled));
        }
    }
    #[test]
    fn fixture_retries_completion_after_full_event_queue() {
        let (tx, rx) = mpsc::sync_channel(1);
        let mut backend = Backend::new();
        backend.action(Action::Cancel, &tx).unwrap();
        backend.action(Action::Finish, &tx).unwrap();
        tx.try_send(Input::Ready).unwrap();
        backend.tick(&tx);
        assert!(matches!(rx.recv().unwrap(), Input::Ready));
        backend.tick(&tx);
        assert!(matches!(rx.try_recv(), Ok(Input::Drained(1))));
    }
    #[test]
    fn bounded_framing() {
        assert!(frame(&mut &b"{}"[..]).is_err());
        assert_eq!(frame(&mut &b"{}\n"[..]).unwrap().unwrap(), b"{}\n");
        assert!(frame(&mut vec![b' '; MAX_FRAME_BYTES + 1].as_slice()).is_err());
    }

    // A real queued recording fixture, not an assertion on protocol status alone.
    // It uses the same generation fence as the native pre-playback commit.
    struct SpeechFixture {
        controller: Controller,
        backend: Backend,
        tx: SyncSender<Input>,
        out: Vec<Engine>,
    }
    impl SpeechFixture {
        fn new() -> Self {
            let (tx, _) = mpsc::sync_channel(64);
            let mut s = Self {
                controller: Controller::default(),
                backend: Backend::new(),
                tx,
                out: vec![],
            };
            s.controller.ready(&mut s.out, &mut vec![]);
            s
        }
        fn apply(&mut self, actions: Vec<Action>) {
            for action in actions {
                self.backend.action(action, &self.tx).unwrap();
            }
        }
        fn host(&mut self, message: Host) {
            let mut actions = vec![];
            self.controller
                .host(message, &mut self.out, &mut actions)
                .unwrap();
            self.apply(actions);
        }
        fn user(&mut self, id: u64) {
            let mut actions = vec![];
            self.controller.hearing(id, &mut self.out, &mut actions);
            self.apply(actions);
        }
        fn tick(&mut self) {
            if let Some(epoch) = self.backend.fixture.tick() {
                if epoch == self.backend.epoch && self.backend.can_drain(epoch) {
                    let mut actions = vec![];
                    self.controller.drained(&mut self.out, &mut actions);
                    self.apply(actions);
                }
            }
        }
        fn announcement(&mut self, id: &str, text: &str) {
            self.host(Host::Announcement {
                reply: id.into(),
                text: text.into(),
            });
        }
    }
    #[test]
    fn announcement_fixture_schedules_sanitized_audio_only_after_idle() {
        let mut f = SpeechFixture::new();
        f.user(1);
        f.controller
            .transcript(1, "user request".into(), &mut vec![], &mut vec![]);
        f.announcement("a", "Worker **finished** safely.\n```\nsecret();\n```\n");
        f.tick();
        assert!(f.backend.fixture.played.is_empty());
        f.host(Host::TurnEnd {
            turn: 1,
            reason: protocol::Reason::Completed,
        });
        f.tick();
        assert!(f.backend.fixture.played.is_empty()); // drain schedules; does not yet play
        f.tick();
        assert_eq!(f.backend.fixture.played, ["Worker finished safely."]);
        f.announcement("a", "Do not repeat.");
        f.tick();
        assert_eq!(f.backend.fixture.played.len(), 1);
    }
    #[test]
    fn clipped_replacement_preserves_one_played_notice_and_the_turn_budget() {
        for replacements in [1, 3] {
            let mut f = SpeechFixture::new();
            f.user(1);
            f.controller
                .transcript(1, "user request".into(), &mut vec![], &mut vec![]);
            f.host(Host::ReplyDelta {
                turn: 1,
                reply: "large".into(),
                text: "A b c. ".repeat(70),
            });
            // No playback tick: cancellation must replace the old queued notice.
            for i in 0..replacements {
                f.host(Host::ReplyDelta {
                    turn: 1,
                    reply: format!("replacement-{i}"),
                    text: "New answer stays on screen. ".into(),
                });
            }
            let reply = format!("replacement-{}", replacements - 1);
            f.host(Host::ReplyDelta {
                turn: 1,
                reply: reply.clone(),
                text: "More answer stays on screen. ".into(),
            });
            f.host(Host::ReplyEnd {
                turn: 1,
                reply,
                text: "New answer stays on screen. More answer stays on screen. ".into(),
            });
            f.host(Host::TurnEnd {
                turn: 1,
                reason: protocol::Reason::Completed,
            });
            for _ in 0..5 {
                f.tick();
            }
            assert_eq!(f.backend.fixture.played, ["The rest is on screen."]);
            assert!(matches!(
                f.out.last(),
                Some(Engine::Status {
                    state: Status::Listening
                })
            ));
        }
    }
    #[test]
    fn fresh_clipping_notice_is_still_canceled_by_mute_or_hearing() {
        for mute in [false, true] {
            let mut f = SpeechFixture::new();
            f.user(1);
            f.controller
                .transcript(1, "user request".into(), &mut vec![], &mut vec![]);
            f.host(Host::ReplyDelta {
                turn: 1,
                reply: "large".into(),
                text: "A b c. ".repeat(70),
            });
            f.host(Host::ReplyDelta {
                turn: 1,
                reply: "replacement".into(),
                text: "New answer stays on screen. ".into(),
            });
            assert_eq!(
                f.backend.fixture.jobs.len(),
                1,
                "replacement must queue its clipping cue before interruption"
            );
            if mute {
                f.host(Host::Mute { on: true });
            } else {
                f.user(2);
            }
            for _ in 0..5 {
                f.tick();
            }
            assert!(f.backend.fixture.played.is_empty());
        }
    }
    #[test]
    fn abandoned_capture_and_duplicate_terminal_preserve_actual_announcement_playback() {
        let mut f = SpeechFixture::new();
        f.user(1);
        f.announcement("queued", "A queued worker update.");
        let mut actions = vec![];
        f.controller
            .transcript(1, String::new(), &mut vec![], &mut actions);
        f.apply(actions);
        f.tick();
        assert_eq!(f.backend.fixture.played, ["A queued worker update."]);

        f.user(2);
        f.controller
            .transcript(2, "user request".into(), &mut vec![], &mut vec![]);
        f.host(Host::TurnEnd {
            turn: 1,
            reason: protocol::Reason::Failed,
        });
        f.announcement("fresh", "An unrelated fresh update.");
        f.host(Host::TurnEnd {
            turn: 1,
            reason: protocol::Reason::Failed,
        });
        f.tick();
        assert_eq!(
            f.backend.fixture.played.last().unwrap(),
            "An unrelated fresh update."
        );
    }
    #[test]
    fn announcement_fixture_barge_in_discards_inflight_and_pending_without_resume() {
        let mut f = SpeechFixture::new();
        f.announcement("a", "First chunk is audible. Second chunk must stop.");
        f.announcement("b", "Pending announcement must stop.");
        f.tick();
        assert_eq!(f.backend.fixture.played, ["First chunk is audible."]);
        f.user(1);
        for _ in 0..5 {
            f.tick();
        }
        assert_eq!(f.backend.fixture.played, ["First chunk is audible."]);
        f.controller
            .transcript(1, "new user request".into(), &mut vec![], &mut vec![]);
        f.host(Host::TurnEnd {
            turn: 1,
            reason: protocol::Reason::Completed,
        });
        for _ in 0..5 {
            f.tick();
        }
        assert_eq!(f.backend.fixture.played.len(), 1);
        f.announcement("fresh", "Fresh announcement may play.");
        f.tick();
        assert_eq!(
            f.backend.fixture.played.last().unwrap(),
            "Fresh announcement may play."
        );
    }
    #[test]
    fn announcement_fixture_mute_and_disconnect_clear_the_actual_queue() {
        for message in [
            Host::Mute { on: true },
            Host::TurnEnd {
                turn: 1,
                reason: protocol::Reason::Disconnected,
            },
        ] {
            let mut f = SpeechFixture::new();
            f.announcement("a", "Never played after cancellation.");
            f.announcement("b", "Also pending and canceled.");
            f.host(message);
            f.host(Host::Mute { on: false });
            for _ in 0..5 {
                f.tick();
            }
            assert!(f.backend.fixture.played.is_empty());
        }
    }

    #[test]
    fn drained_hearing_race_never_resumes_a_queued_announcement() {
        let mut f = SpeechFixture::new();
        f.announcement("current", "Current announcement starts here.");
        f.announcement("pending", "Stale pending announcement must not play.");
        let epoch = f.backend.epoch;
        // Capture detected activity; Hearing is still behind an older Drained in the bus.
        f.backend.fixture.fence.invalidate(1);
        assert!(f.backend.fixture.tick().is_none());
        assert!(!f.backend.can_drain(epoch));
        // Even if main checked Drained just before capture invalidated, the actions
        // it computed must not clear the capture fence or enqueue playable speech.
        let mut actions = vec![];
        f.controller.drained(&mut vec![], &mut actions);
        f.apply(actions);
        assert!(!f.backend.can_drain(f.backend.epoch));
        f.tick();
        assert!(f.backend.fixture.played.is_empty());
        f.user(1); // Only this acknowledges the pending Hearing fence.
        for _ in 0..5 {
            f.tick();
        }
        assert!(f.backend.fixture.played.is_empty());
        f.controller
            .transcript(1, "fresh user prompt".into(), &mut vec![], &mut vec![]);
        f.host(Host::ReplyEnd {
            turn: 1,
            reply: "fresh".into(),
            text: "Fresh response may play.".into(),
        });
        f.host(Host::TurnEnd {
            turn: 1,
            reason: protocol::Reason::Completed,
        });
        for _ in 0..5 {
            f.tick();
        }
        assert_eq!(f.backend.fixture.played, ["Fresh response may play."]);
    }

    #[test]
    fn oversized_output_and_reply_counts_are_bounded_without_ending_the_turn() {
        for (announcement, text) in [
            (true, "One short sentence. ".repeat(500)),     // >9 KB
            (false, "One short sentence. ".repeat(20_000)), // >300 KB
            (true, "A b c. ".repeat(70)),
        ] {
            let mut f = SpeechFixture::new();
            if announcement {
                f.announcement("large", &text);
            } else {
                f.user(1);
                f.controller
                    .transcript(1, "user".into(), &mut vec![], &mut vec![]);
                let message = Host::ReplyEnd {
                    turn: 1,
                    reply: "large".into(),
                    text,
                };
                assert!(message.validate().is_ok());
                f.host(message);
                f.host(Host::TurnEnd {
                    turn: 1,
                    reason: protocol::Reason::Completed,
                });
            }
            for _ in 0..100 {
                f.tick();
            }
            assert!(f.backend.fixture.played.len() <= 49);
            assert_eq!(
                f.backend.fixture.played.last().unwrap(),
                "The rest is on screen."
            );
            f.announcement("after", "Conversation remains alive now.");
            for _ in 0..5 {
                f.tick();
            }
            assert_eq!(
                f.backend.fixture.played.last().unwrap(),
                "Conversation remains alive now."
            );
        }
        let mut f = SpeechFixture::new();
        f.user(1);
        f.controller
            .transcript(1, "user".into(), &mut vec![], &mut vec![]);
        for i in 0..300 {
            f.host(Host::ReplyEnd {
                turn: 1,
                reply: format!("reply-{i}"),
                text: "A b c.".into(),
            });
        }
        f.host(Host::TurnEnd {
            turn: 1,
            reason: protocol::Reason::Completed,
        });
        for _ in 0..100 {
            f.tick();
        }
        assert!(f.backend.fixture.played.len() <= 49);
        f.announcement("after-cap", "Conversation is still running.");
        f.tick();
        assert_eq!(
            f.backend.fixture.played.last().unwrap(),
            "Conversation is still running."
        );
    }
}
