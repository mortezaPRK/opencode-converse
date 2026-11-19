//! Phonon synthesis and inter-chunk audio joins. Text shaping/chunking lives only
//! in the runtime controller; this crate does not maintain a second text queue.
mod engine;
mod joiner;
pub use engine::{Finish, Tts, TtsConfig};
pub use joiner::Joiner;
