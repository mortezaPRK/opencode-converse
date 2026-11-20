//! Pinned model installation and status. Startup never downloads.
use crate::inventory::{self, Asset, Format, Inventory};
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

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

/// Download (or copy from the verified cache) one pinned artifact into a private staged file.
/// The returned path is the only authoritative source for installation.
fn fetch(
    client: &reqwest::blocking::Client,
    cache: &Path,
    stage: &Path,
    name: &str,
    pin: &Asset,
    report: &mut Reporter,
) -> Result<std::path::PathBuf> {
    let cached = cache.join(&pin.file);
    let download = stage.join(name);
    if fs::symlink_metadata(&cached).is_ok() {
        copy_cached(&cached, &download, pin)?;
        report.progress(pin.bytes, true);
        return Ok(download);
    }
    // An unverified cache entry is not overwritten or trusted.
    ensure!(
        !cached.try_exists()?,
        "Unverified model cache {}; remove it explicitly and retry",
        cached.display()
    );
    report.note(&format!("Downloading {} ({} bytes)", pin.file, pin.bytes));
    let response = client.get(&pin.url).send()?.error_for_status()?;
    if let Some(length) = response.content_length() {
        ensure!(
            length == pin.bytes,
            "Unexpected download Content-Length for {}",
            pin.file
        );
    }
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&download)?;
    let mut input = response.take(pin.bytes + 1);
    let mut buffer = [0u8; 64 * 1024];
    let mut n = 0u64;
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        out.write_all(&buffer[..read])?;
        n += read as u64;
        report.progress(read as u64, false);
    }
    out.flush()?;
    out.sync_all()?;
    ensure!(
        n == pin.bytes && verified(&download, pin),
        "SHA-256/size verification failed for {}",
        pin.file
    );
    let cache_stage = stage.join(format!("{name}-cache-publication"));
    copy_verified(&download, &cache_stage, pin)?;
    crate::paths::publish_noclobber(&cache_stage, &cached)?;
    Ok(download)
}

/// A single-file pin for a `files` member. Its cache name is namespaced by the asset id.
fn member_pin(asset: &Asset, member: &inventory::AssetFile) -> Asset {
    Asset {
        id: asset.id.clone(),
        file: format!("{}--{}", asset.id, member.path),
        url: member.url.clone(),
        sha256: member.sha256.clone(),
        bytes: member.bytes,
        format: Format::File,
        files: vec![],
        install: None,
        runtime: inventory::RuntimeAsset {
            path: member.path.clone(),
            tokens: None,
            data_dir: None,
        },
    }
}

const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
const WAIT_INTERVAL: Duration = Duration::from_secs(2);
const LOCK_WAIT: Duration = Duration::from_secs(30 * 60);
const MAX_DOWNLOAD_ENTRIES: usize = 1024;
const MAX_MESSAGE_CHARS: usize = 2000;

/// Human lines go to stderr. In JSON mode every event is one bounded line on `out`.
pub struct Reporter {
    json: bool,
    out: Box<dyn Write>,
    asset: Option<String>,
    total: u64,
    done: u64,
    last: Option<Instant>,
}

impl Reporter {
    pub fn new(json: bool, out: Box<dyn Write>, total: u64) -> Self {
        Self {
            json,
            out,
            asset: None,
            total,
            done: 0,
            last: None,
        }
    }
    fn event(&mut self, value: serde_json::Value) {
        if self.json {
            let _ = writeln!(self.out, "{value}");
            let _ = self.out.flush();
        }
    }
    fn note(&self, text: &str) {
        eprintln!("{text}");
    }
    fn start(&mut self, id: &str, bytes: u64) {
        self.asset = Some(id.into());
        self.event(serde_json::json!({"type":"start","asset":id,"bytes":bytes}));
    }
    fn progress(&mut self, bytes: u64, force: bool) {
        self.done = (self.done + bytes).min(self.total);
        let due = self.last.is_none_or(|at| at.elapsed() >= PROGRESS_INTERVAL);
        if force || due || self.done == self.total {
            self.last = Some(Instant::now());
            let asset = self.asset.clone();
            self.event(serde_json::json!({"type":"progress","asset":asset,"done":self.done,"total":self.total}));
        }
    }
    fn verified(&mut self, id: &str) {
        self.event(serde_json::json!({"type":"verified","asset":id}));
        eprintln!("Verified and installed {id}");
    }
    fn finish(&mut self) {
        self.event(serde_json::json!({"type":"done"}));
    }
    fn fail(&mut self, error: &anyhow::Error) {
        let message: String = format!("{error:#}")
            .chars()
            .take(MAX_MESSAGE_CHARS)
            .collect();
        let asset = self.asset.clone();
        self.event(serde_json::json!({"type":"error","asset":asset,"message":message}));
    }
}

