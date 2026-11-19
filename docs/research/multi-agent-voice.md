# Multiple named voice agents in one conversation

Status: design only. Nothing here is implemented.

## 1. Current architecture (short)

- One Rust subprocess per conversation (mic, VAD, turn detection, ASR, TTS, playback). Messages are JSON lines over stdio: `src/protocol.ts` mirrors `native/runtime/src/protocol.rs`. Engine to host: `hello`, `status`, `prompt{turn,text}`, `interrupt{turn,played_ms,heard}`, `error`. Host to engine: `init`, `reply_delta/reply_end{turn,reply,text}`, `announcement{reply,text}`, `turn_end`, `prompt_failed`, `mute`, `pause`, `shutdown`.
- `native/runtime/src/controller.rs` is the single-owner state machine. It holds turn numbers, replies, generations (cancel-safe), announcements and the acknowledgement timer (`poll_ack`, `ack_enabled`).
- `native/crates/ve-tts/src/engine.rs` loads Phonon 31M through `ptts =0.7.1`. It registers voices with `checkpoint.register_voices` and requires a voice named `default`. It calls `synth.stream(text)` with no voice choice. `assets/models.json` pins `default-voice.safetensors` from `gradium/phonon-31m-en`.
- `src/supervisor.ts` runs one voice supervisor OpenCode session (`ctx.session.create`, `ctx.session.prompt`). Its tools are `start_session`, `send_message`, `list_sessions`, `list_agents`, `attach_session` and `detach_session`. Follow state is kept in plugin storage and never mutates the followed session. Announcements are produced for followed sessions.
- `src/client/bridge.ts` holds the snapshot and transcript, and forwards `reply_*` and `announcement` to Rust. `src/gui.tsx` and `src/tui.tsx` render the transcript panel and popover.
- Assumptions to re-check against the code before building:
  - `node_modules/@opencode-ai` is not installed in this checkout, so the SDK types could not be read directly. The SDK evidence below is the calls `supervisor.ts` already makes: `ctx.session.create`, `ctx.session.prompt` and the worker `create` at about lines 525, 885, 917 and 954.
  - Phase 0 should confirm that `session.prompt` can pass `agent`, `model` and `system` per call, and that `session.messages` can read a followed session.

## 2. Proposed design

### 2.1 Participant model

Recommendation: **configured agents (static), with a bounded set of at most 4**. Runtime creation by voice is phase 3.

New option `agents`:

```jsonc
"agents": [
  { "name": "Ada",   "voice": "Zoey",   "agent": "build",  "model": "provider/model", "prompt": "You are Ada, ..." },
  { "name": "Grace", "voice": "Easton" }
]
```

- `name`: unique, case-insensitive, single spoken token (letters only), 2 to 16 characters. It must not collide with common words or other names (validated by the schema and at startup).
- `voice`: a catalog voice name from §2.5. Optional. If omitted, voices are assigned in order from the shipped set.
- `agent`, `model` and `prompt` are optional. They are passed per prompt to that agent's own supervisor session.
- If `agents` is absent, behaviour is exactly today's single agent. The single agent gets an implicit name (e.g. "Assistant") and the `default` voice.
- Each participant owns one supervisor session. It is created lazily and persisted per `(server, location, name)`, like the current supervisor. Agents never own the sessions they follow or start. They only read them and prompt them (via the existing tools) under the lock in §2.6.
- Each participant's supervisor session gets a system prompt block listing the roster: names, one-line roles, and the rules in §2.2 to §2.4. The existing supervisor tools are unchanged, except for tool additions in §2.3.
- The participant set is fixed at conversation start and bound to it, like the server and location. Changing the roster means restarting the conversation.

### 2.2 Addressing and floor control

Rust owns the floor (it already owns the speech queue and generations).

**User to agent.** The host resolves the addressee, not the LLM. Rust sends `prompt` with the raw ASR text, and TS resolves it:

