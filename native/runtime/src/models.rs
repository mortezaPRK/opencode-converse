//! Explicit, pinned model installation. Startup never calls this module.
use crate::inventory::{self, Asset, Format, Inventory};
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

const MAX_EXTRACTED: u64 = 512 * 1024 * 1024;
const MAX_FILES: usize = 8192;
fn regular_length(path: &Path, expected: u64) -> Result<File> {
    ensure!(expected <= MAX_EXTRACTED, "File exceeds verification limit");
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink(),
        "Not a regular file: {}",
        path.display()
    );
    ensure!(
        meta.len() == expected,
        "Unexpected file length: {}",
        path.display()
    );
    let file = File::open(path)?;
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() == expected,
        "File changed before verification"
    );
    Ok(file)
}

fn digest_reader(reader: impl Read, expected: u64) -> Result<String> {
    ensure!(expected <= MAX_EXTRACTED, "File exceeds verification limit");
    let mut hash = Sha256::new();
    let mut file = reader.take(expected + 1);
    let mut bytes = 0;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        hash.update(&buffer[..n]);
    }
    ensure!(bytes == expected, "File changed length during verification");
    Ok(format!("{:x}", hash.finalize()))
}

fn digest(path: &Path, expected: u64) -> Result<String> {
    digest_reader(regular_length(path, expected)?, expected)
}
fn verified(path: &Path, asset: &Asset) -> bool {
    digest(path, asset.bytes).is_ok_and(|hash| hash == asset.sha256)
}

/// Copy into a private inode, bound the copy, then verify the bytes actually staged.
/// A checked cache pathname is never reopened as the authoritative installation source.
fn copy_verified(source: &Path, target: &Path, asset: &Asset) -> Result<()> {
    let input = regular_length(source, asset.bytes)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    let copied = std::io::copy(&mut input.take(asset.bytes + 1), &mut output)?;
    output.sync_all()?;
    ensure!(
        copied == asset.bytes && verified(target, asset),
        "Staged asset failed SHA-256/size verification"
    );
    Ok(())
}
fn extract(reader: impl Read, dest: &Path, root: &str) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    let mut total = 0u64;
    let mut files = 0usize;
    for entry in archive.entries()?.raw(true) {
        let mut entry = entry?;
        files += 1;
        ensure!(files <= MAX_FILES, "Archive has too many entries");
        let path = entry.path()?.into_owned();
        ensure!(
            !path.as_os_str().is_empty()
                && path
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)))
                && !path.to_string_lossy().contains(['\\', ':']),
            "Unsafe archive path"
        );
        ensure!(
            path.components().count() <= 64,
            "Archive path is too deeply nested"
        );
        ensure!(
            path.components()
                .next()
                .is_some_and(|p| p.as_os_str() == root),
            "Unexpected archive root"
        );
        let kind = entry.header().entry_type();
        ensure!(
            kind.is_file() || kind.is_dir(),
            "Archive links and special files are not allowed"
        );
        let length = entry.size();
        total = total.checked_add(length).context("Archive size overflow")?;
        ensure!(
            total <= MAX_EXTRACTED,
            "Archive exceeds extracted size limit"
        );
        let target = dest.join(&path);
        if kind.is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)?;
        let copied = std::io::copy(&mut entry, &mut out)?;
        ensure!(copied == length, "Truncated archive file");
        out.sync_all()?;
    }
    Ok(())
}

fn same_tree(expected: &Path, actual: &Path) -> Result<bool> {
    same_tree_bounded(expected, actual, &mut 0, &mut 0, 0)
}

fn same_tree_bounded(
    expected: &Path,
    actual: &Path,
    entries: &mut usize,
    bytes: &mut u64,
    depth: usize,
) -> Result<bool> {
    ensure!(depth <= 64, "Model directory is too deeply nested");
    let meta = fs::symlink_metadata(actual)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Ok(false);
    }
    let mut expected_names = vec![];
    for item in fs::read_dir(expected)? {
        let item = item?;
        *entries += 1;
        ensure!(*entries <= MAX_FILES, "Model tree has too many entries");
        expected_names.push(item.file_name());
        let target = actual.join(item.file_name());
        if !target.exists() {
            return Ok(false);
        }
        if item.file_type()?.is_dir() {
            if !same_tree_bounded(&item.path(), &target, entries, bytes, depth + 1)? {
                return Ok(false);
            }
        } else {
            let length = item.metadata()?.len();
            *bytes = bytes
                .checked_add(length)
                .context("Model tree size overflow")?;
            ensure!(
                *bytes <= MAX_EXTRACTED,
                "Model tree exceeds verification limit"
            );
            if digest(&item.path(), length)? != digest(&target, length)? {
                return Ok(false);
            }
        }
    }
    let mut actual_names = fs::read_dir(actual)?
        .take(expected_names.len() + 1)
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    expected_names.sort();
    actual_names.sort();
    Ok(expected_names == actual_names)
}

