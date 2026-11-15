use std::path::{Path, PathBuf};

fn absolute_base(value: std::ffi::OsString, name: &str) -> anyhow::Result<PathBuf> {
    let path = PathBuf::from(value);
    anyhow::ensure!(
        path.is_absolute(),
        "{name} must be an absolute local storage path"
    );
    Ok(path)
}

#[cfg(any(test, target_os = "windows"))]
fn windows_base(
    local: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> anyhow::Result<PathBuf> {
    if let Some(value) = local {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            return Ok(path);
        }
    }
    Ok(absolute_base(
        home.ok_or_else(|| anyhow::anyhow!("LOCALAPPDATA and USERPROFILE are unavailable"))?,
        "USERPROFILE",
    )?
    .join("AppData/Local"))
}

/// Atomic publication must never replace a file, symlink, or even an empty directory.
/// Unsupported filesystems/platforms fail instead of falling back to check-then-rename.
pub fn publish_noclobber(source: &Path, target: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let source = CString::new(source.as_os_str().as_bytes())?;
        let target = CString::new(target.as_os_str().as_bytes())?;
        #[cfg(target_os = "macos")]
        let result = {
            unsafe extern "C" {
                fn renamex_np(
                    from: *const std::ffi::c_char,
                    to: *const std::ffi::c_char,
                    flags: u32,
                ) -> i32;
            }
            // RENAME_EXCL is the macOS exclusive rename primitive.
            // SAFETY: both owned C strings are NUL-terminated and live throughout the call.
            unsafe { renamex_np(source.as_ptr(), target.as_ptr(), 0x4) }
        };
        #[cfg(target_os = "linux")]
        let result = {
            unsafe extern "C" {
                fn renameat2(
                    from_dir: i32,
                    from: *const std::ffi::c_char,
                    to_dir: i32,
                    to: *const std::ffi::c_char,
                    flags: u32,
                ) -> i32;
            }
            // AT_FDCWD and RENAME_NOREPLACE; no unsafe overwrite fallback on ENOSYS.
            // SAFETY: both owned C strings are NUL-terminated and live throughout the call.
            unsafe { renameat2(-100, source.as_ptr(), -100, target.as_ptr(), 1) }
        };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        anyhow::bail!("Exclusive model publication is unsupported on this platform");
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn MoveFileExW(from: *const u16, to: *const u16, flags: u32) -> i32;
        }
        let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
        anyhow::ensure!(
            !source[..source.len() - 1].contains(&0) && !target[..target.len() - 1].contains(&0),
            "Path contains NUL"
        );
        // Flags=0 deliberately omits MOVEFILE_REPLACE_EXISTING and cross-volume copying.
        // SAFETY: both paths are NUL-terminated owned buffers and contain no interior NUL.
        if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    #[cfg(not(any(unix, windows)))]
    anyhow::bail!("Exclusive model publication is unsupported on this platform");
    Ok(())
}

pub fn data_dir() -> anyhow::Result<PathBuf> {
    // Overrides are deliberately local-development-only, never part of stdio settings.
    if cfg!(debug_assertions) {
        if let Some(path) = std::env::var_os("OPENCODE_CONVERSE_DEV_DATA_DIR") {
            let path = PathBuf::from(path);
            anyhow::ensure!(
                path.is_absolute(),
                "OPENCODE_CONVERSE_DEV_DATA_DIR must be absolute"
            );
            return Ok(path);
        }
    }
    #[cfg(target_os = "windows")]
    let path = windows_base(
        std::env::var_os("LOCALAPPDATA"),
        std::env::var_os("USERPROFILE"),
    )?;
    #[cfg(target_os = "macos")]
    let path = absolute_base(
        std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is unavailable"))?,
        "HOME",
    )?
    .join("Library/Application Support");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let path = match std::env::var_os("XDG_DATA_HOME") {
        Some(path) if PathBuf::from(&path).is_absolute() => PathBuf::from(path),
        _ => absolute_base(
            std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is unavailable"))?,
            "HOME",
        )?
        .join(".local/share"),
    };
    Ok(path.join("opencode-converse"))
}