1. Leading vocative: `^(hey |ok |okay )?<name>[,:]? ` (case-insensitive, whole word). Strip the name before prompting.
2. A trailing vocative ("..., Ada") is also accepted.
3. No name resolves to the **default addressee**: the agent that spoke last to the user. At conversation start it is the first configured agent.
4. A mid-sentence name is not treated as addressing (avoids false positives such as "ask Grace to ...").
   - If the user says "ask Grace to ...", Ada receives it and can use `ask_agent` (§2.3).
5. ASR misrecognition: match with a fuzzy set per name, built from edit distance ≤1 on the normalized token plus phonetic key (e.g. metaphone). Only the first two tokens of the utterance are checked. Ambiguity falls back to the default addressee. The transcript shows who actually answered, so the user can correct.

**Floor.** A new Rust floor with one speaker at a time:

- The floor has a FIFO queue of `(speaker, reply)`. The active reply streams as today. Other agents' deltas are buffered text only (no synthesis until they hold the floor). Each queued item has a bound (chars, and a TTL of 60 s). Overflow drops the oldest agent-to-agent item and keeps user-addressed items.
- Priority: replies to a user turn beat agent-to-agent turns beat announcements. Announcements are a one-shot, short, per-agent item and are dropped (not queued past the cap) when the user is speaking.
- A voice change between queue items inserts a short silence (~250 ms) instead of an inter-chunk crossfade, so speakers do not blur. Reuse the existing join logic only within one speaker.

**Barge-in.** The user speaking cancels the active reply and **every queued agent-to-agent item and its generation**. The `interrupt` message carries the `speaker` that was interrupted, with `played_ms`/`heard` as today. The interrupted agent gets the existing "interrupted, heard X" context. Queued, never-spoken replies are not injected into other agents. Agent-to-agent chains are aborted (§2.3). The user's next utterance is a new turn with normal addressing.

**Acknowledgement.** Today's cue ("mm-hm" style, `poll_ack`) is spoken in the addressee's voice only, once per user turn, and only when the addressee has not begun speaking. Not played for agent-to-agent turns.

### 2.3 Agent-to-agent conversation

Transport (all TS-side, no new server path):

- Agent A gets a tool `say_to(name, text)` (plus `say_to_user`, which is implicit for ordinary replies). The tool does not speak. A's spoken reply is the speech. The tool's call enqueues a turn for B.
- Simpler and recommended: **an agent's spoken reply that begins with a vocative to another agent is routed to that agent.** The host applies the §2.2 vocative parse to every agent reply. If it addresses another agent, the reply is spoken (in A's voice) and delivered to B with `session.prompt` as `[Ada → Grace] text`. This needs no tool and matches how humans talk, and the user hears both sides.
- B's turn is a normal turn in B's own supervisor session, with speaker attribution in the prompt text. The reply is spoken in B's voice once B holds the floor.

Bounds (all enforced by the host/Rust, never trusted to the LLM):

- `maxAgentTurns`: a budget of consecutive agent-to-agent turns per user turn. Default 4. It resets on any user speech. When the budget is spent, the chain stops and control returns to the user, and the last agent gets a one-line system note ("hand back to the user").
- Per user turn, a hard cap on total agent turns (all agents), and a token/time cap: wall clock 120 s per chain.
- No self-address and no immediate ping-pong A→B→A→B beyond 2 round trips without a user turn.
- A reply that does not address anyone, or addresses the user, ends the chain.
- The user can always interrupt by voice (barge-in cancels everything, §2.2). Mute and pause stop the chain too.
- Tool-using agent turns in a chain can run long. Each chain turn uses the same per-prompt abort as the user flow.

Cost safeguards:

- Agent-to-agent turns use a per-agent `model` override. Recommend a cheap model for chained turns (optional `chainModel`).
- A chain-turn counter is shown in the UI and logged. A runaway chain is impossible because the budget lives in the host.
- Agent-to-agent turns are off unless `agents.length > 1`. Option `agentToAgent: "off" | "on"` (default `"on"` when there are 2+ agents; use `"off"` if cost matters).

