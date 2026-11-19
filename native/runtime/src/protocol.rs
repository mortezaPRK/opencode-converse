//! Mirror of the fixed src/protocol.ts contract. Never accept executable/model paths.
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 1_048_576;
pub const MAX_TEXT_BYTES: usize = 262_144;
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
    pub use_gpu: Option<bool>,
    #[serde(default)]
    pub endpointing: Option<EndpointingSetting>,
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
        reply: String,
        text: String,
    },
    ReplyEnd {
        turn: u64,
        reply: String,
        text: String,
    },
    Announcement {
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
                    return Err("Unsupported protocol version; expected 1");
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
            }
            Self::ReplyDelta { turn, reply, text } | Self::ReplyEnd { turn, reply, text } => {
                valid_turn(*turn)?;
                if reply.is_empty() || reply.len() > 256 || reply.chars().any(char::is_control) {
                    return Err("Invalid reply identifier");
                }
                if text.contains('\0') {
                    return Err("Reply text contains NUL");
                }
            }
            Self::TurnEnd { turn, .. } | Self::PromptFailed { turn, .. } => valid_turn(*turn)?,
            Self::Announcement { reply, text } => {
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
    },
    Prompt {
        turn: u64,
        text: String,
    },
    /// `played_ms`/`generated_ms` describe the reply being cut off: audio the output
    /// device actually consumed versus audio generated for it. Absent when no native
    /// playback exists (fixtures) so hosts must treat them as optional.
    Interrupt {
        turn: u64,
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
            r#"{"type":"init","protocol":1,"settings":{"asrThreads":2,"ttsThreads":3,"useGpu":false,"endpointing":"patient"}}"#,
        )
        .unwrap();
        assert!(valid.validate().is_ok());
        let Host::Init { settings, .. } = valid else {
            unreachable!()
        };
        assert_eq!(settings.endpointing, Some(EndpointingSetting::Patient));
        assert!(serde_json::from_str::<Host>(
            r#"{"type":"init","protocol":1,"settings":{"endpointing":"fast"}}"#
        )
        .is_err());
        for value in [
            r#"{"type":"init","protocol":1,"settings":{"asrThreads":0}}"#,
            r#"{"type":"init","protocol":1,"settings":{"ttsThreads":257}}"#,
        ] {
            let message: Host = serde_json::from_str(value).unwrap();
            assert!(message.validate().is_err(), "{value}");
        }
    }

    #[test]
    fn interrupt_progress_is_optional_on_the_wire() {
        let bare = serde_json::to_string(&Engine::Interrupt {
            turn: 1,
            played_ms: None,
            generated_ms: None,
            heard: None,
        })
        .unwrap();
        assert_eq!(bare, r#"{"type":"interrupt","turn":1}"#);
        let full = serde_json::to_string(&Engine::Interrupt {
            turn: 1,
            played_ms: Some(900),
            generated_ms: Some(2400),
            heard: Some("Hi".into()),
        })
        .unwrap();
        assert_eq!(
            full,
            r#"{"type":"interrupt","turn":1,"played_ms":900,"generated_ms":2400,"heard":"Hi"}"#
        );
    }

    #[test]
    fn strict_contract() {
        for s in [
            r#"{"type":"shutdown","extra":1}"#,
            r#"{"type":"init","protocol":1,"settings":{"voice":"x"}}"#,
            r#"{"type":"unknown"}"#,
            r#"{"type":"mute","on":1}"#,
            r#"{"type":"pause","on":1}"#,
        ] {
            assert!(serde_json::from_str::<Host>(s).is_err(), "{s}");
        }
        let init: Host =
            serde_json::from_str(r#"{"type":"init","protocol":2,"settings":{}}"#).unwrap();
        assert!(init.validate().is_err());
    }
}