fn asset_lock(dir: &Path, asset: &Asset) -> Result<File> {
    fs::create_dir_all(dir)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(format!("{}.lock", asset.id)))?;
    fs2::FileExt::try_lock_exclusive(&file)
        .context("Another installer holds this model asset lock")?;
    Ok(file)
}

fn copy_cached(cached: &Path, staged: &Path, asset: &Asset) -> Result<()> {
    copy_verified(cached, staged, asset).with_context(|| format!(
        "Unverified model cache {}; inspect and remove this cache entry explicitly, then retry `opencode-converse models install`",
        cached.display()
    ))
}

pub fn install() -> Result<()> {
    let inventory = Inventory::load()?;
    let data = crate::paths::state_dir()?;
    fs::create_dir_all(&data)?;
    // Installation cannot replace files an active engine is loading.
    let _engine_lock = crate::paths::lock()?;
    let models = crate::paths::models_dir()?;
    fs::create_dir_all(&models)?;
    let cache = data.join("downloads");
    fs::create_dir_all(&cache)?;
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(1800))
        .https_only(true)
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent(concat!("opencode-converse/", env!("CARGO_PKG_VERSION")))
        .build()?;
    eprintln!("Installing pinned English speech models. Model/dependency licenses remain their owners' terms; see native/THIRD_PARTY.md and assets model notices.");
    for asset in &inventory.assets {
        ensure!(
            asset.bytes <= MAX_EXTRACTED,
            "Asset {} exceeds the verification limit",
            asset.id
        );
        let _lock = asset_lock(&data.join("model-locks"), asset)?;
        let cached = cache.join(&asset.file);
        let stage = tempfile::Builder::new()
            .prefix(".model-stage-")
            .tempdir_in(&data)?;
        let download = stage.path().join("download");
        if fs::symlink_metadata(&cached).is_ok() {
            copy_cached(&cached, &download, asset)?;
        } else {
            // An unverified cache entry is not overwritten or trusted.
            ensure!(
                !cached.try_exists()?,
                "Unverified model cache {}; remove it explicitly and retry",
                cached.display()
            );
            eprintln!("Downloading {} ({} bytes)", asset.id, asset.bytes);
            let response = client.get(&asset.url).send()?.error_for_status()?;
            if let Some(length) = response.content_length() {
                ensure!(
                    length == asset.bytes,
                    "Unexpected download Content-Length for {}",
                    asset.id
                );
            }
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&download)?;
            let n = std::io::copy(&mut response.take(asset.bytes + 1), &mut out)?;
            out.flush()?;
            out.sync_all()?;
            ensure!(
                n == asset.bytes && verified(&download, asset),
                "SHA-256/size verification failed for {}",
                asset.id
            );
            let cache_stage = stage.path().join("cache-publication");
            copy_verified(&download, &cache_stage, asset)?;
            crate::paths::publish_noclobber(&cache_stage, &cached)?;
        }
        match asset.format {
            Format::File => {
                let target = models.join(asset.runtime_path());
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                if target.exists() {
                    ensure!(
                        verified(&target, asset),
                        "Existing model {} differs; remove it explicitly and retry",
                        target.display()
                    );
                } else {
                    crate::paths::publish_noclobber(&download, &target)?;
                }
            }
            Format::TarBz2 => {
                let root_name = asset.install_root()?;
                let extracted = stage.path().join("extracted");
                fs::create_dir(&extracted)?;
                extract(
                    bzip2::read::BzDecoder::new(File::open(&download)?),
                    &extracted,
                    root_name,
                )?;
                let root = extracted.join(root_name);
                let model = inventory::archive_relative(asset.runtime_path(), root_name)?;
                let tokens = inventory::archive_relative(
                    asset
                        .tokens_path()
                        .context("Archive asset is missing its tokens path")?,
                    root_name,
                )?;
                let data_dir = inventory::archive_relative(
                    asset
                        .data_dir()
                        .context("Archive asset is missing its data directory")?,
                    root_name,
                )?;
                ensure!(
                    root.join(model).is_file()
                        && root.join(tokens).is_file()
                        && root.join(data_dir).is_dir(),
                    "Archive is missing required runtime files"
                );
                let target = models.join(
                    Path::new(asset.runtime_path())
                        .parent()
                        .context("Archive runtime path has no parent")?,
                );
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                if target.exists() {
                    ensure!(
                        same_tree(&root, &target)?,
                        "Existing model directory differs; remove {} explicitly and retry",
                        target.display()
                    );
                } else {
                    crate::paths::publish_noclobber(&root, &target)?;
                }
            }
        }
        eprintln!("Verified and installed {}", asset.id);
    }
    eprintln!("Model installation complete. Run opencode-converse doctor; device behavior is not verified by installation.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn corrupt_cache_identifies_exact_entry_and_manual_recovery_without_removal() {
        let root = tempfile::tempdir().unwrap();
        let cached = root.path().join("corrupt-model.bin");
        std::fs::write(&cached, b"bad").unwrap();
        let error = format!(
            "{:#}",
            copy_cached(&cached, &root.path().join("staged"), &asset(b"new")).unwrap_err()
        );
        assert!(error.contains(&cached.display().to_string()));
        assert!(error.contains("remove") && error.contains("retry"));
        assert_eq!(std::fs::read(cached).unwrap(), b"bad");
    }

    fn asset(content: &[u8]) -> Asset {
        Asset {
            id: "fixture".into(),
            file: "fixture.bin".into(),
            url: "https://invalid.example/fixture".into(),
            sha256: format!("{:x}", Sha256::digest(content)),
            bytes: content.len() as u64,
            format: Format::File,
            install: None,
            runtime: inventory::RuntimeAsset {
                path: "fixture.bin".into(),
                tokens: None,
                data_dir: None,
            },
        }
    }

    #[test]
    fn growing_input_is_hashed_only_to_expected_length_plus_one() {
        struct Endless(usize);
        impl Read for Endless {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                out.fill(0);
                self.0 += out.len();
                Ok(out.len())
            }
        }
        let mut input = Endless(0);
        assert!(digest_reader(&mut input, 3).is_err());
        assert_eq!(input.0, 4);
    }

    #[test]
    fn oversized_existing_files_are_rejected_before_copying() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        File::create(&source)
            .unwrap()
            .set_len(MAX_EXTRACTED + 1)
            .unwrap();
        let target = dir.path().join("private");
        assert!(!verified(&source, &asset(b"small")));
        assert!(copy_verified(&source, &target, &asset(b"small")).is_err());
        assert!(!target.exists());
    }

    #[test]
    fn private_snapshot_is_verified_and_isolated_from_later_cache_changes() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("cache");
        let snapshot = dir.path().join("snapshot");
        let pin = asset(b"verified bytes");
        fs::write(&source, b"verified bytes").unwrap();
        copy_verified(&source, &snapshot, &pin).unwrap();
        fs::write(&source, b"corrupt! bytes").unwrap();
        assert!(verified(&snapshot, &pin));
        assert!(copy_verified(&source, &dir.path().join("bad-snapshot"), &pin).is_err());
    }

    #[test]
    fn tree_verification_rejects_extras_and_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let expected = dir.path().join("expected");
        let actual = dir.path().join("actual");
        fs::create_dir(&expected).unwrap();
        fs::create_dir(&actual).unwrap();
        for id in 0..32 {
            fs::write(actual.join(format!("extra-{id}")), b"").unwrap();
        }
        assert!(!same_tree(&expected, &actual).unwrap());
        fs::write(expected.join("model"), b"ok").unwrap();
        File::create(actual.join("model"))
            .unwrap()
            .set_len(MAX_EXTRACTED + 1)
            .unwrap();
        assert!(same_tree(&expected, &actual).is_err());
    }

    #[test]
    fn installer_asset_lock_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let pin = asset(b"data");
        let first = asset_lock(dir.path(), &pin).unwrap();
        assert!(asset_lock(dir.path(), &pin).is_err());
        drop(first);
        assert!(asset_lock(dir.path(), &pin).is_ok());
    }
    #[test]
    fn compiled_inventory_is_pinned() {
        let inv = Inventory::load().unwrap();
        assert_eq!(inv.assets.len(), 4);
        assert!(inv
            .assets
            .iter()
            .all(|a| a.url.starts_with("https://") && a.sha256.len() == 64 && a.bytes > 0));
    }
    #[test]
    fn rejects_links_in_archive() {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_link(&mut header, "voice/link", "../../escape")
            .unwrap();
        let archive = builder.into_inner().unwrap();
        let dest = tempfile::tempdir().unwrap();
        assert!(extract(archive.as_slice(), dest.path(), "voice").is_err());
    }
    #[test]
    fn extraction_only_creates_regular_files() {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o777);
        header.set_cksum();
        builder
            .append_data(&mut header, "voice/tokens.txt", &b"abc"[..])
            .unwrap();
        let archive = builder.into_inner().unwrap();
        let dest = tempfile::tempdir().unwrap();
        extract(archive.as_slice(), dest.path(), "voice").unwrap();
        assert_eq!(
            fs::read(dest.path().join("voice").join("tokens.txt")).unwrap(),
            b"abc"
        );
    }
}