### 2.4 Shared transcript and what each agent sees

- Transcript entries gain `speaker: { kind: "user" } | { kind: "agent", id, name }`, plus `to?: name` for addressed messages. The bridge appends them from `reply_*` (which now carry `speaker`) and from host-routed agent-to-agent deliveries.
- **What each agent sees (recommended):** its own session history, plus _messages addressed to it_, plus a **rolling bounded digest of the other participants' turns**: the last N (default 6) transcript lines, each truncated, prefixed to each prompt as `[Conversation so far, others only]`. Not the whole conversation.
  - The whole conversation in every session multiplies cost and context by the number of agents and leaks long worker output across agents.
  - Only-addressed messages make agents unaware of context the user assumes is shared ("what did Grace just say?").
  - The digest is host-built, bounded, and per-turn. It is not persisted into the other sessions' history beyond the prompt that carries it.
- Unaddressed user speech goes to the default addressee only. Others see it in the digest.
- GUI (`src/gui.tsx`): transcript rows get a name label and a stable color from a fixed palette (by roster index). The popover and status line show the current speaker, e.g. "Ada speaking" or "Thinking: Grace". Queued speakers are shown as small chips. TUI (`src/tui.tsx`): the same, name prefixes with ANSI/theme colors. Agent-to-agent lines show "Ada → Grace".
- The snapshot gains `participants: [{id,name,state}]`, `floor: {speaker?, queued: id[]}`, and `chainTurns`.

### 2.5 Voices

Verified on HF (`gradium/phonon-voices`, API `modelInfo`, README, tree):

- License: **apache-2.0** (card metadata `license:apache-2.0`). 102 voices (English 37: US/UK/characters; FR 15; DE 13; ES 25; PT 12). The model `gradium/phonon-31m-en` is also apache-2.0.
- Format: each voice is a `voices/<voice_id>.safetensors` with a single tensor `speaker_wavs`, 1x512x125, float32 (format `ba7a5a45@300`), about 256 KB each. All 102 total about 26 MB. Metadata is in `voices.jsonl` (name, voice_id, language, locale, gender, age, description). The tree API gives LFS sha256 and size per file, which makes pinning easy.
- Caveat: the card says the voices are "for Phonon S (31M)". Verify at implementation time that the embeddings work with `phonon-31m-en` (English model, `Lang::En`). Only use the English voices (en-us, en-gb, characters). Also verify that the voices in the catalog are free of additional per-voice terms; the card says nothing beyond apache-2.0, so do the exact-artifact license review AGENTS.md requires before redistributing (as with the model).
- Pinning: add each shipped voice to `assets/models.json` as an asset with `url` at a pinned HF commit (`resolve/<sha>/voices/<id>.safetensors`, repo sha seen: `0ee17205f254b5abc43b266c8a0f215565c352f7`, to be re-confirmed at implementation), `sha256` (LFS oid), `bytes`, and a runtime path under the Phonon dir. Download follows the existing model installer (`native/runtime/src/models.rs`). Do not add the entire catalog. Ship a small set so installs stay light.
- Default set to ship (4 distinct, English, 2F/2M), final choice by listening to the samples: Zoey (`NbpkqMVS3CJeq2j8`, F, US), Easton (`LMDIYE0Oa377Qh6Z`, M, US), Maeve (`6PWnV0Nq4wu7RVBT`, F, UK), Micah (`40QrMVgAxFiUnP9f`, M, US). Keep `default-voice` for the single-agent case. Other catalog voices could be an opt-in `models install --voice <name>` later, not an option path (AGENTS.md: locations are fixed).
- Selection per utterance in ptts 0.7.1: `SpeechOptions::voice(name)` selects a registered voice per request (`ptts-0.7.1/src/synth.rs` ~L246). Voices are registered with `Checkpoint::register_voices` or `Synth::add_voice_file(name, path)` (`checkpoint.rs` L141). `SynthBuilder::voice` sets only the default. `ve-tts/src/engine.rs` currently calls `synth.stream(text)` with no options, so it needs a variant taking `SpeechOptions` with the voice.
- Cost of switching: embeddings are small (256 KB) and load once at startup via `add_voice_file` (the per-voice cost is not in the docs, so measure it). The per-request cost is likely a voice prompt prefill of 125 frames at the start of each stream. Measure time-to-first-audio when the voice changes versus when it is the same. If it is significant, keep the next speaker's voice warm by running its first sentence while the previous plays (single TTS thread today: do not parallelise without measuring CPU load).
- Voice name to path resolution happens in Rust from the fixed state folder. The host sends only voice _names_ from the validated catalog, never paths.

