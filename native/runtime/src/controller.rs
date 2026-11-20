use crate::chain::{Chain, Limits};
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
    /// Agents may hand the floor to each other. Set once from the init settings.
    pub agent_chat: bool,
    /// Budget for the agent-to-agent chain started by the current user turn. `None` when no
    /// user turn is in flight or the chain was cut (user speech, mute, failure).
    chain: Option<Chain>,
    limits: Limits,
    /// The last complete reply of the current turn: who said it and what, for hand-offs.
    last_final: Option<(usize, String)>,
    idle_sent: bool,
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
            agent_chat: true,
            chain: None,
            limits: Limits::default(),
            last_final: None,
            idle_sent: false,
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
    /// User speech, mute, failure and shutdown all end the agent-to-agent chain. A reply that
    /// merely supersedes another inside one turn does not.
    fn end_chain(&mut self) {
        self.chain = None;
        self.last_final = None;
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
    /// True when nothing is going on that a worker update could disturb: no turn in flight,
    /// nothing speaking, queued or acknowledged, no user speech, and output neither muted nor
    /// paused. Muted and paused count as busy because announcements are dropped or held then,
    /// and an update must wait for the user instead of being consumed silently.
    pub fn is_idle(&self) -> bool {
        self.ready
            && !self.muted
            && !self.paused
            && !self.active
            && self.hearing.is_none()
            && !self.playing
            && !self.announcing
            && self.announcements.is_empty()
            && self.queue.is_empty()
            && self.reply.is_none()
            && self.ack_armed.is_none()
    }
    fn sync_idle(&mut self, out: &mut Vec<Engine>) {
        let idle = self.is_idle();
        if idle != self.idle_sent {
            self.idle_sent = idle;
            out.push(Engine::Idle { idle });
        }
    }
    pub fn ready(&mut self, out: &mut Vec<Engine>, actions: &mut Vec<Action>) {
        self.ready_inner(out, actions);
        self.sync_idle(out);
    }
    pub fn hearing(&mut self, id: u64, out: &mut Vec<Engine>, actions: &mut Vec<Action>) {
        self.hearing_inner(id, out, actions);
        self.sync_idle(out);
    }
    pub fn transcript(
        &mut self,
        id: u64,
        text: String,
        out: &mut Vec<Engine>,
        actions: &mut Vec<Action>,
    ) {
        self.transcript_inner(id, text, out, actions);
        self.sync_idle(out);
    }
    pub fn drained(&mut self, out: &mut Vec<Engine>, actions: &mut Vec<Action>) {
        self.drained_inner(out, actions);
        self.sync_idle(out);
    }
    pub fn host(
        &mut self,
        message: Host,
        out: &mut Vec<Engine>,
        actions: &mut Vec<Action>,
    ) -> Result<(), &'static str> {
        let result = self.host_inner(message, out, actions);
        self.sync_idle(out);
        result
    }
    fn ready_inner(&mut self, out: &mut Vec<Engine>, actions: &mut Vec<Action>) {
        self.ready = true;
        self.rest(out);
        self.schedule_announcement(out, actions);
    }
    fn hearing_inner(&mut self, id: u64, out: &mut Vec<Engine>, actions: &mut Vec<Action>) {
        if self.muted || !self.ready {
            return;
        }
        self.invalidate_announcements();
        self.end_chain();
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
    fn transcript_inner(
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
        self.last_final = None;
        self.chain = (self.agent_chat && self.roster.len() > 1)
            .then(|| Chain::new(Instant::now(), self.limits));
        self.ack_armed = self.ack_enabled.then(Instant::now);
        self.finished.clear();
        self.speech_chunks = 0;
        self.speech_clipped = false;
        out.push(Engine::Prompt {
            turn: self.turn,
            speaker: self.id_of(resolved.index),
            addressed: resolved.addressed,
            text: resolved.text,
            from: None,
        });
        self.status(Status::Thinking, out);
    }
    fn drained_inner(&mut self, out: &mut Vec<Engine>, actions: &mut Vec<Action>) {
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
    fn host_inner(
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
                    self.end_chain();
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
                    self.end_chain();
                    self.cancel(actions);
                    self.active = false;
                    self.hearing = None;
                    self.rest(out);
                    return Ok(());
                }
                if reason != Reason::Completed && turn == self.turn && self.active {
                    self.invalidate_announcements();
                    self.end_chain();
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
                        if self.last_final.is_none() && !reply.raw.trim().is_empty() {
                            self.last_final = Some((reply.speaker, clip(&reply.raw, 8192).into()));
                        }
                        self.finish_reply(reply, out, actions);
                    }
                    actions.push(Action::Finish);
                } else {
                    self.invalidate_announcements();
                    self.end_chain();
                    self.cancel(actions);
                }
                self.active = false;
                if reason != Reason::Completed {
                    self.rest(out);
                } else if !self.playing {
                    let _ = self.promote(out, actions);
                }
                if reason == Reason::Completed {
                    self.relay(out, actions);
                }
            }
            Host::PromptFailed { turn, message } => {
                let _ = message; // Host failure details may contain transcript content. Never log.
                if self.active && turn == self.turn {
                    self.invalidate_announcements();
                    self.end_chain();
                    self.cancel(actions);
                    self.active = false;
                    self.rest(out);
                }
            }
            Host::Shutdown {} => {
                self.invalidate_announcements();
                self.end_chain();
                self.cancel(actions);
            }
            Host::Init { .. } => return Err("init is only valid once"),
        }
        Ok(())
    }
    /// The turn that just completed ended with a reply addressed to another agent: hand the
    /// floor over, if the chain budget allows. The next agent's prompt is a new controller turn,
    /// so floor, queue, interruption and status all work exactly as for a user turn. Announcements
    /// never reach this path because they are not turn replies.
    fn relay(&mut self, out: &mut Vec<Engine>, actions: &mut Vec<Action>) {
        let Some((from, text)) = self.last_final.take() else {
            return;
        };
        if !self.agent_chat || self.chain.is_none() || self.muted || self.hearing.is_some() {
            return;
        }
        let resolved = crate::addressing::resolve(&self.roster, &text);
        // Talking to yourself, to nobody, or to a name that is not on the roster ends the chain.
        if !resolved.addressed || resolved.index == from {
            self.chain = None;
            return;
        }
        let to = resolved.index;
        let verdict = self
            .chain
            .as_mut()
            .map(|chain| chain.admit(Instant::now(), from, to));
        if let Some(Err(reason)) = verdict {
            self.chain = None;
            out.push(Engine::ChainEnd {
                turn: self.turn,
                reason: reason.as_str(),
            });
            return;
        }
        self.turn += 1;
        self.active = true;
        self.addressee = to;
        self.finished.clear();
        self.ack_armed = None;
        if !self.playing {
            // The previous turn already finished its generation; start a fresh one.
            self.push_cancel(actions);
            self.speech_chunks = 0;
            self.speech_clipped = false;
            self.status(Status::Thinking, out);
        }
        out.push(Engine::Prompt {
            turn: self.turn,
            speaker: self.id_of(to),
            addressed: true,
            text: resolved.text,
            from: Some(self.id_of(from)),
        });
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
        if final_ {
            self.last_final = Some((speaker, clip(&text, 8192).to_owned()));
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
        assert!(matches!(&out[..], [Engine::Idle { idle: false }]));
        out.clear();
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
    // ---- agent-to-agent chains ----

    fn finals(c: &mut Controller, turn: u64, speaker: &str, reply: &str, text: &str) {
        let mut out = vec![];
        let mut a = vec![];
        c.host(
            Host::ReplyEnd {
                turn,
                speaker: speaker.into(),
                reply: reply.into(),
                text: text.into(),
            },
            &mut out,
            &mut a,
        )
        .unwrap();
    }
    fn end_turn(c: &mut Controller, turn: u64) -> (Vec<Engine>, Vec<Action>) {
        let mut out = vec![];
        let mut a = vec![];
        c.host(
            Host::TurnEnd {
                turn,
                reason: Reason::Completed,
            },
            &mut out,
            &mut a,
        )
        .unwrap();
        (out, a)
    }
    fn relayed(out: &[Engine]) -> Option<(u64, &str, &str, &str)> {
        out.iter().find_map(|e| match e {
            Engine::Prompt {
                turn,
                speaker,
                text,
                from: Some(from),
                ..
            } => Some((*turn, from.as_str(), speaker.as_str(), text.as_str())),
            _ => None,
        })
    }
    fn chain_end(out: &[Engine]) -> Option<&'static str> {
        out.iter().find_map(|e| match e {
            Engine::ChainEnd { reason, .. } => Some(*reason),
            _ => None,
        })
    }
    /// One agent turn: reply, finish the turn, return what the engine emitted.
    fn agent_turn(c: &mut Controller, turn: u64, speaker: &str, text: &str) -> Vec<Engine> {
        finals(c, turn, speaker, &format!("r{turn}"), text);
        end_turn(c, turn).0
    }

    #[test]
    fn a_reply_addressed_to_another_agent_is_relayed_as_a_new_turn() {
        let mut c = team();
        prompt(&mut c, "Ada, what do you think?");
        let out = agent_turn(&mut c, 1, "a1", "Grace, can you check the build?");
        assert_eq!(
            relayed(&out),
            Some((2, "a1", "a2", "can you check the build?"))
        );
        // The relay is a normal turn: Grace's reply queues behind Ada's audio, then speaks
        // in her own voice once the floor frees.
        finals(&mut c, 2, "a2", "r2", "It is green.");
        let mut out = vec![];
        let mut a = vec![];
        c.drained(&mut out, &mut a);
        assert!(a.contains(&Action::Voice("easton".into())));
        assert!(a.contains(&Action::Speak("It is green.".into())));
    }

    #[test]
    fn trailing_vocatives_relay_too() {
        let mut c = team();
        prompt(&mut c, "Ada, hello");
        let out = agent_turn(&mut c, 1, "a1", "Can you take this one, Linus?");
        assert_eq!(
            relayed(&out),
            Some((2, "a1", "a3", "Can you take this one"))
        );
    }

    #[test]
    fn four_agent_turns_then_the_chain_ends_with_a_reason() {
        let mut c = team();
        prompt(&mut c, "Ada, go");
        assert!(relayed(&agent_turn(&mut c, 1, "a1", "Grace, your move.")).is_some());
        assert!(relayed(&agent_turn(&mut c, 2, "a2", "Linus, your move.")).is_some());
        assert!(relayed(&agent_turn(&mut c, 3, "a3", "Ada, your move.")).is_some());
        let out = agent_turn(&mut c, 4, "a1", "Grace, again.");
        assert!(relayed(&out).is_none());
        assert_eq!(chain_end(&out), Some("turns"));
        assert!(!c.active);
        // A new user turn starts a fresh budget.
        prompt(&mut c, "Ada, go again");
        let turn = c.turn;
        assert!(relayed(&agent_turn(&mut c, turn, "a1", "Grace, your move.")).is_some());
    }

    #[test]
    fn the_wall_clock_cap_ends_the_chain() {
        let mut c = team();
        prompt(&mut c, "Ada, go");
        c.chain.as_mut().unwrap().started =
            Instant::now() - crate::chain::MAX_WALL - Duration::from_secs(1);
        let out = agent_turn(&mut c, 1, "a1", "Grace, your move.");
        assert!(relayed(&out).is_none());
        assert_eq!(chain_end(&out), Some("time"));
    }

    #[test]
    fn ping_pong_between_a_pair_is_limited() {
        let mut c = team();
        c.limits = Limits {
            turns: 99,
            ..Limits::default()
        };
        prompt(&mut c, "Ada, go");
        let mut turn = 1;
        for i in 0..4 {
            let (speaker, text) = if i % 2 == 0 {
                ("a1", "Grace, you.")
            } else {
                ("a2", "Ada, you.")
            };
            let out = agent_turn(&mut c, turn, speaker, text);
            assert!(relayed(&out).is_some(), "hop {i}");
            turn += 1;
        }
        let out = agent_turn(&mut c, turn, "a1", "Grace, you.");
        assert_eq!(chain_end(&out), Some("ping_pong"));
    }

    #[test]
    fn self_address_unknown_names_and_plain_replies_never_relay() {
        for text in [
            "Ada, I will do it myself.",
            "Zed, can you check?",
            "The build is green.",
            "Let me ask Grace to check it.",
        ] {
            let mut c = team();
            prompt(&mut c, "Ada, go");
            let out = agent_turn(&mut c, 1, "a1", text);
            assert!(relayed(&out).is_none(), "{text}");
            assert!(chain_end(&out).is_none(), "{text}");
            assert!(!c.active, "{text}");
        }
    }

    #[test]
    fn a_user_turn_to_a_single_agent_or_with_chat_off_never_relays() {
        let mut c = Controller::default();
        c.ready(&mut vec![], &mut vec![]);
        prompt(&mut c, "hello");
        assert!(relayed(&agent_turn(&mut c, 1, "a1", "Grace, hi.")).is_none());
        let mut c = team();
        c.agent_chat = false;
        prompt(&mut c, "Ada, go");
        let out = agent_turn(&mut c, 1, "a1", "Grace, your move.");
        assert!(relayed(&out).is_none() && chain_end(&out).is_none());
    }

    #[test]
    fn user_speech_cancels_the_active_and_queued_chain_and_stops_it() {
        let mut c = team();
        prompt(&mut c, "Ada, go");
        say(&mut c, "a1", "r1", "Grace, your move. ", true);
        let (out, _) = end_turn(&mut c, 1);
        assert!(relayed(&out).is_some());
        // Grace's reply is queued behind Ada's audio when the user cuts in.
        finals(&mut c, 2, "a2", "r2", "Linus, over to you.");
        let mut out = vec![];
        let mut a = vec![];
        c.hearing(2, &mut out, &mut a);
        assert!(a.contains(&Action::Cancel));
        assert!(out
            .iter()
            .any(|e| matches!(e, Engine::Interrupt { turn: 2, .. })));
        assert!(c.chain.is_none() && c.queue.is_empty());
        // The cancelled turn's late completion neither relays nor speaks.
        let (out, mut a) = end_turn(&mut c, 2);
        assert!(relayed(&out).is_none() && a.is_empty());
        c.drained(&mut vec![], &mut a);
        assert!(!a.iter().any(|x| matches!(x, Action::Speak(_))));
    }

    #[test]
    fn announcements_never_start_a_chain() {
        let mut c = team();
        let mut out = vec![];
        let mut a = vec![];
        c.host(
            Host::Announcement {
                speaker: "a1".into(),
                reply: "w1".into(),
                text: "Grace, the build finished.".into(),
            },
            &mut out,
            &mut a,
        )
        .unwrap();
        c.drained(&mut out, &mut a);
        assert!(relayed(&out).is_none() && !c.active);
    }

    #[test]
    fn a_failed_relay_target_turn_ends_the_chain_quietly() {
        let mut c = team();
        prompt(&mut c, "Ada, go");
        assert!(relayed(&agent_turn(&mut c, 1, "a1", "Grace, your move.")).is_some());
        let mut out = vec![];
        let mut a = vec![];
        c.host(
            Host::PromptFailed {
                turn: 2,
                message: "x".into(),
            },
            &mut out,
            &mut a,
        )
        .unwrap();
        assert!(c.chain.is_none() && !c.active);
        assert!(chain_end(&out).is_none());
    }
}

