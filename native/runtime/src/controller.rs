use crate::protocol::{Engine, Host, Participant, Reason, Status, MAX_TEXT_BYTES};
use crate::speech::Speech;
use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

/// Silence after a committed user turn before a short acknowledgement is spoken.
pub const ACK_DELAY: Duration = Duration::from_millis(600);
/// Neutral, success-free phrases, synthesized once at engine start and rotated.
/// They are never sent to the agent or the host transcript.
pub const ACK_PHRASES: [&str; 3] = ["Okay.", "On it.", "Let me check."];

#[derive(Debug, PartialEq)]
pub enum Action {
    Cancel,
    UserActivity(u64),
    Speak(String),
    Finish,
    Mute(bool),
    Pause(bool),
    /// Play the cached acknowledgement phrase with this index, on the current generation.
    Ack(usize),
    /// Voice for every following `Speak`/`Ack` until the next `Cancel`, which resets it to
    /// `default`. Only emitted when it differs from the voice already in force.
    Voice(String),
}

/// Replies from other speakers waiting for the floor. Bounded by count and bytes.
const QUEUE_MAX_ITEMS: usize = 8;
const QUEUE_MAX_BYTES: usize = 64 * 1024;

struct Queued {
    turn: u64,
    speaker: usize,
    id: String,
    text: String,
    final_: bool,
}

struct Reply {
    speaker: usize,
    id: String,
    raw: String,
    speech: Speech,
    emitted: bool,
}

/// Single-owner conversation state. Backend generations independently prevent a
/// synthesis result racing a cancellation from ever reaching playback.
pub struct Controller {
    pub muted: bool,
    pub ready: bool,
    turn: u64,
    active: bool,
    hearing: Option<u64>,
    reply: Option<Reply>,
    finished: HashSet<String>,
    playing: bool,
    announcing: bool,
    announcements: VecDeque<(usize, String, String)>,
    seen_announcements: VecDeque<String>,
    status: Status,
    speech_chunks: usize,
    speech_clipped: bool,
    paused: bool,
    /// Set once at start; never in fixture mode, which must stay deterministic.
    pub ack_enabled: bool,
    ack_armed: Option<Instant>,
    ack_next: usize,
    roster: Vec<Participant>,
    /// Who the current turn was addressed to; the ack and "thinking" status are theirs.
    addressee: usize,
    /// Whose voice the audio in flight uses.
    speaking_as: usize,
    /// Voice the backend currently has in force (reset to `default` by every `Cancel`).
    voice: String,
    /// Replies from non-floor speakers, promoted in order once the floor is free.
    queue: VecDeque<Queued>,
    queue_bytes: usize,
    status_speaker: Option<usize>,
}

impl Default for Controller {
    fn default() -> Self {
        Self {
            muted: false,
            ready: false,
            turn: 0,
            active: false,
            hearing: None,
            reply: None,
            finished: HashSet::new(),
            playing: false,
            announcing: false,
            announcements: VecDeque::new(),
            seen_announcements: VecDeque::new(),
            status: Status::Loading,
            speech_chunks: 0,
            speech_clipped: false,
            paused: false,
            ack_enabled: false,
            ack_armed: None,
            ack_next: 0,
            roster: vec![Participant::implicit()],
            addressee: 0,
            speaking_as: 0,
            voice: "default".into(),
            queue: VecDeque::new(),
            queue_bytes: 0,
            status_speaker: None,
        }
    }
}

