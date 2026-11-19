//! Single source of truth for pinned speech assets and their runtime roles.
use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Component, Path};

const INVENTORY: &str = include_str!("../../../assets/models.json");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    pub schema: u32,
    pub language: String,
    pub roles: Roles,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Roles {
    pub vad: String,
    pub turn: String,
    pub asr: String,
    pub tts: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub id: String,
    /// Single-artifact assets (`file`, `tar.bz2`) pin one URL; `files` assets pin each member.
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub bytes: u64,
    pub format: Format,
    #[serde(default)]
    pub files: Vec<AssetFile>,
    #[serde(default)]
    pub install: Option<Install>,
    pub runtime: RuntimeAsset,
}

/// One independently pinned member of a `files` asset, installed under `runtime.path`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetFile {
    pub path: String,
    pub url: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Deserialize, PartialEq)]
pub enum Format {
    #[serde(rename = "file")]
    File,
    #[serde(rename = "tar.bz2")]
    TarBz2,
    /// A directory of individually pinned files.
    #[serde(rename = "files")]
    Files,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Install {
    pub root: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeAsset {
    pub path: String,
    #[serde(default)]
    pub tokens: Option<String>,
    #[serde(default)]
    pub data_dir: Option<String>,
}

impl Asset {
    pub fn runtime_path(&self) -> &str {
        &self.runtime.path
    }
    pub fn tokens_path(&self) -> Option<&str> {
        self.runtime.tokens.as_deref()
    }
    pub fn data_dir(&self) -> Option<&str> {
        self.runtime.data_dir.as_deref()
    }
    pub fn install_root(&self) -> Result<&str> {
        self.install
            .as_ref()
            .map(|install| install.root.as_str())
            .context("Archive asset is missing its install root")
    }
}

impl Inventory {
    pub fn load() -> Result<Self> {
        let inventory: Self =
            serde_json::from_str(INVENTORY).context("Invalid models inventory")?;
        inventory.validate()?;
        Ok(inventory)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1 && self.language == "en",
            "Unsupported compiled inventory"
        );
        let mut ids = HashSet::new();
        for asset in &self.assets {
            ensure!(ids.insert(asset.id.as_str()), "Duplicate asset id");
            ensure!(safe_component(&asset.id), "Invalid asset id");
            if asset.format == Format::Files {
                ensure!(
                    asset.file.is_empty()
                        && asset.url.is_empty()
                        && asset.sha256.is_empty()
                        && asset.bytes == 0
                        && asset.install.is_none(),
                    "Files asset {} pins only its members",
                    asset.id
                );
                ensure!(
                    !asset.files.is_empty() && asset.files.len() <= MAX_ASSET_FILES,
                    "Files asset {} has an invalid member count",
                    asset.id
                );
                let mut names = HashSet::new();
                let mut total = 0u64;
                for member in &asset.files {
                    ensure!(
                        safe_component(&member.path) && names.insert(member.path.as_str()),
                        "Invalid or duplicate member path in asset {}",
                        asset.id
                    );
                    ensure!(
                        member.url.starts_with("https://")
                            && valid_pin(&member.sha256, member.bytes),
                        "Invalid pin for {}/{}",
                        asset.id,
                        member.path
                    );
                    total = total.saturating_add(member.bytes);
                }
                ensure!(
                    total <= MAX_ASSET_BYTES,
                    "Files asset {} exceeds the size limit",
                    asset.id
                );
                ensure!(
                    safe_component(&asset.runtime.path),
                    "Files asset {} runtime path must be one directory name",
                    asset.id
                );
                ensure!(
                    asset.runtime.tokens.is_none() && asset.runtime.data_dir.is_none(),
                    "Files asset {} declares no tokens or data directory",
                    asset.id
                );
                continue;
            }
            ensure!(
                asset.files.is_empty() && safe_component(&asset.file),
                "Invalid asset id/file"
            );
            ensure!(
                valid_pin(&asset.sha256, asset.bytes),
                "Invalid compiled asset digest/size"
            );
            ensure!(
                safe_relative(&asset.runtime.path),
                "Invalid runtime path for asset {}",
                asset.id
            );
            if let Some(value) = &asset.runtime.tokens {
                ensure!(
                    safe_relative(value),
                    "Invalid runtime tokens path for asset {}",
                    asset.id
                );
            }
            if let Some(value) = &asset.runtime.data_dir {
                ensure!(
                    safe_relative(value),
                    "Invalid runtime data directory for asset {}",
                    asset.id
                );
            }
            match asset.format {
                Format::Files => unreachable!("handled before the single-artifact checks"),
                Format::File => {
                    ensure!(
                        asset.install.is_none(),
                        "File asset {} cannot have an install root",
                        asset.id
                    );
                    ensure!(
                        asset.runtime.path == asset.file,
                        "File asset {} runtime path must match its installed file",
                        asset.id
                    );
                }
                Format::TarBz2 => {
                    let root = asset.install_root()?;
                    ensure!(
                        safe_component(root),
                        "Invalid install root for asset {}",
                        asset.id
                    );
                    let parent = Path::new(&asset.runtime.path)
                        .parent()
                        .context("Archive runtime path has no parent")?;
                    for value in [
                        Some(asset.runtime.path.as_str()),
                        asset.runtime.tokens.as_deref(),
                        asset.runtime.data_dir.as_deref(),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        ensure!(
                            Path::new(value).starts_with(parent),
                            "Archive asset {} runtime paths must share one directory",
                            asset.id
                        );
                        archive_relative(value, root).with_context(|| {
                            format!(
                                "Archive asset {} path is outside its install root",
                                asset.id
                            )
                        })?;
                    }
                }
            }
        }
        for (role, id) in [
            ("vad", &self.roles.vad),
            ("turn", &self.roles.turn),
            ("asr", &self.roles.asr),
            ("tts", &self.roles.tts),
        ] {
            let asset = self
                .asset(id)
                .with_context(|| format!("Inventory role {role} references unknown asset {id}"))?;
            if role == "tts" {
                ensure!(
                    asset.format == Format::Files,
                    "Inventory TTS asset {id} must be a files directory"
                );
            }
        }
        Ok(())
    }

    pub fn asset(&self, id: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.id == id)
    }