/// Cache file names (under `downloads/`) that one asset's pinned artifacts use.
fn cache_names(asset: &Asset) -> Vec<String> {
    if asset.format == Format::Files {
        asset
            .files
            .iter()
            .map(|member| member_pin(asset, member).file)
            .collect()
    } else {
        vec![asset.file.clone()]
    }
}

fn asset_bytes(asset: &Asset) -> u64 {
    if asset.format == Format::Files {
        asset.files.iter().map(|member| member.bytes).sum()
    } else {
        asset.bytes
    }
}

/// Installed members and their pins: (path under models/, pin).
fn installed_members(models: &Path, asset: &Asset) -> Vec<(std::path::PathBuf, Asset)> {
    let root = models.join(asset.runtime_path());
    if asset.format == Format::Files {
        asset
            .files
            .iter()
            .map(|member| (root.join(&member.path), member_pin(asset, member)))
            .collect()
    } else {
        vec![(
            root,
            Asset {
                id: asset.id.clone(),
                file: asset.file.clone(),
                url: asset.url.clone(),
                sha256: asset.sha256.clone(),
                bytes: asset.bytes,
                format: Format::File,
                files: vec![],
                install: None,
                runtime: inventory::RuntimeAsset {
                    path: asset.runtime.path.clone(),
                    tokens: None,
                    data_dir: None,
                },
            },
        )]
    }
}

/// Cheap, offline check: every pinned file exists with its pinned length.
fn present(models: &Path, asset: &Asset) -> bool {
    installed_members(models, asset).iter().all(|(path, pin)| {
        fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() == pin.bytes)
    })
}

/// Full check used before skipping an install: every pinned file hashes correctly.
fn intact(models: &Path, asset: &Asset) -> bool {
    asset.format != Format::TarBz2
        && installed_members(models, asset)
            .iter()
            .all(|(path, pin)| verified(path, pin))
}

/// `models status --json`: one line, no network, no hashing.
pub fn status(json: bool) -> Result<()> {
    let inventory = Inventory::load()?;
    let models = crate::paths::models_dir()?;
    let mut assets = vec![];
    let mut missing = 0u64;
    for asset in &inventory.assets {
        let installed = present(&models, asset);
        if !installed {
            missing += asset_bytes(asset);
        }
        assets.push(
            serde_json::json!({"id":asset.id,"installed":installed,"bytes":asset_bytes(asset)}),
        );
    }
    let installed = missing == 0;
    if json {
        println!(
            "{}",
            serde_json::json!({"type":"status","installed":installed,"missing_bytes":missing,"assets":assets})
        );
    } else if installed {
        eprintln!("Speech models are installed.");
    } else {
        eprintln!("Speech models are missing ({missing} bytes to download). Run `opencode-converse models install`.");
    }
    Ok(())
}

/// Remove regular files in `downloads/` that are not in `keep`. Never follows or removes
/// links, directories, or anything outside `cache`; scans a bounded number of entries.
fn clean_downloads(cache: &Path, keep: &HashSet<String>) {
    let Ok(entries) = fs::read_dir(cache) else {
        return;
    };
    for entry in entries.take(MAX_DOWNLOAD_ENTRIES).flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if keep.contains(&name) || !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let _ = fs::remove_file(entry.path());
    }
}