impl Controller {
    /// Fix the roster for this conversation. Called once, before anything is spoken.
    pub fn set_roster(&mut self, roster: Vec<Participant>) {
        if !roster.is_empty() {
            self.roster = roster;
        }
    }
    fn index_of(&self, id: &str) -> Result<usize, &'static str> {
        self.roster
            .iter()
            .position(|p| p.id == id)
            .ok_or("Unknown speaker")
    }
    fn id_of(&self, index: usize) -> String {
        self.roster[index].id.clone()
    }
    /// The participant the user would hear (or be waiting on) right now.
    fn current_speaker(&self) -> usize {
        match &self.reply {
            Some(reply) => reply.speaker,
            None if self.playing => self.speaking_as,
            None => self.addressee,
        }
    }
    fn use_voice(&mut self, speaker: usize, actions: &mut Vec<Action>) {
        self.speaking_as = speaker;
        let voice = &self.roster[speaker].voice;
        if *voice != self.voice {
            self.voice = voice.clone();
            actions.push(Action::Voice(self.voice.clone()));
        }
    }
    fn push_cancel(&mut self, actions: &mut Vec<Action>) {
        actions.push(Action::Cancel);
        self.voice = "default".into();
    }
    fn clear_queue(&mut self) {
        self.queue.clear();
        self.queue_bytes = 0;
    }
    fn speak(
        &mut self,
        chunks: Vec<String>,
        clipped: bool,
        out: &mut Vec<Engine>,
        actions: &mut Vec<Action>,
    ) {
        if self.speech_clipped {
            return;
        }
        const LIMIT: usize = 48;
        let overflow = clipped || self.speech_chunks.saturating_add(chunks.len()) > LIMIT;
        if !chunks.is_empty() {
            self.ack_armed = None;
        }
        for chunk in chunks
            .into_iter()
            .take(LIMIT.saturating_sub(self.speech_chunks))
        {
            self.use_voice(self.speaking_as, actions);
            actions.push(Action::Speak(chunk));
            self.speech_chunks += 1;
            self.playing = true;
            self.status(Status::Speaking, out);
        }
        if overflow && !self.speech_clipped {
            self.speech_clipped = true;
            self.clipping_notice(out, actions);
        }
    }
    fn clipping_notice(&mut self, out: &mut Vec<Engine>, actions: &mut Vec<Action>) {
        self.use_voice(self.speaking_as, actions);
        actions.push(Action::Speak("The rest is on screen.".into()));
        self.playing = true;
        self.status(Status::Speaking, out);
    }
    fn status(&mut self, status: Status, out: &mut Vec<Engine>) {
        let speaker = match status {
            Status::Thinking => Some(self.addressee),
            Status::Speaking => Some(self.speaking_as),
            _ => None,
        };
        if self.status != status || self.status_speaker != speaker {
            self.status = status;
            self.status_speaker = speaker;
            out.push(Engine::Status {
                state: status,
                speaker: speaker.map(|index| self.id_of(index)),
            });
        }
    }
    fn rest(&mut self, out: &mut Vec<Engine>) {
        self.status(
            if self.muted {
                Status::Muted
            } else if self.active {
                Status::Thinking
            } else {
                Status::Listening
            },
            out,
        );
    }
    fn cancel(&mut self, actions: &mut Vec<Action>) {
        self.push_cancel(actions);
        self.clear_queue();
        self.reply = None;
        self.playing = false;
        self.announcing = false;
        self.ack_armed = None;
    }
    /// Speak one cached acknowledgement if reply audio has not begun within
    /// `ACK_DELAY` of the committed turn. At most once per turn; never while
    /// paused, muted, listening, or after cancellation. It does not change the
    /// voice status: the assistant is still thinking.
    pub fn poll_ack(&mut self, now: Instant, actions: &mut Vec<Action>) {
        let Some(armed) = self.ack_armed else { return };
        if now.saturating_duration_since(armed) < ACK_DELAY {
            return;
        }
        self.ack_armed = None;
        if !self.ack_enabled
            || !self.ready
            || !self.active
            || self.muted
            || self.paused
            || self.hearing.is_some()
            || self.playing
            || self.speech_chunks > 0
        {
            return;
        }
        self.use_voice(self.addressee, actions);
        actions.push(Action::Ack(self.ack_next));
        self.ack_next = (self.ack_next + 1) % ACK_PHRASES.len();
    }
    #[cfg(test)]
    fn armed_at(&self) -> Option<Instant> {
        self.ack_armed
    }
    fn invalidate_announcements(&mut self) {
        self.announcements.clear();
        self.announcing = false;
    }
    fn schedule_announcement(&mut self, out: &mut Vec<Engine>, actions: &mut Vec<Action>) {
        if !self.ready
            || self.muted
            || self.paused
            || self.active
            || self.hearing.is_some()
            || self.playing
        {
            return;
        }
        while let Some((speaker, _id, text)) = self.announcements.pop_front() {
            let chunks = Speech::default().push(&text, true);
            if chunks.is_empty() {
                continue;
            }
            // New generation makes a duplicate/stale drained event harmless.
            self.push_cancel(actions);
            self.speaking_as = speaker;
            self.speech_chunks = 0;
            self.speech_clipped = false;
            // Host-side UTF-8 clipping may leave up to three bytes of slack.
            // A near-boundary cue is deliberately approximate, not a wire flag.
            self.speak(chunks, text.len().saturating_add(3) >= 8192, out, actions);
            actions.push(Action::Finish);
            self.playing = true;
            self.announcing = true;
            self.status(Status::Speaking, out);
            break;
        }
    }
    pub fn ready(&mut self, out: &mut Vec<Engine>, actions: &mut Vec<Action>) {
        self.ready = true;
        self.rest(out);
        self.schedule_announcement(out, actions);
    }
    pub fn hearing(&mut self, id: u64, out: &mut Vec<Engine>, actions: &mut Vec<Action>) {
        if self.muted || !self.ready {
            return;
        }
        self.invalidate_announcements();
        if self.active {
            out.push(Engine::Interrupt {
                turn: self.turn,
                speaker: Some(self.id_of(self.current_speaker())),
                played_ms: None,
                generated_ms: None,
                heard: None,
            });
        }
        self.cancel(actions);
        actions.push(Action::UserActivity(id));
        self.active = false;
        self.hearing = Some(id);
        self.status(Status::Hearing, out);
    }
    pub fn transcript(
        &mut self,
        id: u64,
        text: String,
        out: &mut Vec<Engine>,
        actions: &mut Vec<Action>,
    ) {
        if self.muted || self.hearing != Some(id) {
            return;
        }
        self.hearing = None;
        // Normalize C0 bytes one-for-one, preserving LF/TAB and word boundaries.
        // Reuse the UTF-8 buffer; no escaped transcript-sized copy is required.
        let text = if text.len() <= MAX_TEXT_BYTES {
            normalize_transcript(text)
        } else {
            text
        };
        if text.trim().is_empty() || text.len() > MAX_TEXT_BYTES {
            self.rest(out);
            self.schedule_announcement(out, actions);
            return;
        }
        let resolved = crate::addressing::resolve(&self.roster, &text);
        self.addressee = resolved.index;
        self.speaking_as = resolved.index;
        self.turn += 1;
        self.active = true;
        self.ack_armed = self.ack_enabled.then(Instant::now);
        self.finished.clear();
        self.speech_chunks = 0;
        self.speech_clipped = false;
        out.push(Engine::Prompt {
            turn: self.turn,
            speaker: self.id_of(resolved.index),
            addressed: resolved.addressed,
            text: resolved.text,
        });
        self.status(Status::Thinking, out);
    }
    pub fn drained(&mut self, out: &mut Vec<Engine>, actions: &mut Vec<Action>) {
        self.playing = false;
        self.announcing = false;
        // The next queued speaker takes the floor without a listening flicker in between.
        let promoted = self.promote(out, actions);
        // A later user utterance must not be overwritten by a prior speech completion.
        if !promoted && self.hearing.is_none() {
            self.rest(out);
        }
        self.schedule_announcement(out, actions);
    }
    pub fn host(
        &mut self,
        message: Host,
        out: &mut Vec<Engine>,
        actions: &mut Vec<Action>,
    ) -> Result<(), &'static str> {
        match message {
            Host::Mute { on } => {
                if self.muted == on {
                    return Ok(());
                }
                self.muted = on;
                self.hearing = None;
                if on {
                    self.invalidate_announcements();
                    if self.active {
                        out.push(Engine::Interrupt {
                            turn: self.turn,
                            speaker: Some(self.id_of(self.current_speaker())),
                            played_ms: None,
                            generated_ms: None,
                            heard: None,
                        });
                    }
                    self.active = false;
                    self.cancel(actions);
                }
                actions.push(Action::Mute(on));
                if self.ready {
                    self.rest(out);
                }
            }
            Host::Pause { on } => {
                if self.paused == on {
                    return Ok(());
                }
                self.paused = on;
                actions.push(Action::Pause(on));
                if !on {
                    // Announcements held while paused may speak once output resumes.
                    self.schedule_announcement(out, actions);
                }
            }
            Host::ReplyDelta {
                turn,
                speaker,
                reply,
                text,
            } => {
                let speaker = self.index_of(&speaker)?;
                self.reply(turn, speaker, reply, text, false, out, actions)?
            }
            Host::ReplyEnd {
                turn,
                speaker,
                reply,
                text,
            } => {
                let speaker = self.index_of(&speaker)?;
                self.reply(turn, speaker, reply, text, true, out, actions)?
            }
            Host::Announcement {
                speaker,
                reply,
                text,
            } => {
                let speaker = self.index_of(&speaker)?;
                if self.seen_announcements.contains(&reply) {
                    return Ok(());
                }
                if self.seen_announcements.len() == 256 {
                    self.seen_announcements.pop_front();
                }
                self.seen_announcements.push_back(reply.clone());
                if self.muted {
                    return Ok(());
                }
                if self.announcements.len() == 4 {
                    self.announcements.pop_front();
                }
                let text = clip(&text, 8192).to_owned();
                self.announcements.push_back((speaker, reply, text));
                self.schedule_announcement(out, actions);
            }
            Host::TurnEnd { turn, reason } => {
                if reason == Reason::Disconnected {
                    self.invalidate_announcements();
                    self.cancel(actions);
                    self.active = false;
                    self.hearing = None;
                    self.rest(out);
                    return Ok(());
                }
                if reason != Reason::Completed && turn == self.turn && self.active {
                    self.invalidate_announcements();
                    self.cancel(actions);
                    self.active = false;
                    self.rest(out);
                    return Ok(());
                }
                if !self.active || turn != self.turn {
                    return Ok(());
                }
                if reason == Reason::Completed {
                    self.ack_armed = None;
                    if let Some(reply) = self.reply.take() {
                        self.finish_reply(reply, out, actions);
                    }
                    actions.push(Action::Finish);
                } else {
                    self.invalidate_announcements();
                    self.cancel(actions);
                }
                self.active = false;
                if reason != Reason::Completed {
                    self.rest(out);
                } else if !self.playing {
                    let _ = self.promote(out, actions);
                }
            }
            Host::PromptFailed { turn, message } => {
                let _ = message; // Host failure details may contain transcript content. Never log.
                if self.active && turn == self.turn {
                    self.invalidate_announcements();
                    self.cancel(actions);
                    self.active = false;
                    self.rest(out);
                }
            }
            Host::Shutdown {} => {
                self.invalidate_announcements();
                self.cancel(actions);
            }
            Host::Init { .. } => return Err("init is only valid once"),
        }
        Ok(())
    }
    fn finish_reply(&mut self, mut reply: Reply, out: &mut Vec<Engine>, actions: &mut Vec<Action>) {
        self.speak(reply.speech.push("", true), false, out, actions);
        self.finished.insert(reply.id);
    }
    /// Whoever holds the floor: the speaker of the reply being streamed, or of audio that is
    /// still playing. `None` when the floor is free.
    fn floor_holder(&self) -> Option<usize> {
        match &self.reply {
            Some(reply) => Some(reply.speaker),
            None if self.playing => Some(self.speaking_as),
            None => None,
        }
    }
    fn enqueue(&mut self, item: Queued) {
        if self.queue.len() >= QUEUE_MAX_ITEMS
            || self.queue_bytes.saturating_add(item.text.len()) > QUEUE_MAX_BYTES
        {
            return;
        }
        self.queue_bytes += item.text.len();
        self.queue.push_back(item);
    }
    /// Give the floor to the next queued speaker once the current one is done speaking.
    /// Everything that speaker queued is replayed in order on a fresh generation.
    fn promote(&mut self, out: &mut Vec<Engine>, actions: &mut Vec<Action>) -> bool {
        if self.playing || self.reply.is_some() || self.muted || self.paused || !self.ready {
            return false;
        }
        let Some(first) = self.queue.front().map(|item| item.speaker) else {
            return false;
        };
        let (mine, rest): (VecDeque<_>, VecDeque<_>) =
            self.queue.drain(..).partition(|item| item.speaker == first);
        self.queue = rest;
        self.queue_bytes = self.queue.iter().map(|item| item.text.len()).sum();
        self.push_cancel(actions);
        self.speaking_as = first;
        self.speech_chunks = 0;
        self.speech_clipped = false;
        let mut finished = false;
        for item in mine {
            finished = item.final_;
            let _ = self.accept(
                item.turn,
                item.speaker,
                item.id,
                item.text,
                item.final_,
                out,
                actions,
            );
        }
        if finished {
            actions.push(Action::Finish);
        }
        true
    }
    #[allow(clippy::too_many_arguments)]
    fn reply(
        &mut self,
        turn: u64,
        speaker: usize,
        id: String,
        text: String,
        final_: bool,
        out: &mut Vec<Engine>,
        actions: &mut Vec<Action>,
    ) -> Result<(), &'static str> {
        if !self.ready
            || self.muted
            || !self.active
            || turn != self.turn
            || self.finished.contains(&id)
        {
            return Ok(());
        }
        if self.floor_holder().is_some_and(|holder| holder != speaker) {
            self.enqueue(Queued {
                turn,
                speaker,
                id,
                text,
                final_,
            });
            return Ok(());
        }
        self.accept(turn, speaker, id, text, final_, out, actions)
    }
    #[allow(clippy::too_many_arguments)]
    fn accept(
        &mut self,
        _turn: u64,
        speaker: usize,
        id: String,
        text: String,
        final_: bool,
        out: &mut Vec<Engine>,
        actions: &mut Vec<Action>,
    ) -> Result<(), &'static str> {
        self.speaking_as = speaker;
        if self.reply.as_ref().is_some_and(|r| r.id != id) {
            // Superseding a reply invalidates queued chunks, not just future deltas.
            let old_id = self.reply.as_ref().unwrap().id.clone();
            self.finished.insert(old_id);
            let clipped = self.speech_clipped;
            self.cancel(actions);
            self.rest(out);
            if clipped {
                // Cancellation discarded the old queued cue, not the turn's
                // speech budget. Keep one cue in the replacement generation.
                self.clipping_notice(out, actions);
            }
        }
        if self.finished.len() >= 256 {
            self.speak(vec![], true, out, actions);
            return Ok(());
        }
        let reply = self.reply.get_or_insert_with(|| Reply {
            speaker,
            id,
            raw: String::new(),
            speech: Speech::default(),
            emitted: false,
        });
        if final_ && !text.starts_with(&reply.raw) {
            if !reply.emitted {
                reply.raw.clear();
            }
            reply.speech = Speech::default();
        }
        let addition = if final_ {
            // Full final text is an authoritative fallback. Append only its unstreamed suffix.
            // Divergent final text cannot safely replace audio already heard: do not replay.
            if reply.raw.is_empty() {
                text.as_str()
            } else {
                text.strip_prefix(&reply.raw).unwrap_or("")
            }
        } else {
            text.as_str()
        };
        let clipped = addition.len() > MAX_TEXT_BYTES.saturating_sub(reply.raw.len());
        let addition = clip(addition, MAX_TEXT_BYTES.saturating_sub(reply.raw.len()));
        let chunks = reply.speech.push(addition, final_ || clipped);
        reply.raw.push_str(addition);
        let before = actions.len();
        self.speak(chunks, clipped, out, actions);
        self.reply.as_mut().unwrap().emitted |= actions[before..]
            .iter()
            .any(|a| matches!(a, Action::Speak(_)));
        if final_ {
            let reply = self.reply.take().unwrap();
            self.finished.insert(reply.id);
        }
        Ok(())
    }
}

