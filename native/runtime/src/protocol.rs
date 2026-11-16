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
}

fn device<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    String::deserialize(d).map(Some)
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Host {
    Init {
        protocol: u32,
        settings: Settings,
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
    Interrupt {
        turn: u64,
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
                        settings
                    }
                    .validate()
                    .is_ok(),
                    accepted
                );
            }
        }
    }
    #[test]
    fn strict_contract() {
        for s in [
            r#"{"type":"shutdown","extra":1}"#,
            r#"{"type":"init","protocol":1,"settings":{"voice":"x"}}"#,
            r#"{"type":"unknown"}"#,
            r#"{"type":"mute","on":1}"#,
        ] {
            assert!(serde_json::from_str::<Host>(s).is_err(), "{s}");
        }
        let init: Host =
            serde_json::from_str(r#"{"type":"init","protocol":2,"settings":{}}"#).unwrap();
        assert!(init.validate().is_err());
    }
}