### 2.6 Followed worker sessions

- Following stays per agent, in plugin storage keyed `(conversation, agent id, session id)`. Following never mutates the session.
- **Exclusive prompter lock**: a conversation-scoped map `sessionId -> agentId` in the host. `send_message(session, ...)` from agent X succeeds only if the session is unlocked or locked by X. The lock is taken at first prompt, and released on detach, when the session goes idle with no pending reply for N minutes, or when the conversation ends. If another agent holds it, the tool returns an error naming the holder ("Grace is already driving that session; ask her via a spoken request").
- Two agents may _follow_ (listen to) the same session. Only the lock holder may prompt. Announcements for a followed session are spoken once, attributed to the lock holder if any, else the first follower, in that agent's voice ("Ada: the build finished"). The announcement carries `speaker`.
- Locks are in-memory per conversation, and reset on restart. The OpenCode server does not enforce them, so a user or another client can still prompt.
- Permissions: nothing here approves permissions. The supervisor tools keep their no-approve rule (AGENTS.md). Permission requests on worker sessions are surfaced for the user to approve in OpenCode. A permission announcement may be spoken (attributed to the following agent) but never answered by any agent.

### 2.7 Protocol, Rust, TS, GUI/TUI, schema changes

Protocol (bump `PROTOCOL_VERSION` to 2, no aliases, both ends exact):

- `init.settings` gains `participants: [{ id, name, voice }]` (ids `a1..a4`, ASCII). Names are used only for ack/text shaping. Rust validates voices against registered ones and fails with `error{fatal:true}` on unknown.
- Engine to host `prompt`: unchanged (raw text). Addressing is host-side, since agents map to sessions. Optional later: `name_hint` from ASR.
- Host to engine: `reply_delta/reply_end/announcement` gain `speaker: id`. `reply` ids remain unique per conversation. Add `floor_policy` on `init`: `{ queue_max, ttl_ms, priority }`. Add `turn_end.speaker`.
- Engine to host: `interrupt` gains `speaker` and `dropped: [reply ids]` (queued items cancelled). `status` gains optional `speaker`. New `floor{speaker?, queued:[id]}` event for UI.
- Bounded: frames stay under `MAX_FRAME_BYTES`. Per-speaker text buffers are capped (as the current reply buffers).

Rust (`controller.rs`, `speech.rs`, `ve-tts`):

- Replies are keyed by `(speaker, reply)`. The floor queue, priority and TTL live in the controller (single owner). Synthesis jobs carry `voice`. The generation counter stays global, so barge-in cancels all.
- `ack` is tied to the addressee. `ve-tts` accepts `SpeechOptions.voice`, with cached ack phrases per voice (the ack cache is a few phrases per voice, synthesized at startup lazily).

TS:

- `supervisor.ts`: becomes one `ParticipantSupervisor` per agent (session, tools, prompt prefix) under a `Conversation` coordinator that owns the roster, the addressing parse, chain budget, lock map and digest builder. Existing per-supervisor logic stays; global state moves to the coordinator.
- `bridge.ts`: `speaker` on transcript entries, snapshot additions (§2.4), routing of `prompt` to an addressee, routing of agent replies to other agents, aborting chains on `interrupt`.
- Option schema (`docs/options.schema.json`, `docs/configuration.md`): add `agents[]` (maxItems 4, `name` pattern `^[A-Za-z]{2,16}$`, unique, `voice` enum of the shipped catalog names, optional `agent`, `model`, `prompt` with maxLength), `agentToAgent`, `maxAgentTurns` (1 to 8, default 4). Do not add engine or model paths (AGENTS.md).

## 3. Phased plan

| Phase                     | Scope                                                                                                                                                                                                                                                                                                               | Effort |
| ------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------ |
| 0                         | Spike: confirm `session.prompt` per-call agent/model/system in the SDK, measure Phonon voice-switch latency, verify the voice embeddings on `phonon-31m-en`, listen and pick 4 voices, license review                                                                                                               | S      |
| 1 (smallest useful slice) | Two to four configured agents with names and voices. Pin 4 voices in `models.json`. Protocol v2 with `speaker`. Vocative addressing + default addressee. One supervisor session each, reply spoken in own voice, single-speaker floor (no agent-to-agent). Speaker labels and colors in the transcript. Schema/docs | L      |
| 2                         | Agent-to-agent routing by vocative, chain budget, queue/priority/TTL, barge-in dropping the queue, rolling digest, exclusive prompter lock, announcement attribution, floor chips in the UI, cost caps                                                                                                              | L      |
| 3                         | Runtime-created agents by voice ("add an agent called Linus"), extra opt-in voices (`models install --voice`), per-agent ack tuning, UI polish (themes, per-agent mute), fuzzy name matching tuning                                                                                                                 | M      |

## 4. Open questions (with recommended defaults)

1. Agent-to-agent talk on by default when 2+ agents are configured? Recommend: **yes, bounded** (`maxAgentTurns: 4`, cheap `chainModel` optional).
2. Who answers when the user names nobody? Recommend: **the agent who spoke last** (first configured at start).
3. Should agents see the whole conversation? Recommend: **no; addressed messages plus a bounded 6-line digest of others**.
4. Roster fixed at conversation start (restart to change), or live edits? Recommend: **fixed**; runtime creation only in phase 3.
5. Which 4 default voices (Zoey, Easton, Maeve, Micah proposed) and ship all 4 by default (~1 MB), or only on demand? Recommend: **ship the 4 (~1 MB)**.
6. When a followed session is held by another agent, should agents be told to ask that agent, or may the user reassign the lock by voice ("Grace, take over the build session")? Recommend: **user can reassign by voice, agents cannot steal**.

## 5. Risks

- **Cost runaway**: agent loops multiply LLM calls. Mitigation: host-enforced budgets, wall-clock cap, cheap chain model, `agentToAgent: "off"`, UI counter.
- **Crosstalk and overlap**: the single floor and priorities prevent overlapping speech, but long queues feel laggy. TTL and per-item caps drop stale items. The user hears at most one voice at a time.
- **ASR misrecognizing names**: unusual names fail. Mitigation: restrict name rules, fuzzy and phonetic matching, default addressee fallback, visible attribution so the user can correct. Consider suggesting common-word-free names in docs.
- **Latency**: more agents means more sessions to warm and possible voice switch cost in TTS (unmeasured). Mitigation: lazy sessions, measure in phase 0, cached acks per voice.
- **Context bloat**: the digest and roster prompt add tokens per turn. Bounded by design.
- **Phonon voice licensing**: the card says apache-2.0, but the per-artifact review is required before redistributing (AGENTS.md). Voices are human-likeness embeddings, so also review any attribution or name/likeness terms in the dataset. Keep third-party attribution in the license notices. Pin by sha256 so a repo change cannot alter shipped bytes.
- **Lock gaps**: locks are advisory (server does not enforce), so another client can still prompt the same session.
