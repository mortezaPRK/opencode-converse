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
    pub file: String,
    pub url: String,
    pub sha256: String,
    pub bytes: u64,
    pub format: Format,
    #[serde(default)]
    pub install: Option<Install>,
    pub runtime: RuntimeAsset,
}

#[derive(Debug, Deserialize, PartialEq)]
pub enum Format {
    #[serde(rename = "file")]
    File,
    #[serde(rename = "tar.bz2")]
    TarBz2,
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
            ensure!(
                safe_component(&asset.id) && safe_component(&asset.file),
                "Invalid asset id/file"
            );
            ensure!(
                asset.bytes > 0
                    && asset.sha256.len() == 64
                    && asset.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
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
                    asset.tokens_path().is_some() && asset.data_dir().is_some(),
                    "Inventory TTS asset {id} must declare tokens and dataDir"
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
        assert_eq!(inventory.role_asset("tts").unwrap().id, "piper-lessac");
        assert!(inventory
            .assets
            .iter()
            .all(|asset| asset.url.starts_with("https://") && asset.bytes > 0));
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

    #[test]
    fn archive_runtime_paths_must_stay_under_the_install_root() {
        assert_eq!(
            archive_relative("tts/voice/model.onnx", "voice").unwrap(),
            "model.onnx"
        );
        assert!(archive_relative("tts/other/model.onnx", "voice").is_err());
    }
}
