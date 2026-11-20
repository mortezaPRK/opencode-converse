//! Mirror of the fixed src/protocol.ts contract. Never accept executable/model paths.
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 4;
pub const MAX_FRAME_BYTES: usize = 1_048_576;
pub const MAX_TEXT_BYTES: usize = 262_144;
pub const MAX_PARTICIPANTS: usize = 4;
/// Voices the engine may be asked to speak with: the Phonon default plus the shipped catalog.
pub const VOICES: [&str; 5] = ["default", "zoey", "maeve", "easton", "preston"];
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Settings {
    #[serde(default, deserialize_with = "device")]
    pub input_device: Option<String>,
    #[serde(default, deserialize_with = "device")]
    pub output_device: Option<String>,
    #[serde(default)]
    pub asr_threads: Option<u16>,
    #[serde(default)]
    pub tts_threads: Option<u16>,
    #[serde(default)]
    pub endpointing: Option<EndpointingSetting>,
    #[serde(default)]
    pub acknowledgement: Option<AcknowledgementSetting>,
    #[serde(default)]
    pub echo_cancellation: Option<EchoCancellationSetting>,
    /// The fixed roster for this conversation. Absent means one agent speaking the default voice.
    #[serde(default)]
    pub participants: Option<Vec<Participant>>,
    /// Whether an agent's reply that addresses another agent is relayed to that agent.
    #[serde(default)]
    pub agent_chat: Option<AgentChatSetting>,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentChatSetting {
    On,
    Off,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Participant {
    pub id: String,
    pub name: String,
    pub voice: String,
}

impl Participant {
    /// The single implicit agent used when the host sends no roster.
    pub fn implicit() -> Self {
        Self {
            id: "a1".into(),
            name: "Converse".into(),
            voice: "default".into(),
        }
    }
}

fn valid_participants(list: &[Participant]) -> bool {
    if list.is_empty() || list.len() > MAX_PARTICIPANTS {
        return false;
    }
    let mut ids = std::collections::HashSet::new();
    let mut names = std::collections::HashSet::new();
    list.iter().all(|p| {
        !p.id.is_empty()
            && p.id.len() <= 16
            && p.id.bytes().all(|b| b.is_ascii_alphanumeric())
            && (2..=16).contains(&p.name.len())
            && p.name.bytes().all(|b| b.is_ascii_alphabetic())
            && VOICES.contains(&p.voice.as_str())
            && ids.insert(p.id.clone())
            && names.insert(p.name.to_ascii_lowercase())
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EchoCancellationSetting {
    On,
    Off,
}

impl Settings {
    /// Agents may talk to each other unless the host explicitly turned it off.
    pub fn agent_chat_enabled(&self) -> bool {
        self.agent_chat != Some(AgentChatSetting::Off)
    }
    /// Echo cancellation is on unless the host explicitly turned it off.
    pub fn echo_cancellation_enabled(&self) -> bool {
        self.echo_cancellation != Some(EchoCancellationSetting::Off)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcknowledgementSetting {
    Auto,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointingSetting {
    Responsive,
    Patient,
}

fn device<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    String::deserialize(d).map(Some)
}

fn valid_threads(value: Option<u16>) -> bool {
    value.is_none_or(|threads| (1..=256).contains(&threads))
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Host {
    Init {
        protocol: u32,
        settings: Box<Settings>,
    },
    ReplyDelta {
        turn: u64,
        speaker: String,
        reply: String,
        text: String,
    },
    ReplyEnd {
        turn: u64,
        speaker: String,
        reply: String,
        text: String,
    },
    Announcement {
        speaker: String,
        reply: String,
        text: String,
    },
    TurnEnd {
        turn: u64,
        reason: Reason,
    },
    PromptFailed {
        turn: u64,
        message: String,
    },
    Mute {
        on: bool,
    },
    Pause {
        on: bool,
    },
    Shutdown {},
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Completed,
    Interrupted,
    Disconnected,
    Failed,
}

impl Host {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Init { protocol, settings } => {
                if *protocol != VERSION {
                    return Err("Unsupported protocol version; expected 4");
                }
                for device in [&settings.input_device, &settings.output_device]
                    .into_iter()
                    .flatten()
                {
                    if device.is_empty()
                        || device.len() > 1024
                        || device.chars().any(char::is_control)
                    {
                        return Err("Invalid device name");
                    }
                }
                if !valid_threads(settings.asr_threads) || !valid_threads(settings.tts_threads) {
                    return Err("Invalid speech thread count");
                }
                if settings
                    .participants
                    .as_deref()
                    .is_some_and(|list| !valid_participants(list))
                {
                    return Err("Invalid participants");
                }
            }
            Self::ReplyDelta {
                turn,
                speaker,
                reply,
                text,
            }
            | Self::ReplyEnd {
                turn,
                speaker,
                reply,
                text,
            } => {
                valid_turn(*turn)?;
                valid_speaker(speaker)?;
                if reply.is_empty() || reply.len() > 256 || reply.chars().any(char::is_control) {
                    return Err("Invalid reply identifier");
                }
                if text.contains('\0') {
                    return Err("Reply text contains NUL");
                }
            }
            Self::TurnEnd { turn, .. } | Self::PromptFailed { turn, .. } => valid_turn(*turn)?,
            Self::Announcement {
                speaker,
                reply,
                text,
            } => {
                valid_speaker(speaker)?;
                if reply.is_empty() || reply.len() > 256 || reply.chars().any(char::is_control) {
                    return Err("Invalid announcement identifier");
                }
                if text.contains('\0') {
                    return Err("Announcement contains NUL");
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn valid_speaker(speaker: &str) -> Result<(), &'static str> {
    if speaker.is_empty()
        || speaker.len() > 16
        || !speaker.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        Err("Invalid speaker identifier")
    } else {
        Ok(())
    }
}

fn valid_turn(turn: u64) -> Result<(), &'static str> {
    if turn == 0 || turn > MAX_SAFE_INTEGER {
        Err("Invalid turn identifier")
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Loading,
    Listening,
    Hearing,
    Thinking,
    Speaking,
    Muted,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Engine {
    Hello {
        protocol: u32,
    },
    Status {
        state: Status,
        /// Who the state is about: the addressee while thinking, the voice while speaking.
        #[serde(skip_serializing_if = "Option::is_none")]
        speaker: Option<String>,
    },
    /// The conversation became idle (`true`) or busy (`false`). Sent on every change, never
    /// repeated. Idle means nothing is running, speaking, queued or heard.
    Idle {
        idle: bool,
    },
    /// `speaker` is the participant the transcript was addressed to (the first agent when the
    /// user named nobody); `text` has the spoken name removed.
    Prompt {
        turn: u64,
        speaker: String,
        addressed: bool,
        text: String,
        /// Set when another agent's reply addressed this participant: the relaying speaker.
        /// Absent for prompts that come from the user.
        #[serde(skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },
    /// An agent-to-agent hand-off was refused by the chain budget. `reason` is `turns`,
    /// `time` or `ping_pong`. Sent once per refused hand-off; never for a natural end.
    ChainEnd {
        turn: u64,
        reason: &'static str,
    },
    /// `played_ms`/`generated_ms` describe the reply being cut off: audio the output
    /// device actually consumed versus audio generated for it. Absent when no native
    /// playback exists (fixtures) so hosts must treat them as optional.
    Interrupt {
        turn: u64,
        /// The participant that held the floor or was addressed when the user cut in.
        #[serde(skip_serializing_if = "Option::is_none")]
        speaker: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        played_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        generated_ms: Option<u64>,
        /// Spoken text the user heard before the cut (bounded). For transcripts only;
        /// never agent context. Absent when unknown.
        #[serde(skip_serializing_if = "Option::is_none")]
        heard: Option<String>,
    },
    Error {
        code: &'static str,
        message: String,
        fatal: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn device_names_are_bounded_by_utf8_bytes_not_host_utf16_units() {
        for (name, accepted) in [
            ("é".repeat(512), true),
            (format!("{}a", "€".repeat(341)), true),
            ("é".repeat(600), false),
            ("€".repeat(600), false),
        ] {
            assert!(name.encode_utf16().count() <= 1024);
            assert_eq!(name.len() <= 1024, accepted);
            for input in [true, false] {
                let mut settings = Settings::default();
                if input {
                    settings.input_device = Some(name.clone());
                } else {
                    settings.output_device = Some(name.clone());
                }
                assert_eq!(
                    Host::Init {
                        protocol: VERSION,
                        settings: Box::new(settings)
                    }
                    .validate()
                    .is_ok(),
                    accepted
                );
            }
        }
    }
    #[test]
    fn configured_inference_limits_are_bounded() {
        let valid: Host = serde_json::from_str(
            r#"{"type":"init","protocol":4,"settings":{"asrThreads":2,"ttsThreads":3,"endpointing":"patient","acknowledgement":"off","echoCancellation":"off"}}"#,
        )
        .unwrap();
        assert!(valid.validate().is_ok());
        let Host::Init { settings, .. } = valid else {
            unreachable!()
        };
        assert_eq!(settings.endpointing, Some(EndpointingSetting::Patient));
        assert_eq!(settings.acknowledgement, Some(AcknowledgementSetting::Off));
        assert_eq!(
            settings.echo_cancellation,
            Some(EchoCancellationSetting::Off)
        );
        assert!(!settings.echo_cancellation_enabled());
        assert!(Settings::default().echo_cancellation_enabled());
        for bad in ["\"loud\"", "1", "true"] {
            let value =
                format!(r#"{{"type":"init","protocol":4,"settings":{{"acknowledgement":{bad}}}}}"#);
            assert!(serde_json::from_str::<Host>(&value).is_err(), "{value}");
        }
        for bad in ["\"auto\"", "1", "true"] {
            let value = format!(
                r#"{{"type":"init","protocol":4,"settings":{{"echoCancellation":{bad}}}}}"#
            );
            assert!(serde_json::from_str::<Host>(&value).is_err(), "{value}");
        }
        assert!(serde_json::from_str::<Host>(
            r#"{"type":"init","protocol":4,"settings":{"endpointing":"fast"}}"#
        )
        .is_err());
        for value in [
            r#"{"type":"init","protocol":4,"settings":{"asrThreads":0}}"#,
            r#"{"type":"init","protocol":4,"settings":{"ttsThreads":257}}"#,
        ] {
            let message: Host = serde_json::from_str(value).unwrap();
            assert!(message.validate().is_err(), "{value}");
        }
    }

    #[test]
    fn interrupt_progress_is_optional_on_the_wire() {
        let bare = serde_json::to_string(&Engine::Interrupt {
            turn: 1,
            speaker: None,
            played_ms: None,
            generated_ms: None,
            heard: None,
        })
        .unwrap();
        assert_eq!(bare, r#"{"type":"interrupt","turn":1}"#);
        let full = serde_json::to_string(&Engine::Interrupt {
            turn: 1,
            speaker: Some("a2".into()),
            played_ms: Some(900),
            generated_ms: Some(2400),
            heard: Some("Hi".into()),
        })
        .unwrap();
        assert_eq!(
            full,
            r#"{"type":"interrupt","turn":1,"speaker":"a2","played_ms":900,"generated_ms":2400,"heard":"Hi"}"#
        );
    }

    #[test]
    fn strict_contract() {
        for s in [
            r#"{"type":"shutdown","extra":1}"#,
            r#"{"type":"init","protocol":4,"settings":{"voice":"x"}}"#,
            r#"{"type":"unknown"}"#,
            r#"{"type":"mute","on":1}"#,
            r#"{"type":"pause","on":1}"#,
        ] {
            assert!(serde_json::from_str::<Host>(s).is_err(), "{s}");
        }
        let init: Host =
            serde_json::from_str(r#"{"type":"init","protocol":1,"settings":{}}"#).unwrap();
        assert!(init.validate().is_err());
    }

    #[test]
    fn participants_are_validated_and_speakers_are_required() {
        let init = |participants: &str| -> Result<(), &'static str> {
            let message: Host = serde_json::from_str(&format!(
                r#"{{"type":"init","protocol":4,"settings":{{"participants":{participants}}}}}"#
            ))
            .map_err(|_| "parse")?;
            message.validate()
        };
        let p = |id: &str, name: &str, voice: &str| {
            format!(r#"{{"id":"{id}","name":"{name}","voice":"{voice}"}}"#)
        };
        assert!(init(&format!(
            "[{},{}]",
            p("a1", "Ada", "zoey"),
            p("a2", "Grace", "easton")
        ))
        .is_ok());
        for bad in [
            "[]".to_string(),
            format!("[{}]", p("a1", "Ada", "unknown")),
            format!("[{}]", p("a1", "Ada1", "zoey")),
            format!("[{}]", p("a1", "A", "zoey")),
            format!(
                "[{},{}]",
                p("a1", "Ada", "zoey"),
                p("a1", "Grace", "easton")
            ),
            format!("[{},{}]", p("a1", "Ada", "zoey"), p("a2", "ada", "easton")),
            format!(
                "[{}]",
                ["a1", "a2", "a3", "a4", "a5"]
                    .iter()
                    .map(|id| p(
                        id,
                        &format!("Name{}", id.replace(char::is_numeric, "")),
                        "zoey"
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        ] {
            assert!(init(&bad).is_err(), "{bad}");
        }
        assert!(
            serde_json::from_str::<Host>(r#"{"type":"announcement","reply":"x","text":"y"}"#)
                .is_err()
        );
        let ok: Host = serde_json::from_str(
            r#"{"type":"announcement","speaker":"a1","reply":"x","text":"y"}"#,
        )
        .unwrap();
        assert!(ok.validate().is_ok());
        let bad: Host = serde_json::from_str(
            r#"{"type":"announcement","speaker":"a 1","reply":"x","text":"y"}"#,
        )
        .unwrap();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn agent_chat_is_on_unless_turned_off_and_rejects_other_values() {
        let parse = |settings: &str| {
            serde_json::from_str::<Host>(&format!(
                r#"{{"type":"init","protocol":4,"settings":{settings}}}"#
            ))
        };
        let on = |settings: &str| match parse(settings).unwrap() {
            Host::Init { settings, .. } => settings.agent_chat_enabled(),
            _ => unreachable!(),
        };
        assert!(on("{}") && on(r#"{"agentChat":"on"}"#));
        assert!(!on(r#"{"agentChat":"off"}"#));
        assert!(parse(r#"{"agentChat":"maybe"}"#).is_err());
    }
}