pub const MODEL_FILES: &[&str] = &[
    "silero_vad.onnx",
    "smart-turn-v3.2-cpu.onnx",
    "ggml-base.en.bin",
    "tts/vits-piper-en_US-lessac-medium/en_US-lessac-medium.onnx",
    "tts/vits-piper-en_US-lessac-medium/tokens.txt",
];

pub fn check_models() -> anyhow::Result<PathBuf> {
    let models = data_dir()?.join("models");
    let mut missing: Vec<_> = MODEL_FILES
        .iter()
        .filter(|name| !models.join(name).is_file())
        .map(|name| models.join(name).display().to_string())
        .collect();
    let espeak = models.join("tts/vits-piper-en_US-lessac-medium/espeak-ng-data");
    if !espeak.is_dir() {
        missing.push(espeak.display().to_string());
    }
    anyhow::ensure!(missing.is_empty(), "Models are missing. Run `opencode-converse models install` to explicitly download the pinned, separately licensed English artifacts. Required locations:\n{}\nThen run `opencode-converse doctor`.", missing.join("\n"));
    Ok(models)
}

pub fn lock() -> anyhow::Result<std::fs::File> {
    let dir = data_dir()?;
    lock_at(&dir)
}

fn lock_at(dir: &Path) -> anyhow::Result<std::fs::File> {
    std::fs::create_dir_all(dir)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("engine.lock"))?;
    fs2::FileExt::try_lock_exclusive(&file).map_err(|_| {
        anyhow::anyhow!("Another OpenCode Converse runtime holds the local audio engine lock")
    })?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_missing_local_data_uses_only_absolute_host_home() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            windows_base(None, Some(root.path().into())).unwrap(),
            root.path().join("AppData/Local")
        );
        for value in ["", ".", "relative"] {
            assert!(windows_base(None, Some(value.into())).is_err());
            assert_eq!(
                windows_base(Some(value.into()), Some(root.path().into())).unwrap(),
                root.path().join("AppData/Local")
            );
        }
        assert!(windows_base(None, None).is_err());
    }
    #[test]
    fn local_storage_base_must_be_absolute() {
        for value in ["", "relative", "."] {
            assert!(absolute_base(value.into(), "HOME").is_err());
        }
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            absolute_base(root.path().into(), "HOME").unwrap(),
            root.path()
        );
    }

    #[test]
    fn publication_never_replaces_files_or_empty_directories() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        std::fs::write(&source, b"new").unwrap();
        std::fs::write(&target, b"existing").unwrap();
        assert!(publish_noclobber(&source, &target).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"existing");
        std::fs::remove_file(&source).unwrap();
        std::fs::remove_file(&target).unwrap();
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("file"), b"new").unwrap();
        std::fs::create_dir(&target).unwrap();
        assert!(publish_noclobber(&source, &target).is_err());
        assert!(source.is_dir() && target.is_dir());
        std::fs::remove_dir(&target).unwrap();
        publish_noclobber(&source, &target).unwrap();
        assert_eq!(std::fs::read(target.join("file")).unwrap(), b"new");
    }

    #[test]
    fn racing_publishers_have_exactly_one_winner() {
        let root = tempfile::tempdir().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let target = root.path().join("target");
        let jobs: Vec<_> = (0..2)
            .map(|id| {
                let source = root.path().join(format!("source-{id}"));
                std::fs::write(&source, [id]).unwrap();
                let target = target.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    publish_noclobber(&source, &target).is_ok()
                })
            })
            .collect();
        assert_eq!(
            jobs.into_iter()
                .map(|job| usize::from(job.join().unwrap()))
                .sum::<usize>(),
            1
        );
        assert_eq!(std::fs::read(target).unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn publication_does_not_replace_a_dangling_symlink() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        std::fs::write(&source, b"new").unwrap();
        std::os::unix::fs::symlink("missing", &target).unwrap();
        assert!(publish_noclobber(&source, &target).is_err());
        assert!(std::fs::symlink_metadata(target)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn engine_lock_is_exclusive_and_released_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let first = lock_at(root.path()).unwrap();
        assert!(lock_at(root.path()).is_err());
        drop(first);
        assert!(lock_at(root.path()).is_ok());
    }
}
