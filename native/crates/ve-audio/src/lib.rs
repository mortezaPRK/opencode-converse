//! Full-duplex device I/O at native device rates, with off-callback resampling.
mod io;
mod resample;
pub use io::{AudioIo, Capture, CaptureEvent};
pub use resample::StreamResampler;

#[derive(Debug, Clone)]
pub struct AudioConfig {
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub block_frames: usize,
    pub target_in_rate: u32,
    pub target_out_rate: u32,
}
impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            block_frames: 128,
            target_in_rate: 16_000,
            target_out_rate: 48_000,
        }
    }
}
