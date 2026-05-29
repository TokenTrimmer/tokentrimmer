//! `.tt-init.lock` — records SHA-256 of each template file installed,
//! so `--upgrade` can detect customer modifications.

use std::collections::BTreeMap;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("serialize: {0}")]
    Serialize(#[from] toml::ser::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Manifest {
    pub version: String,
    /// Map of relative path (forward slashes) → SHA-256 hex of original template content.
    pub installed: BTreeMap<String, String>,
}

impl Manifest {
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            installed: BTreeMap::new(),
        }
    }

    pub fn record(&mut self, rel_path: &Path, original_content: &str) {
        let hex = sha256_hex(original_content.as_bytes());
        self.installed
            .insert(rel_path.to_string_lossy().replace('\\', "/"), hex);
    }

    pub fn load(path: &Path) -> Result<Option<Manifest>, ManifestError> {
        if !path.exists() {
            return Ok(None);
        }
        let s = std::fs::read_to_string(path)?;
        Ok(Some(toml::from_str(&s)?))
    }

    pub fn save(&self, path: &Path) -> Result<(), ManifestError> {
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeAction {
    /// File is unmodified vs the installed template — safe to overwrite.
    SafeOverwrite,
    /// File was modified by the user — skip unless --force.
    UserModified,
    /// File did not exist before this run — fresh install.
    Fresh,
}

pub fn classify_upgrade(
    manifest: &Manifest,
    dest_rel: &Path,
    current_disk_content: Option<&str>,
) -> UpgradeAction {
    let rel = dest_rel.to_string_lossy().replace('\\', "/");
    let recorded = manifest.installed.get(&rel);
    match (recorded, current_disk_content) {
        (None, _) => UpgradeAction::Fresh,
        (Some(prev_hash), Some(disk)) => {
            let now = sha256_hex(disk.as_bytes());
            if &now == prev_hash {
                UpgradeAction::SafeOverwrite
            } else {
                UpgradeAction::UserModified
            }
        }
        (Some(_), None) => UpgradeAction::Fresh,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_lookup() {
        let mut m = Manifest::new("0.1.0");
        m.record(&PathBuf::from("AGENTS.md"), "hello");
        assert_eq!(m.installed.len(), 1);
        assert!(m.installed.contains_key("AGENTS.md"));
    }

    #[test]
    fn classify_fresh_when_not_recorded() {
        let m = Manifest::new("0.1.0");
        assert_eq!(
            classify_upgrade(&m, &PathBuf::from("X.md"), None),
            UpgradeAction::Fresh
        );
        assert_eq!(
            classify_upgrade(&m, &PathBuf::from("X.md"), Some("anything")),
            UpgradeAction::Fresh
        );
    }

    #[test]
    fn classify_safe_when_unchanged() {
        let mut m = Manifest::new("0.1.0");
        m.record(&PathBuf::from("AGENTS.md"), "hello");
        assert_eq!(
            classify_upgrade(&m, &PathBuf::from("AGENTS.md"), Some("hello")),
            UpgradeAction::SafeOverwrite
        );
    }

    #[test]
    fn classify_user_modified_when_changed() {
        let mut m = Manifest::new("0.1.0");
        m.record(&PathBuf::from("AGENTS.md"), "hello");
        assert_eq!(
            classify_upgrade(&m, &PathBuf::from("AGENTS.md"), Some("changed")),
            UpgradeAction::UserModified
        );
    }

    #[test]
    fn save_load_roundtrip() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut m = Manifest::new("0.1.0");
        m.record(&PathBuf::from("AGENTS.md"), "hello");
        m.save(tmp.path()).unwrap();
        let loaded = Manifest::load(tmp.path()).unwrap().unwrap();
        assert_eq!(loaded.version, "0.1.0");
        assert_eq!(loaded.installed.len(), 1);
    }
}