/// Staging directories left behind by an installer that was killed. The engine lock is
/// held, so no live installer owns one. Only real directories with our prefix are removed.
fn remove_stale_stages(data: &Path) {
    let Ok(entries) = fs::read_dir(data) else {
        return;
    };
    for entry in entries.take(MAX_DOWNLOAD_ENTRIES).flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".model-stage-")
            && entry.file_type().is_ok_and(|kind| kind.is_dir())
        {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn remove_cached(cache: &Path, asset: &Asset) {
    for name in cache_names(asset) {
        if fs::symlink_metadata(cache.join(&name)).is_ok_and(|meta| meta.is_file()) {
            let _ = fs::remove_file(cache.join(name));
        }
    }
}

fn acquire_engine_lock(dir: &Path, report: &mut Reporter) -> Result<File> {
    acquire_lock(dir, report, LOCK_WAIT, WAIT_INTERVAL)
}

fn acquire_lock(
    dir: &Path,
    report: &mut Reporter,
    wait: Duration,
    interval: Duration,
) -> Result<File> {
    let deadline = Instant::now() + wait;
    let mut announced = false;
    loop {
        match crate::paths::lock_at(dir) {
            Ok(file) => return Ok(file),
            Err(error)
                if error.to_string().contains("audio engine lock") && Instant::now() < deadline =>
            {
                if !announced {
                    announced = true;
                    report.note("Waiting for another Converse process to finish...");
                }
                // Heartbeat so a supervising host can tell waiting from a hang.
                report.event(serde_json::json!({"type":"waiting"}));
                std::thread::sleep(interval);
            }
            Err(error) => return Err(error),
        }
    }
}

pub fn install(json: bool) -> Result<()> {
    let mut report = Reporter::new(json, Box::new(std::io::stdout()), 0);
    let result = install_all(&mut report);
    match &result {
        Ok(()) => report.finish(),
        Err(error) => report.fail(error),
    }
    result
}

fn install_all(report: &mut Reporter) -> Result<()> {
    let inventory = Inventory::load()?;
    let data = crate::paths::state_dir()?;
    fs::create_dir_all(&data)?;
    // Installation cannot replace files an active engine is loading. Concurrent installers
    // queue here; the later one finds everything installed and verified.
    let _engine_lock = acquire_engine_lock(&data, report)?;
    let models = crate::paths::models_dir()?;
    fs::create_dir_all(&models)?;
    let cache = data.join("downloads");
    fs::create_dir_all(&cache)?;
    remove_stale_stages(&data);
    let pending: Vec<&Asset> = inventory
        .assets
        .iter()
        .filter(|asset| !intact(&models, asset))
        .collect();
    // Drop obsolete files; keep only what still-pending pinned assets can reuse.
    clean_downloads(
        &cache,
        &pending
            .iter()
            .flat_map(|asset| cache_names(asset))
            .collect(),
    );
    report.total = pending.iter().map(|asset| asset_bytes(asset)).sum();
    for asset in &inventory.assets {
        if !pending.iter().any(|item| item.id == asset.id) {
            remove_cached(&cache, asset);
        }
    }
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(1800))
        .https_only(true)
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent(concat!("opencode-converse/", env!("CARGO_PKG_VERSION")))
        .build()?;
    report.note("Installing pinned English speech models. Model/dependency licenses remain their owners' terms; see native/THIRD_PARTY.md and assets model notices.");
    for asset in pending {
        ensure!(
            asset.bytes <= MAX_EXTRACTED,
            "Asset {} exceeds the verification limit",
            asset.id
        );
        report.start(&asset.id, asset_bytes(asset));
        install_asset(&client, &data, &models, &cache, asset, report)
            .with_context(|| format!("Installing {}", asset.id))?;
        remove_cached(&cache, asset);
        report.verified(&asset.id);
    }
    report.note("Model installation complete. Run opencode-converse doctor; device behavior is not verified by installation.");
    Ok(())
}