fn clip(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn normalize_transcript(text: String) -> String {
    let mut bytes = text.into_bytes();
    for byte in &mut bytes {
        if *byte < 32 && !matches!(*byte, b'\n' | b'\t') {
            *byte = b' ';
        }
    }
    String::from_utf8(bytes).expect("ASCII control normalization preserves UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn announcement_truncation_cue_allows_utf8_boundary_slack() {
        for (text, clipped) in [
            (format!("a{}", "🦀".repeat(3000)), true),
            ("a".repeat(8192), true),
            ("a".repeat(8188), false),
        ] {
            let mut c = Controller::default();
            let mut out = vec![];
            let mut actions = vec![];
            c.ready(&mut out, &mut actions);
            c.host(
                Host::Announcement {
                    speaker: "a1".into(),
                    reply: "a".into(),
                    text,
                },
                &mut out,
                &mut actions,
            )
            .unwrap();
            let cues = actions
                .iter()
                .filter(|a| matches!(a, Action::Speak(text) if text == "The rest is on screen."))
                .count();
            assert_eq!(cues, usize::from(clipped));
        }
    }
    #[test]
    fn divergent_final_discards_unspoken_draft_without_replaying_spoken_prefix() {
        let mut c = active();
        let mut actions = reply(&mut c, "The result is ready. The value is 1", false);
        actions.extend(reply(&mut c, "The result is ready. The value is 2.", true));
        assert_eq!(actions, [Action::Speak("The result is ready.".into())]);
    }
    #[test]
    fn unspoken_divergent_final_is_authoritative_but_spoken_prefix_is_not_replayed() {
        let mut c = active();
        assert!(reply(&mut c, "The result is 1", false).is_empty());
        assert_eq!(
            reply(&mut c, "The result is 2.", true),
            [Action::Speak("The result is 2.".into())]
        );
        let mut c = active();
        assert!(!reply(&mut c, "This is already spoken. ", false).is_empty());
        let actions = reply(&mut c, "A contradictory final text.", true);
        assert!(!actions
            .iter()
            .any(|a| matches!(a, Action::Speak(text) if text.contains("contradictory"))));
    }
    #[test]
    fn transcript_controls_cannot_expand_the_prompt_frame() {
        assert_eq!(normalize_transcript("a\u{1}b\r\n\tc".into()), "a b \n\tc");
        let mut c = Controller::default();
        c.ready(&mut vec![], &mut vec![]);
        c.hearing(1, &mut vec![], &mut vec![]);
        let text = format!("a{}", "\u{1}".repeat(MAX_TEXT_BYTES - 1));
        let mut out = vec![];
        c.transcript(1, text, &mut out, &mut vec![]);
        let prompt = out
            .iter()
            .find(|e| matches!(e, Engine::Prompt { .. }))
            .unwrap();
        assert!(serde_json::to_vec(prompt).unwrap().len() < crate::protocol::MAX_FRAME_BYTES);
    }
    #[test]
    fn abandoned_transcript_schedules_announcements() {
        let mut c = Controller::default();
        let mut out = vec![];
        let mut actions = vec![];
        c.ready(&mut out, &mut actions);
        c.hearing(1, &mut out, &mut actions);
        c.host(
            Host::Announcement {
                speaker: "a1".into(),
                reply: "a".into(),
                text: "A worker finished.".into(),
            },
            &mut out,
            &mut actions,
        )
        .unwrap();
        actions.clear();
        c.transcript(1, String::new(), &mut out, &mut actions);
        assert!(actions.iter().any(|a| matches!(a, Action::Speak(_))));
    }
    #[test]
    fn duplicate_terminal_does_not_cancel_background_announcement() {
        let mut c = active();
        let mut out = vec![];
        let mut actions = vec![];
        c.host(
            Host::TurnEnd {
                turn: 1,
                reason: Reason::Failed,
            },
            &mut out,
            &mut actions,
        )
        .unwrap();
        c.host(
            Host::Announcement {
                speaker: "a1".into(),
                reply: "a".into(),
                text: "A worker finished.".into(),
            },
            &mut out,
            &mut actions,
        )
        .unwrap();
        actions.clear();
        c.host(
            Host::TurnEnd {
                turn: 1,
                reason: Reason::Failed,
            },
            &mut out,
            &mut actions,
        )
        .unwrap();
        assert!(actions.is_empty());
        assert!(c.playing);
    }
    #[test]
    fn replacement_without_chunks_returns_to_thinking() {
        let mut c = active();
        assert_eq!(
            reply(&mut c, "This is audible now. ", false),
            [Action::Speak("This is audible now.".into())]
        );
        assert!(c.playing);
        c.host(
            Host::ReplyDelta {
                turn: 1,
                speaker: "a1".into(),
                reply: "replacement".into(),
                text: "short".into(),
            },
            &mut vec![],
            &mut vec![],
        )
        .unwrap();
        assert!(!c.playing);
        assert_eq!(c.status, Status::Thinking);
    }
    fn active() -> Controller {
        let mut c = Controller::default();
        let mut out = vec![];
        let mut a = vec![];
        c.ready(&mut out, &mut a);
        c.hearing(1, &mut out, &mut a);
        c.transcript(1, "test prompt".into(), &mut out, &mut a);
        c
    }
    fn reply(c: &mut Controller, text: &str, final_: bool) -> Vec<Action> {
        let mut a = vec![];
        let mut out = vec![];
        let message = if final_ {
            Host::ReplyEnd {
                turn: 1,
                speaker: "a1".into(),
                reply: "r".into(),
                text: text.into(),
            }
        } else {
            Host::ReplyDelta {
                turn: 1,
                speaker: "a1".into(),
                reply: "r".into(),
                text: text.into(),
            }
        };
        c.host(message, &mut out, &mut a).unwrap();
        a
    }
    #[test]
    fn final_fallback_and_no_double_speech() {
        let mut c = active();
        let mut a = reply(&mut c, "This is the first sentence.", false);
        a.extend(reply(
            &mut c,
            "This is the first sentence. And the second.",
            true,
        ));
        a.extend(reply(
            &mut c,
            "This is the first sentence. And the second.",
            true,
        ));
        let text = a
            .into_iter()
            .filter_map(|a| {
                if let Action::Speak(s) = a {
                    Some(s)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(text, "This is the first sentence. And the second.");
        assert_eq!(
            reply(&mut active(), "Full reply only.", true),
            vec![Action::Speak("Full reply only.".into())]
        );
    }
    #[test]
    fn cancellation_and_stale_replies() {
        let mut c = active();
        let mut out = vec![];
        let mut a = vec![];
        reply(&mut c, "unfinished", false);
        c.hearing(2, &mut out, &mut a);
        assert!(a.contains(&Action::Cancel));
        assert!(reply(&mut c, "stale reply", true).is_empty());
        c.transcript(2, "new prompt".into(), &mut out, &mut a);
        assert!(reply(&mut c, "old turn", true).is_empty());
    }
    #[test]
    fn pause_keeps_the_turn_and_its_speech_for_resume() {
        let mut c = active();
        let mut out = vec![];
        let mut a = vec![];
        c.host(
            Host::ReplyEnd {
                turn: 1,
                speaker: "a1".into(),
                reply: "r".into(),
                text: "A spoken reply.".into(),
            },
            &mut out,
            &mut a,
        )
        .unwrap();
        assert_eq!(a, [Action::Speak("A spoken reply.".into())]);
        assert_eq!(c.status, Status::Speaking);

        out.clear();
        a.clear();
        c.host(Host::Pause { on: true }, &mut out, &mut a).unwrap();
        assert_eq!(a, [Action::Pause(true)]);
        assert!(out.is_empty(), "pausing is not a voice-state change");
        assert!(c.active && c.playing, "pause must not cancel the turn");

        // Speech completed while paused waits for resume instead of being dropped.
        out.clear();
        a.clear();
        c.host(
            Host::TurnEnd {
                turn: 1,
                reason: Reason::Completed,
            },
            &mut out,
            &mut a,
        )
        .unwrap();
        assert_eq!(a, [Action::Finish]);
        assert!(!c.active);

        a.clear();
        c.host(Host::Pause { on: false }, &mut out, &mut a).unwrap();
        assert_eq!(a, [Action::Pause(false)]);
    }

    #[test]
    fn barge_in_while_paused_discards_held_speech() {
        let mut c = active();
        let mut out = vec![];
        let mut a = vec![];
        c.host(
            Host::ReplyEnd {
                turn: 1,
                speaker: "a1".into(),
                reply: "r".into(),
                text: "Held speech.".into(),
            },
            &mut out,
            &mut a,
        )
        .unwrap();
        c.host(Host::Pause { on: true }, &mut out, &mut a).unwrap();
        out.clear();
        a.clear();
        c.hearing(2, &mut out, &mut a);
        assert!(a.contains(&Action::Cancel), "held speech is discarded");
        assert!(matches!(
            out.first(),
            Some(Engine::Interrupt { turn: 1, .. })
        ));
        assert!(!c.active);
    }
    #[test]
    fn paused_announcements_wait_for_resume() {
        let mut c = Controller::default();
        let mut out = vec![];
        let mut a = vec![];
        c.ready(&mut out, &mut a);
        out.clear();
        c.host(Host::Pause { on: true }, &mut out, &mut a).unwrap();
        assert_eq!(a, [Action::Pause(true)]);
        assert!(out.is_empty());
        a.clear();
        c.host(
            Host::Announcement {
                speaker: "a1".into(),
                reply: "a".into(),
                text: "A worker update.".into(),
            },
            &mut out,
            &mut a,
        )
        .unwrap();
        assert!(a.is_empty(), "a paused announcement must not queue audio");

        c.host(Host::Pause { on: false }, &mut out, &mut a).unwrap();
        assert!(a.contains(&Action::Pause(false)));
        assert!(a.iter().any(|action| matches!(action, Action::Speak(_))));
        assert!(a.contains(&Action::Finish));
    }

    #[test]
    fn mute_invalidates_capture_and_reply() {
        let mut c = active();
        let mut out = vec![];
        let mut a = vec![];
        c.host(Host::Mute { on: true }, &mut out, &mut a).unwrap();
        c.host(Host::Mute { on: false }, &mut out, &mut a).unwrap();
        c.transcript(1, "old capture".into(), &mut out, &mut a);
        assert!(reply(&mut c, "muted old turn", true).is_empty());
        assert_eq!(c.status, Status::Listening);
    }
    #[test]
    fn announcement_storage_is_bounded_and_new_activity_invalidates_it() {
        let mut c = active();
        let mut out = vec![];
        let mut a = vec![];
        for i in 0..1000 {
            c.host(
                Host::Announcement {
                    speaker: "a1".into(),
                    reply: format!("a-{i}"),
                    text: "A worker update.".into(),
                },
                &mut out,
                &mut a,
            )
            .unwrap();
        }
        assert_eq!(c.announcements.len(), 4);
        assert_eq!(c.seen_announcements.len(), 256);
        assert!(!a.iter().any(|a| matches!(a, Action::Speak(_))));
        c.hearing(2, &mut out, &mut a);
        assert!(c.announcements.is_empty());
    }

    fn acking() -> Controller {
        let mut c = Controller {
            ack_enabled: true,
            ..Controller::default()
        };
        let (mut out, mut a) = (vec![], vec![]);
        c.ready(&mut out, &mut a);
        c.hearing(1, &mut out, &mut a);
        c.transcript(1, "test prompt".into(), &mut out, &mut a);
        c
    }
    fn due(c: &mut Controller) -> Vec<Action> {
        let mut a = vec![];
        let at = c.armed_at().map_or_else(Instant::now, |t| t + ACK_DELAY);
        c.poll_ack(at, &mut a);
        a
    }
    #[test]
    fn acknowledges_once_after_the_delay_and_rotates_phrases() {
        let mut c = acking();
        let armed = c.armed_at().unwrap();
        let mut a = vec![];
        c.poll_ack(armed + ACK_DELAY - Duration::from_millis(1), &mut a);
        assert!(a.is_empty(), "too early");
        c.poll_ack(armed + ACK_DELAY, &mut a);
        assert_eq!(a, [Action::Ack(0)]);
        assert_eq!(c.status, Status::Thinking, "acknowledging is not replying");
        assert!(due(&mut c).is_empty(), "at most one per turn");
        for expected in [1, 2, 0] {
            let (mut out, mut a) = (vec![], vec![]);
            c.hearing(9, &mut out, &mut a);
            c.transcript(9, "again".into(), &mut out, &mut a);
            assert_eq!(due(&mut c), [Action::Ack(expected)]);
        }
    }
    #[test]
    fn phrases_are_short_and_claim_nothing() {
        for phrase in ACK_PHRASES {
            assert!(phrase.split_whitespace().count() <= 3);
            let lower = phrase.to_lowercase();
            for word in ["done", "fixed", "finished", "complete", "success", "ready"] {
                assert!(!lower.contains(word), "{phrase}");
            }
        }
    }
    #[test]
    fn disabled_by_default_and_for_unusable_transcripts() {
        let mut c = active();
        assert!(c.armed_at().is_none());
        assert!(due(&mut c).is_empty());
        let mut c = Controller {
            ack_enabled: true,
            ..Controller::default()
        };
        let (mut out, mut a) = (vec![], vec![]);
        c.ready(&mut out, &mut a);
        c.hearing(1, &mut out, &mut a);
        c.transcript(1, "  \t ".into(), &mut out, &mut a);
        assert!(c.armed_at().is_none());
        c.hearing(2, &mut out, &mut a);
        c.transcript(2, "x".repeat(MAX_TEXT_BYTES + 1), &mut out, &mut a);
        assert!(c.armed_at().is_none());
    }
    #[test]
    fn barge_in_cancel_and_failure_disarm() {
        let (mut out, mut a) = (vec![], vec![]);
        let mut c = acking();
        let at = c.armed_at().unwrap() + ACK_DELAY;
        c.hearing(2, &mut out, &mut a);
        c.poll_ack(at, &mut a);
        assert!(!a.contains(&Action::Ack(0)));
        for reason in [Reason::Interrupted, Reason::Failed, Reason::Disconnected] {
            let mut c = acking();
            let at = c.armed_at().unwrap() + ACK_DELAY;
            c.host(Host::TurnEnd { turn: 1, reason }, &mut out, &mut a)
                .unwrap();
            c.poll_ack(at, &mut a);
            assert!(!a.contains(&Action::Ack(0)));
        }
        let mut c = acking();
        let at = c.armed_at().unwrap() + ACK_DELAY;
        c.host(
            Host::PromptFailed {
                turn: 1,
                message: String::new(),
            },
            &mut out,
            &mut a,
        )
        .unwrap();
        c.poll_ack(at, &mut a);
        assert!(!a.contains(&Action::Ack(0)));
        let mut c = acking();
        let at = c.armed_at().unwrap() + ACK_DELAY;
        c.host(
            Host::TurnEnd {
                turn: 1,
                reason: Reason::Completed,
            },
            &mut out,
            &mut a,
        )
        .unwrap();
        c.poll_ack(at, &mut a);
        assert!(!a.contains(&Action::Ack(0)), "no ack after completion");
    }
    #[test]
    fn muted_or_paused_output_is_never_acknowledged() {
        let (mut out, mut a) = (vec![], vec![]);
        let mut c = acking();
        let at = c.armed_at().unwrap() + ACK_DELAY;
        c.host(Host::Mute { on: true }, &mut out, &mut a).unwrap();
        c.host(Host::Mute { on: false }, &mut out, &mut a).unwrap();
        a.clear();
        c.poll_ack(at, &mut a);
        assert!(a.is_empty());
        let mut c = acking();
        let at = c.armed_at().unwrap() + ACK_DELAY;
        c.host(Host::Pause { on: true }, &mut out, &mut a).unwrap();
        a.clear();
        c.poll_ack(at, &mut a);
        assert!(a.is_empty());
        c.host(Host::Pause { on: false }, &mut out, &mut a).unwrap();
        a.clear();
        c.poll_ack(at, &mut a);
        assert!(a.is_empty(), "a paused deadline is dropped, not deferred");
    }
    #[test]
    fn early_reply_audio_suppresses_the_acknowledgement() {
        let mut c = acking();
        let at = c.armed_at().unwrap() + ACK_DELAY;
        assert!(!reply(&mut c, "Here is the answer. ", false).is_empty());
        let mut a = vec![];
        c.poll_ack(at, &mut a);
        assert!(a.is_empty());
        // Text that has not yet formed a chunk is not audio: still acknowledged.
        let mut c = acking();
        assert!(reply(&mut c, "Here is", false).is_empty());
        assert_eq!(due(&mut c), [Action::Ack(0)]);
    }
    #[test]
    fn reply_after_the_acknowledgement_queues_behind_it_without_cancelling() {
        let mut c = acking();
        assert_eq!(due(&mut c), [Action::Ack(0)]);
        let a = reply(&mut c, "Here is the answer. ", false);
        assert_eq!(a, [Action::Speak("Here is the answer.".into())]);
        assert!(!a.contains(&Action::Cancel));
        let a = reply(&mut c, "Here is the answer. More.", true);
        assert_eq!(a, [Action::Speak("More.".into())]);
    }
    #[test]
    fn worker_announcements_are_never_acknowledged() {
        let mut c = Controller {
            ack_enabled: true,
            ..Controller::default()
        };
        let (mut out, mut a) = (vec![], vec![]);
        c.ready(&mut out, &mut a);
        c.host(
            Host::Announcement {
                speaker: "a1".into(),
                reply: "w".into(),
                text: "A worker finished.".into(),
            },
            &mut out,
            &mut a,
        )
        .unwrap();
        assert!(a.iter().any(|x| matches!(x, Action::Speak(_))));
        assert!(c.armed_at().is_none());
        assert!(due(&mut c).is_empty());
    }

    fn roster() -> Vec<Participant> {
        [
            ("a1", "Ada", "zoey"),
            ("a2", "Grace", "easton"),
            ("a3", "Linus", "default"),
        ]
        .into_iter()
        .map(|(id, name, voice)| Participant {
            id: id.into(),
            name: name.into(),
            voice: voice.into(),
        })
        .collect()
    }
    fn team() -> Controller {
        let mut c = Controller::default();
        c.set_roster(roster());
        c.ready(&mut vec![], &mut vec![]);
        c
    }
    fn say(
        c: &mut Controller,
        speaker: &str,
        reply: &str,
        text: &str,
        final_: bool,
    ) -> Vec<Action> {
        let mut a = vec![];
        let message = if final_ {
            Host::ReplyEnd {
                turn: 1,
                speaker: speaker.into(),
                reply: reply.into(),
                text: text.into(),
            }
        } else {
            Host::ReplyDelta {
                turn: 1,
                speaker: speaker.into(),
                reply: reply.into(),
                text: text.into(),
            }
        };
        c.host(message, &mut vec![], &mut a).unwrap();
        a
    }
    fn prompt(c: &mut Controller, text: &str) -> (Vec<Engine>, Vec<Action>) {
        let mut out = vec![];
        let mut a = vec![];
        c.hearing(1, &mut out, &mut a);
        c.transcript(1, text.into(), &mut out, &mut a);
        (out, a)
    }
    #[test]
    fn prompt_is_addressed_stripped_and_defaults_to_the_first_agent() {
        let mut c = team();
        let (out, _) = prompt(&mut c, "Grace, check the build.");
        assert!(
            matches!(&out[..], [.., Engine::Prompt { speaker, addressed: true, text, .. }, _] if speaker == "a2" && text == "check the build.")
        );
        let mut c = team();
        let (out, _) = prompt(&mut c, "What is the status?");
        assert!(out.iter().any(|e| matches!(e, Engine::Prompt { speaker, addressed: false, text, .. } if speaker == "a1" && text == "What is the status?")));
        assert!(out.iter().any(|e| matches!(e, Engine::Status { state: Status::Thinking, speaker: Some(s) } if s == "a1")));
    }
    #[test]
    fn each_speaker_is_spoken_in_its_own_voice_and_default_stays_silent_on_the_wire() {
        let mut c = team();
        prompt(&mut c, "Ada, hello");
        assert_eq!(
            say(&mut c, "a1", "r", "Hi there.", true),
            [
                Action::Voice("zoey".into()),
                Action::Speak("Hi there.".into())
            ]
        );
        // A later turn spoken by a default-voice agent needs no voice action after Cancel.
        let mut c = team();
        prompt(&mut c, "Linus, hello");
        assert_eq!(
            say(&mut c, "a3", "r", "Hi there.", true),
            [Action::Speak("Hi there.".into())]
        );
    }
    #[test]
    fn the_acknowledgement_uses_the_addressees_voice() {
        let mut c = team();
        c.ack_enabled = true;
        prompt(&mut c, "Grace, run it");
        let mut a = vec![];
        c.poll_ack(c.armed_at().unwrap() + ACK_DELAY, &mut a);
        assert_eq!(a, [Action::Voice("easton".into()), Action::Ack(0)]);
    }
    #[test]
    fn another_speaker_waits_for_the_floor_then_speaks_in_order() {
        let mut c = team();
        prompt(&mut c, "Ada, hello");
        say(&mut c, "a1", "r1", "Ada speaks first. ", false);
        // Grace has to wait: nothing is synthesized for her yet.
        assert!(say(&mut c, "a2", "r2", "Grace waits her turn.", true).is_empty());
        let mut out = vec![];
        let mut a = vec![];
        c.host(
            Host::TurnEnd {
                turn: 1,
                reason: Reason::Completed,
            },
            &mut out,
            &mut a,
        )
        .unwrap();
        assert!(!a
            .iter()
            .any(|x| matches!(x, Action::Speak(t) if t.contains("Grace"))));
        // Ada's audio drains: Grace takes the floor on a fresh generation in her voice.
        let mut a = vec![];
        c.drained(&mut out, &mut a);
        assert_eq!(
            a,
            [
                Action::Cancel,
                Action::Voice("easton".into()),
                Action::Speak("Grace waits her turn.".into()),
                Action::Finish
            ]
        );
        assert!(out.iter().any(|e| matches!(e, Engine::Status { state: Status::Speaking, speaker: Some(s) } if s == "a2")));
    }
    #[test]
    fn user_speech_cancels_active_and_queued_speakers() {
        let mut c = team();
        prompt(&mut c, "Ada, hello");
        say(&mut c, "a1", "r1", "Ada speaks. ", false);
        say(&mut c, "a2", "r2", "Grace is queued.", true);
        let mut out = vec![];
        let mut a = vec![];
        c.hearing(2, &mut out, &mut a);
        assert!(a.contains(&Action::Cancel));
        assert!(out
            .iter()
            .any(|e| matches!(e, Engine::Interrupt { speaker: Some(s), .. } if s == "a1")));
        c.drained(&mut out, &mut a);
        assert!(!a
            .iter()
            .any(|x| matches!(x, Action::Speak(t) if t.contains("Grace"))));
    }
    #[test]
    fn the_floor_queue_is_bounded() {
        let mut c = team();
        prompt(&mut c, "Ada, hello");
        say(&mut c, "a1", "r1", "Ada speaks. ", false);
        for i in 0..50 {
            say(&mut c, "a2", &format!("q{i}"), &"x ".repeat(4000), false);
        }
        assert!(c.queue.len() <= QUEUE_MAX_ITEMS && c.queue_bytes <= QUEUE_MAX_BYTES);
    }
    #[test]
    fn announcements_are_spoken_by_the_named_speaker() {
        let mut c = team();
        let mut out = vec![];
        let mut a = vec![];
        c.host(
            Host::Announcement {
                speaker: "a2".into(),
                reply: "x".into(),
                text: "The build finished.".into(),
            },
            &mut out,
            &mut a,
        )
        .unwrap();
        assert_eq!(
            a,
            [
                Action::Cancel,
                Action::Voice("easton".into()),
                Action::Speak("The build finished.".into()),
                Action::Finish
            ]
        );
        assert!(c
            .host(
                Host::Announcement {
                    speaker: "zz".into(),
                    reply: "y".into(),
                    text: "x".into()
                },
                &mut out,
                &mut a
            )
            .is_err());
    }
}