#[cfg(test)]
mod idle_tests {
    use super::*;
    fn idles(out: &[Engine]) -> Vec<bool> {
        out.iter()
            .filter_map(|e| match e {
                Engine::Idle { idle } => Some(*idle),
                _ => None,
            })
            .collect()
    }
    fn announce(c: &mut Controller, out: &mut Vec<Engine>, a: &mut Vec<Action>) {
        c.host(
            Host::Announcement {
                speaker: "a1".into(),
                reply: "r".into(),
                text: "A worker update.".into(),
            },
            out,
            a,
        )
        .unwrap();
    }
    #[test]
    fn idle_is_announced_once_when_ready_and_only_on_changes() {
        let mut c = Controller::default();
        let (mut out, mut a) = (vec![], vec![]);
        assert!(!c.is_idle());
        c.ready(&mut out, &mut a);
        assert_eq!(idles(&out), [true]);
        out.clear();
        c.hearing(1, &mut out, &mut a);
        assert_eq!(idles(&out), [false]);
        out.clear();
        c.transcript(1, "check the build".into(), &mut out, &mut a);
        assert!(idles(&out).is_empty(), "a committed turn stays busy");
        c.host(
            Host::ReplyEnd {
                turn: 1,
                speaker: "a1".into(),
                reply: "r".into(),
                text: "Sure.".into(),
            },
            &mut out,
            &mut a,
        )
        .unwrap();
        c.host(
            Host::TurnEnd {
                turn: 1,
                reason: Reason::Completed,
            },
            &mut out,
            &mut a,
        )
        .unwrap();
        assert!(idles(&out).is_empty(), "still playing: not idle");
        assert!(!c.is_idle());
        c.drained(&mut out, &mut a);
        assert_eq!(idles(&out), [true]);
        assert!(c.is_idle());
    }
    #[test]
    fn a_playing_announcement_is_busy_until_it_drains() {
        let mut c = Controller::default();
        let (mut out, mut a) = (vec![], vec![]);
        c.ready(&mut out, &mut a);
        out.clear();
        announce(&mut c, &mut out, &mut a);
        assert_eq!(idles(&out), [false]);
        out.clear();
        c.drained(&mut out, &mut a);
        assert_eq!(idles(&out), [true]);
    }
    #[test]
    fn user_speech_makes_the_conversation_busy_and_an_abandoned_one_idle_again() {
        let mut c = Controller::default();
        let (mut out, mut a) = (vec![], vec![]);
        c.ready(&mut out, &mut a);
        c.hearing(1, &mut out, &mut a);
        assert!(!c.is_idle());
        out.clear();
        c.transcript(1, "   ".into(), &mut out, &mut a);
        assert_eq!(idles(&out), [true]);
    }
    #[test]
    fn pause_and_mute_count_as_busy_so_updates_wait() {
        let mut c = Controller::default();
        let (mut out, mut a) = (vec![], vec![]);
        c.ready(&mut out, &mut a);
        out.clear();
        c.host(Host::Pause { on: true }, &mut out, &mut a).unwrap();
        assert_eq!(idles(&out), [false]);
        out.clear();
        c.host(Host::Pause { on: false }, &mut out, &mut a).unwrap();
        assert_eq!(idles(&out), [true]);
        out.clear();
        c.host(Host::Mute { on: true }, &mut out, &mut a).unwrap();
        assert_eq!(idles(&out), [false]);
        out.clear();
        c.host(Host::Mute { on: false }, &mut out, &mut a).unwrap();
        assert_eq!(idles(&out), [true]);
    }
}
