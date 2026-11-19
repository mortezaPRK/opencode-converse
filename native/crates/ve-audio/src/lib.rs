//! Full-duplex device I/O at native device rates, with off-callback resampling.
mod echo;
mod io;
mod resample;
pub use echo::{EchoCanceller, EchoReference, AEC_FRAME, AEC_RATE};
pub use io::{AudioIo, Capture, CaptureEvent, PlaybackProgress};
pub use resample::StreamResampler;

#[derive(Debug, Clone)]
pub struct AudioConfig {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub block_frames: usize,
    pub target_in_rate: u32,
    pub target_out_rate: u32,
    /// Tap the output callback so capture can cancel the engine's own playback.
    pub echo_cancellation: bool,
}
impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            block_frames: 128,
            target_in_rate: 16_000,
            target_out_rate: 48_000,
            echo_cancellation: true,
        }
    }
}