fn install_asset(
    client: &reqwest::blocking::Client,
    data: &Path,
    models: &Path,
    cache: &Path,
    asset: &Asset,
    report: &mut Reporter,
) -> Result<()> {
    let _lock = asset_lock(&data.join("model-locks"), asset)?;
    let stage = tempfile::Builder::new()
        .prefix(".model-stage-")
        .tempdir_in(data)?;
    if asset.format == Format::Files {
        let target = models.join(asset.runtime_path());
        let staged = stage.path().join("files");
        fs::create_dir(&staged)?;
        for (index, member) in asset.files.iter().enumerate() {
            let pin = member_pin(asset, member);
            let download = fetch(
                client,
                cache,
                stage.path(),
                &format!("download-{index}"),
                &pin,
                report,
            )?;
            // The verified private copy becomes the staged member without reopening a cache path.
            fs::rename(&download, staged.join(&member.path))?;
        }
        if target.exists() {
            ensure!(
                same_tree(&staged, &target)?,
                "Existing model directory differs; remove {} explicitly and retry",
                target.display()
            );
        } else {
            crate::paths::publish_noclobber(&staged, &target)?;
        }
        return Ok(());
    }
    let download = fetch(client, cache, stage.path(), "download", asset, report)?;
    match asset.format {
        Format::Files => unreachable!("installed above"),
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn lines(sink: &Sink) -> Vec<serde_json::Value> {
        String::from_utf8(sink.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn json_progress_is_throttled_bounded_and_ends_at_total() {
        let sink = Sink::default();
        let mut report = Reporter::new(true, Box::new(sink.clone()), 1000);
        report.start("a", 1000);
        for _ in 0..1000 {
            report.progress(1, false);
        }
        report.progress(50, false); // overrun is clamped
        report.verified("a");
        report.finish();
        let events = lines(&sink);
        assert_eq!(events[0]["type"], "start");
        let progress: Vec<_> = events.iter().filter(|e| e["type"] == "progress").collect();
        assert!(progress.len() < 20, "throttled, got {}", progress.len());
        assert_eq!(progress.last().unwrap()["done"], 1000);
        assert!(progress
            .iter()
            .all(|e| e["done"].as_u64().unwrap() <= 1000 && e["asset"] == "a"));
        assert_eq!(events[events.len() - 2]["type"], "verified");
        assert_eq!(events.last().unwrap()["type"], "done");
    }

    #[test]
    fn json_errors_name_the_asset_and_bound_the_message() {
        let sink = Sink::default();
        let mut report = Reporter::new(true, Box::new(sink.clone()), 1);
        report.start("tts", 1);
        report.fail(&anyhow::anyhow!("x".repeat(10_000)));
        let event = &lines(&sink)[1];
        assert_eq!(event["type"], "error");
        assert_eq!(event["asset"], "tts");
        assert_eq!(event["message"].as_str().unwrap().len(), MAX_MESSAGE_CHARS);
    }

    #[test]
    fn human_mode_emits_no_json() {
        let sink = Sink::default();
        let mut report = Reporter::new(false, Box::new(sink.clone()), 10);
        report.start("a", 10);
        report.progress(10, true);
        report.finish();
        assert!(sink.0.lock().unwrap().is_empty());
    }

    #[test]
    fn status_is_presence_and_length_only() {
        let dir = tempfile::tempdir().unwrap();
        let pin = asset(b"data");
        assert!(!present(dir.path(), &pin));
        fs::write(dir.path().join("fixture.bin"), b"dat").unwrap();
        assert!(!present(dir.path(), &pin));
        fs::write(dir.path().join("fixture.bin"), b"xxxx").unwrap();
        assert!(present(dir.path(), &pin) && !intact(dir.path(), &pin));
        fs::write(dir.path().join("fixture.bin"), b"data").unwrap();
        assert!(present(dir.path(), &pin) && intact(dir.path(), &pin));
        assert_eq!(asset_bytes(&pin), 4);
    }

    #[test]
    fn files_assets_need_every_member() {
        let inv = Inventory::load().unwrap();
        let multi = inv
            .assets
            .iter()
            .find(|a| a.format == Format::Files)
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(multi.runtime_path());
        fs::create_dir_all(&root).unwrap();
        assert!(!present(dir.path(), multi));
        assert_eq!(cache_names(multi).len(), multi.files.len());
        assert!(cache_names(multi)[0].starts_with(&format!("{}--", multi.id)));
    }

    #[test]
    fn download_cleanup_removes_stale_files_only_inside_downloads() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("downloads");
        fs::create_dir_all(cache.join("subdir")).unwrap();
        fs::write(cache.join("keep.bin"), b"k").unwrap();
        fs::write(cache.join("old-model.tar.bz2"), b"o").unwrap();
        fs::write(cache.join("subdir").join("inner"), b"i").unwrap();
        let outside = root.path().join("outside.txt");
        fs::write(&outside, b"precious").unwrap();
        fs::write(root.path().join("sibling.bin"), b"s").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, cache.join("link")).unwrap();
        clean_downloads(&cache, &HashSet::from(["keep.bin".to_string()]));
        assert!(cache.join("keep.bin").exists());
        assert!(!cache.join("old-model.tar.bz2").exists());
        assert!(cache.join("subdir").join("inner").exists());
        assert_eq!(fs::read(&outside).unwrap(), b"precious");
        assert!(root.path().join("sibling.bin").exists());
        #[cfg(unix)]
        assert!(fs::symlink_metadata(cache.join("link")).is_ok());
    }

    #[test]
    fn download_cleanup_is_bounded() {
        let root = tempfile::tempdir().unwrap();
        for id in 0..(MAX_DOWNLOAD_ENTRIES + 50) {
            fs::write(root.path().join(format!("f{id}")), b"").unwrap();
        }
        clean_downloads(root.path(), &HashSet::new());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 50);
    }

    #[test]
    fn stale_stage_directories_are_removed_without_touching_anything_else() {
        let data = tempfile::tempdir().unwrap();
        fs::create_dir_all(data.path().join(".model-stage-abc/inner")).unwrap();
        fs::create_dir_all(data.path().join("models")).unwrap();
        fs::write(data.path().join(".model-stage-file"), b"x").unwrap();
        remove_stale_stages(data.path());
        assert!(!data.path().join(".model-stage-abc").exists());
        assert!(data.path().join("models").exists());
        assert!(data.path().join(".model-stage-file").exists());
    }

    #[test]
    fn concurrent_installers_queue_on_the_engine_lock() {
        let dir = tempfile::tempdir().unwrap();
        let held = crate::paths::lock_at(dir.path()).unwrap();
        let sink = Sink::default();
        let mut report = Reporter::new(true, Box::new(sink.clone()), 0);
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        let waited = acquire_lock(
            dir.path(),
            &mut report,
            Duration::from_secs(10),
            Duration::from_millis(50),
        );
        assert!(waited.is_ok());
        release.join().unwrap();
        assert!(lines(&sink).iter().any(|e| e["type"] == "waiting"));
        // A holder that never leaves times the waiter out with the lock error.
        let timed_out = acquire_lock(
            dir.path(),
            &mut report,
            Duration::from_millis(150),
            Duration::from_millis(50),
        );
        assert!(format!("{:#}", timed_out.unwrap_err()).contains("audio engine lock"));
    }

    #[test]
    fn installed_asset_cache_is_removed() {
        let cache = tempfile::tempdir().unwrap();
        let pin = asset(b"data");
        fs::write(cache.path().join("fixture.bin"), b"data").unwrap();
        fs::write(cache.path().join("other.bin"), b"x").unwrap();
        remove_cached(cache.path(), &pin);
        assert!(!cache.path().join("fixture.bin").exists());
        assert!(cache.path().join("other.bin").exists());
    }

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
            files: vec![],
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
        assert_eq!(inv.assets.len(), 5);
        for asset in &inv.assets {
            if asset.format == Format::Files {
                assert!(asset
                    .files
                    .iter()
                    .all(|f| f.url.starts_with("https://") && f.sha256.len() == 64 && f.bytes > 0));
            } else {
                assert!(asset.url.starts_with("https://") && asset.sha256.len() == 64);
            }
        }
    }

    #[test]
    fn member_hash_mismatch_is_rejected_and_never_published() {
        let dir = tempfile::tempdir().unwrap();
        let parent = asset(b"unused");
        let member = inventory::AssetFile {
            path: "weights.bin".into(),
            url: "https://invalid.example/weights".into(),
            sha256: format!("{:x}", Sha256::digest(b"expected")),
            bytes: 8,
        };
        let pin = member_pin(&parent, &member);
        assert_eq!(pin.file, "fixture--weights.bin");
        let cached = dir.path().join(&pin.file);
        fs::write(&cached, b"tampered").unwrap(); // right length, wrong digest
        let staged = dir.path().join("staged");
        assert!(copy_cached(&cached, &staged, &pin).is_err());
        assert!(!staged.exists() || !verified(&staged, &pin));
        let staged = dir.path().join("staged-good");
        fs::write(&cached, b"expected").unwrap();
        copy_cached(&cached, &staged, &pin).unwrap();
        assert!(verified(&staged, &pin));
    }

    #[test]
    fn files_directory_publication_is_no_clobber_and_detects_drift() {
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("staged");
        let target = dir.path().join("models").join("tts");
        fs::create_dir_all(&staged).unwrap();
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(staged.join("a.bin"), b"one").unwrap();
        crate::paths::publish_noclobber(&staged, &target).unwrap();
        fs::create_dir_all(&staged).unwrap();
        fs::write(staged.join("a.bin"), b"one").unwrap();
        assert!(same_tree(&staged, &target).unwrap());
        assert!(crate::paths::publish_noclobber(&staged, &target).is_err());
        fs::write(target.join("a.bin"), b"two").unwrap();
        assert!(!same_tree(&staged, &target).unwrap());
        fs::write(target.join("a.bin"), b"one").unwrap();
        fs::write(target.join("extra.bin"), b"x").unwrap();
        assert!(!same_tree(&staged, &target).unwrap());
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
