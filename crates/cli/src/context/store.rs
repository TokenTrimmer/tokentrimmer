//! On-disk credential + config store at `~/.tokentrimmer/`.
//! `credentials.toml` holds the secret API key (0600 on unix); `config.toml`
//! holds non-secret settings (base_url). The directory is created 0700.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

/// `~/.tokentrimmer/` (falls back to `./.tokentrimmer` if HOME is unknown).
pub fn config_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".tokentrimmer"))
        .unwrap_or_else(|| PathBuf::from(".tokentrimmer"))
}

#[derive(Serialize, Deserialize, Default)]
struct CredentialsFile {
    api_key: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct ConfigFile {
    base_url: Option<String>,
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

fn ensure_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    set_mode(dir, 0o700).ok(); // best-effort; non-unix is a no-op
    Ok(())
}

/// Read the stored API key, or `None` if the file is absent / the key is blank.
/// Errors only on a present-but-unparseable file (never silently drop a key).
pub fn load_credentials(dir: &Path) -> anyhow::Result<Option<String>> {
    let p = dir.join("credentials.toml");
    if !p.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    let f: CredentialsFile =
        toml::from_str(&raw).with_context(|| format!("parse {}", p.display()))?;
    Ok(f.api_key.filter(|s| !s.trim().is_empty()))
}

/// Write the API key to `credentials.toml` (0600). Creates the dir (0700).
///
/// Atomic: the secret is written to a 0600 temp file in the same directory and
/// renamed into place, so it is never briefly readable at umask-default perms
/// (closes the old write-then-chmod TOCTOU). On non-unix the temp file carries
/// no perms (`set_mode` is a no-op) — the key is stored unprotected there, as
/// before.
pub fn save_credentials(dir: &Path, api_key: &str) -> anyhow::Result<()> {
    use std::io::Write as _;

    ensure_dir(dir)?;
    let p = dir.join("credentials.toml");
    let body = toml::to_string(&CredentialsFile {
        api_key: Some(api_key.to_string()),
    })?;

    // Temp file in the SAME dir so `persist` is a same-filesystem atomic rename.
    // tempfile creates it 0600 on unix; the explicit set_mode is a
    // version-independent guarantee. This chmod is HARD-failed on purpose (not
    // best-effort like the dir's 0700) — the temp's perms are security-load-
    // bearing, so don't relax this to `.ok()`.
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("create temp file in {}", dir.display()))?;
    set_mode(tmp.path(), 0o600).with_context(|| format!("chmod {}", tmp.path().display()))?;
    tmp.write_all(body.as_bytes())
        .with_context(|| format!("write {}", tmp.path().display()))?;
    tmp.persist(&p)
        .map_err(|e| e.error)
        .with_context(|| format!("persist {}", p.display()))?;
    Ok(())
}

/// Remove `credentials.toml`. Returns `true` if a file was removed.
pub fn delete_credentials(dir: &Path) -> anyhow::Result<bool> {
    let p = dir.join("credentials.toml");
    if p.exists() {
        std::fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Read the persisted base URL, or `None` if absent / blank. Errors on corrupt.
pub fn load_config(dir: &Path) -> anyhow::Result<Option<String>> {
    let p = dir.join("config.toml");
    if !p.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
    let f: ConfigFile = toml::from_str(&raw).with_context(|| format!("parse {}", p.display()))?;
    Ok(f.base_url.filter(|s| !s.trim().is_empty()))
}

/// Persist the base URL to `config.toml`. Creates the dir (0700).
pub fn save_config(dir: &Path, base_url: &str) -> anyhow::Result<()> {
    ensure_dir(dir)?;
    let p = dir.join("config.toml");
    let body = toml::to_string(&ConfigFile {
        base_url: Some(base_url.to_string()),
    })?;
    std::fs::write(&p, body).with_context(|| format!("write {}", p.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_round_trip_and_perms() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_credentials(dir.path()).unwrap(), None);
        save_credentials(dir.path(), "tt_live_abc123").unwrap();
        assert_eq!(
            load_credentials(dir.path()).unwrap(),
            Some("tt_live_abc123".to_string())
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("credentials.toml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn delete_credentials_reports_presence() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!delete_credentials(dir.path()).unwrap());
        save_credentials(dir.path(), "tt_test_x").unwrap();
        assert!(delete_credentials(dir.path()).unwrap());
        assert_eq!(load_credentials(dir.path()).unwrap(), None);
    }

    #[test]
    fn corrupt_credentials_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("credentials.toml"), "not = [valid").unwrap();
        assert!(load_credentials(dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn save_credentials_tightens_a_preexisting_loose_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("credentials.toml");
        // Simulate a credentials file left world/group-readable by an older,
        // pre-fix write (or a hostile pre-creation).
        std::fs::write(&p, "api_key = \"stale\"\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();

        save_credentials(dir.path(), "tt_live_new").unwrap();

        // The new key is stored and the file is now 0600 (the atomic replace
        // carries the temp file's restrictive perms).
        assert_eq!(
            load_credentials(dir.path()).unwrap(),
            Some("tt_live_new".to_string())
        );
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "credentials.toml must be 0600 after save");
    }

    #[test]
    fn config_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_config(dir.path()).unwrap(), None);
        save_config(dir.path(), "https://staging.example.com").unwrap();
        assert_eq!(
            load_config(dir.path()).unwrap(),
            Some("https://staging.example.com".to_string())
        );
    }
}
