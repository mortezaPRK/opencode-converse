//! Piper/VITS via sherpa-onnx's cancellable C API.
use anyhow::{ensure, Context, Result};
use sherpa_rs::sherpa_rs_sys as sys;
use std::ffi::{c_void, CString};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub struct TtsConfig {
    pub model: PathBuf,
    pub tokens: PathBuf,
    pub data_dir: PathBuf,
    pub num_threads: i32,
}

pub struct Tts {
    ptr: *const sys::SherpaOnnxOfflineTts,
    sample_rate: u32,
}

// SAFETY: this wrapper owns the handle and is moved to one synthesis worker.
// It is not Sync, and no other thread calls into the handle.
unsafe impl Send for Tts {}

fn cpath(path: &Path) -> Result<CString> {
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes()
    };
    #[cfg(not(unix))]
    let bytes = path
        .to_str()
        .context("Piper native loader paths must be representable as UTF-8 on this platform")?
        .as_bytes();
    CString::new(bytes).context("Piper native loader paths cannot contain NUL bytes")
}

impl Tts {
    pub fn new(config: TtsConfig) -> Result<Self> {
        ensure!(
            config.model.is_file(),
            "Piper model not found at {}; set \"ttsModel\" or run opencode-converse doctor",
            config.model.display()
        );
        ensure!(
            config.tokens.is_file(),
            "Piper tokens not found at {}; set \"ttsTokens\" or run opencode-converse doctor",
            config.tokens.display()
        );
        ensure!(
            config.data_dir.is_dir(),
            "Piper data directory not found at {}; set \"ttsDataDir\" or run opencode-converse doctor",
            config.data_dir.display()
        );
        let model = cpath(&config.model)?;
        let tokens = cpath(&config.tokens)?;
        let data = cpath(&config.data_dir)?;
        let empty = CString::new("")?;
        let cpu = CString::new("cpu")?;
        let ptr = unsafe {
            let mut cfg: sys::SherpaOnnxOfflineTtsConfig = std::mem::zeroed();
            cfg.model.num_threads = config.num_threads.max(1);
            cfg.model.debug = 0;
            cfg.model.provider = cpu.as_ptr();
            cfg.max_num_sentences = 1;
            cfg.silence_scale = 1.0;
            cfg.model.vits.model = model.as_ptr();
            cfg.model.vits.lexicon = empty.as_ptr();
            cfg.model.vits.tokens = tokens.as_ptr();
            cfg.model.vits.data_dir = data.as_ptr();
            cfg.model.vits.dict_dir = empty.as_ptr();
            cfg.model.vits.noise_scale = 0.667;
            cfg.model.vits.noise_scale_w = 0.8;
            cfg.model.vits.length_scale = 1.0;
            sys::SherpaOnnxCreateOfflineTts(&cfg)
        };
        ensure!(
            !ptr.is_null(),
            "Piper failed to load model {}; run opencode-converse doctor",
            config.model.display()
        );
        let rate = unsafe { sys::SherpaOnnxOfflineTtsSampleRate(ptr) };
        if rate <= 0 {
            unsafe {
                sys::SherpaOnnxDestroyOfflineTts(ptr);
            }
            anyhow::bail!("Piper returned an invalid sample rate");
        }
        Ok(Self {
            ptr,
            sample_rate: rate as u32,
        })
    }
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    pub fn synth(&mut self, text: &str, cancel: Arc<AtomicBool>) -> Result<Vec<f32>> {
        if text.is_empty() || cancel.load(Ordering::Acquire) {
            return Ok(vec![]);
        }
        let text = CString::new(text)?;
        let mut ctx = Callback {
            samples: vec![],
            cancel: &cancel,
            limit: self.sample_rate as usize * 20,
        };
        let generated = unsafe {
            sys::SherpaOnnxOfflineTtsGenerateWithProgressCallbackWithArg(
                self.ptr,
                text.as_ptr(),
                0,
                1.0,
                Some(callback),
                &mut ctx as *mut Callback as *mut c_void,
            )
        };
        if !generated.is_null() {
            unsafe {
                sys::SherpaOnnxDestroyOfflineTtsGeneratedAudio(generated);
            }
        }
        Ok(ctx.finish())
    }
}
impl Drop for Tts {
    fn drop(&mut self) {
        unsafe {
            sys::SherpaOnnxDestroyOfflineTts(self.ptr);
        }
    }
}
struct Callback<'a> {
    samples: Vec<f32>,
    cancel: &'a AtomicBool,
    limit: usize,
}
impl Callback<'_> {
    fn finish(self) -> Vec<f32> {
        // Called after native-result cleanup: cancellation can arrive after the
        // last callback. Playback must still validate its generation before use.
        if self.cancel.load(Ordering::Acquire) {
            Vec::new()
        } else {
            self.samples
        }
    }
}
unsafe extern "C" fn callback(
    samples: *const f32,
    n: i32,
    _progress: f32,
    arg: *mut c_void,
) -> i32 {
    if arg.is_null() || samples.is_null() || n <= 0 {
        return 0;
    }
    // SAFETY: pointer remains valid until synchronous generation returns.
    let ctx = unsafe { &mut *(arg as *mut Callback) };
    if ctx.cancel.load(Ordering::Acquire) {
        return 0;
    }
    if ctx.samples.len().saturating_add(n as usize) > ctx.limit {
        // Long synthesis is bounded speech truncation, not a fatal engine error.
        let remaining = ctx.limit.saturating_sub(ctx.samples.len());
        ctx.samples
            .extend_from_slice(unsafe { std::slice::from_raw_parts(samples, remaining) });
        return 0;
    }
    ctx.samples
        .extend_from_slice(unsafe { std::slice::from_raw_parts(samples, n as usize) });
    1 // sherpa: 0 aborts, 1 continues
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_paths_preserve_utf8_and_reject_nul() {
        let path = Path::new("models/voix café/model.onnx");
        assert_eq!(
            cpath(path).unwrap().as_bytes(),
            path.to_str().unwrap().as_bytes()
        );
        assert!(cpath(Path::new("models/invalid\0/model.onnx"))
            .unwrap_err()
            .to_string()
            .contains("NUL"));
    }

    #[cfg(unix)]
    #[test]
    fn all_tts_resource_paths_preserve_non_utf8_unix_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        use std::path::PathBuf;

        let root = PathBuf::from(OsString::from_vec(b"/native/models/\xff-root".to_vec()));
        for resource in ["en_US-lessac-medium.onnx", "tokens.txt", "espeak-ng-data"] {
            let path = root.join("vits-piper-en_US-lessac-medium").join(resource);
            assert!(path.to_str().is_none());
            assert_eq!(
                cpath(&path).unwrap().as_bytes(),
                path.as_os_str().as_bytes(),
                "native loader must receive the original bytes for {resource}"
            );
        }
        let nul = PathBuf::from(OsString::from_vec(b"/native/\xff\0/model.onnx".to_vec()));
        assert!(cpath(&nul).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn native_paths_reject_unrepresentable_windows_utf16() {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        use std::path::PathBuf;

        let path = PathBuf::from(OsString::from_wide(&[0x43, 0x3a, 0x5c, 0xd800, 0x5c, 0x61]));
        assert!(path.to_str().is_none());
        assert!(cpath(&path).unwrap_err().to_string().contains("UTF-8"));
    }
    #[test]
    fn cancelled_generation_never_returns_previously_appended_pcm() {
        for another_callback in [false, true] {
            let cancel = AtomicBool::new(false);
            let mut ctx = Callback {
                samples: vec![],
                cancel: &cancel,
                limit: 8,
            };
            let samples = [0.25, -0.5, 0.75];
            let append = unsafe {
                callback(
                    samples.as_ptr(),
                    samples.len() as i32,
                    0.5,
                    &mut ctx as *mut Callback as *mut c_void,
                )
            };
            assert_eq!(append, 1);
            assert_eq!(ctx.samples, samples);
            cancel.store(true, Ordering::Release);
            if another_callback {
                assert_eq!(
                    unsafe {
                        callback(
                            samples.as_ptr(),
                            samples.len() as i32,
                            1.0,
                            &mut ctx as *mut Callback as *mut c_void,
                        )
                    },
                    0
                );
                assert_eq!(
                    ctx.samples, samples,
                    "cancellation must not append another block"
                );
            }
            assert!(
                ctx.finish().is_empty(),
                "cancelled synthesis returned its previously buffered PCM"
            );
        }
    }

    #[test]
    fn valid_generation_returns_untouched_pcm_and_entry_cancellation_is_empty() {
        let cancel = AtomicBool::new(false);
        let mut ctx = Callback {
            samples: vec![],
            cancel: &cancel,
            limit: 8,
        };
        let samples = [0.25, -0.5, 0.75];
        assert_eq!(
            unsafe {
                callback(
                    samples.as_ptr(),
                    samples.len() as i32,
                    1.0,
                    &mut ctx as *mut Callback as *mut c_void,
                )
            },
            1
        );
        assert_eq!(ctx.finish(), samples);
        cancel.store(true, Ordering::Release);
        let mut ctx = Callback {
            samples: vec![],
            cancel: &cancel,
            limit: 8,
        };
        assert_eq!(
            unsafe {
                callback(
                    samples.as_ptr(),
                    samples.len() as i32,
                    1.0,
                    &mut ctx as *mut Callback as *mut c_void,
                )
            },
            0
        );
        assert!(ctx.finish().is_empty());
    }

    #[test]
    fn excessive_synthesized_audio_is_truncated_not_an_error() {
        let cancel = AtomicBool::new(false);
        let mut ctx = Callback {
            samples: vec![],
            cancel: &cancel,
            limit: 4,
        };
        let samples = [0.25f32; 10];
        let result = unsafe {
            callback(
                samples.as_ptr(),
                samples.len() as i32,
                0.0,
                &mut ctx as *mut Callback as *mut c_void,
            )
        };
        assert_eq!(result, 0);
        assert_eq!(ctx.samples, [0.25; 4]);
        assert_eq!(
            ctx.finish(),
            [0.25; 4],
            "valid truncation must remain successful PCM"
        );
    }
}