    pub fn role_asset(&self, role: &str) -> Result<&Asset> {
        let id = match role {
            "vad" => &self.roles.vad,
            "turn" => &self.roles.turn,
            "asr" => &self.roles.asr,
            "tts" => &self.roles.tts,
            _ => anyhow::bail!("Unknown speech model role"),
        };
        self.asset(id)
            .with_context(|| format!("Inventory role {role} references unknown asset {id}"))
    }
}

const MAX_ASSET_FILES: usize = 32;
const MAX_ASSET_BYTES: u64 = 512 * 1024 * 1024;

fn valid_pin(sha256: &str, bytes: u64) -> bool {
    bytes > 0 && sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit())
}

fn safe_component(value: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && path.components().count() == 1
        && matches!(path.components().next(), Some(Component::Normal(_)))
        && !value.contains(['\\', ':'])
}

fn safe_relative(value: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && path.components().all(|c| matches!(c, Component::Normal(_)))
        && !value.contains(['\\', ':'])
}

/// Return the archive path below its declared install root.
pub fn archive_relative<'a>(runtime_path: &'a str, root: &str) -> Result<&'a str> {
    let path = Path::new(runtime_path);
    let mut components = path.components();
    while let Some(component) = components.next() {
        if component.as_os_str() == root {
            return components
                .as_path()
                .to_str()
                .context("Archive runtime path is not valid UTF-8");
        }
    }
    anyhow::bail!("{runtime_path} is not under install root {root}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_inventory_is_consistent() {
        let inventory = Inventory::load().unwrap();
        assert_eq!(inventory.assets.len(), 4);
        assert_eq!(inventory.role_asset("vad").unwrap().id, "silero-vad");
        let tts = inventory.role_asset("tts").unwrap();
        assert_eq!(tts.id, "phonon-31m-en");
        assert_eq!(tts.format, Format::Files);
        for name in [
            "config.json",
            "tokenizer.json",
            "model.q8.gguf",
            "default-voice.safetensors",
            "LICENSE",
        ] {
            assert!(tts.files.iter().any(|file| file.path == name), "{name}");
        }
        assert!(inventory.assets.iter().all(|asset| {
            if asset.format == Format::Files {
                asset
                    .files
                    .iter()
                    .all(|f| f.url.starts_with("https://") && f.bytes > 0)
            } else {
                asset.url.starts_with("https://") && asset.bytes > 0
            }
        }));
    }

    #[test]
    fn rejects_traversal_and_cross_platform_paths() {
        for path in [
            "../escape",
            "/absolute",
            "dir/../escape",
            "C:/escape",
            "dir\\escape",
        ] {
            assert!(!safe_relative(path), "{path}");
        }
    }

    fn parse(json: &str) -> Result<Inventory> {
        let inventory: Inventory = serde_json::from_str(json)?;
        inventory.validate()?;
        Ok(inventory)
    }

    fn with_tts(files: &str) -> String {
        let mut value: serde_json::Value = serde_json::from_str(INVENTORY).unwrap();
        let tts = value["assets"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|asset| asset["id"] == "phonon-31m-en")
            .unwrap();
        tts["files"] = serde_json::from_str(files).unwrap();
        value.to_string()
    }

    #[test]
    fn files_assets_reject_unsafe_duplicate_insecure_and_unpinned_members() {
        let pin = "0".repeat(64);
        let member = |path: &str, url: &str, sha: &str, bytes: u64| {
            format!(r#"[{{"path":"{path}","url":"{url}","sha256":"{sha}","bytes":{bytes}}}]"#)
        };
        assert!(parse(&with_tts(&member("a.bin", "https://x.example/a", &pin, 1))).is_ok());
        for bad in [
            member("../a.bin", "https://x.example/a", &pin, 1),
            member("dir/a.bin", "https://x.example/a", &pin, 1),
            member("a.bin", "http://x.example/a", &pin, 1),
            member("a.bin", "https://x.example/a", "abc", 1),
            member("a.bin", "https://x.example/a", &pin, 0),
            member("a.bin", "https://x.example/a", &pin, MAX_ASSET_BYTES + 1),
            "[]".to_string(),
        ] {
            assert!(parse(&with_tts(&bad)).is_err(), "{bad}");
        }
        let duplicate = format!(
            r#"[{{"path":"a","url":"https://x.example/a","sha256":"{pin}","bytes":1}},{{"path":"a","url":"https://x.example/b","sha256":"{pin}","bytes":1}}]"#
        );
        assert!(parse(&with_tts(&duplicate)).is_err());
    }

    #[test]
    fn archive_runtime_paths_must_stay_under_the_install_root() {
        assert_eq!(
            archive_relative("tts/voice/model.onnx", "voice").unwrap(),
            "model.onnx"
        );
        assert!(archive_relative("tts/other/model.onnx", "voice").is_err());
    }
}
