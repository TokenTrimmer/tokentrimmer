//! Credential + config resolution for the `tt` CLI.
//!
//! Precedence (both key and base URL): flag > env > ~/.tokentrimmer file >
//! built-in default. The `resolve_*` functions are pure (explicit inputs) so
//! tests never read real env / $HOME; `ResolvedContext::load` is the thin
//! real-world wrapper.

pub mod store;

use tt_shared::context::SecretString;

/// Built-in default gateway base URL (the canonical custom domain; SDKs use it).
pub const DEFAULT_BASE_URL: &str = "https://api.tokentrimmer.com";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Flag,
    Env,
    File,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseSource {
    Flag,
    Env,
    File,
    Default,
}

impl std::fmt::Display for KeySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            KeySource::Flag => "--tt-api-key",
            KeySource::Env => "TT_API_KEY env",
            KeySource::File => "credentials.toml",
            KeySource::None => "none",
        })
    }
}

impl std::fmt::Display for BaseSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            BaseSource::Flag => "--tt-api-base",
            BaseSource::Env => "TT_API_BASE env",
            BaseSource::File => "config.toml",
            BaseSource::Default => "default",
        })
    }
}

/// The resolved client context every gateway-touching command consumes.
pub struct ResolvedContext {
    pub api_key: Option<SecretString>,
    pub key_source: KeySource,
    pub base_url: String,
    pub base_source: BaseSource,
}

fn nonempty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

/// Pure: pick the API key by precedence flag > env > file.
pub fn resolve_key(
    flag: Option<String>,
    env: Option<String>,
    file: Option<String>,
) -> (Option<SecretString>, KeySource) {
    if let Some(k) = nonempty(flag) {
        return (Some(SecretString::new(k)), KeySource::Flag);
    }
    if let Some(k) = nonempty(env) {
        return (Some(SecretString::new(k)), KeySource::Env);
    }
    if let Some(k) = nonempty(file) {
        return (Some(SecretString::new(k)), KeySource::File);
    }
    (None, KeySource::None)
}

/// Pure: pick the base URL by precedence flag > env > file > built-in default.
pub fn resolve_base(
    flag: Option<String>,
    env: Option<String>,
    file: Option<String>,
) -> (String, BaseSource) {
    if let Some(b) = nonempty(flag) {
        return (b, BaseSource::Flag);
    }
    if let Some(b) = nonempty(env) {
        return (b, BaseSource::Env);
    }
    if let Some(b) = nonempty(file) {
        return (b, BaseSource::File);
    }
    (DEFAULT_BASE_URL.to_string(), BaseSource::Default)
}

/// Mask a key for display: keep the `tt_live_`/`tt_test_` prefix + a few chars.
pub fn mask_key(key: &str) -> String {
    let n = key.len().min(12);
    format!("{}…", &key[..n])
}

impl ResolvedContext {
    /// Resolve from CLI flags + real env (`TT_API_KEY`/`TT_API_BASE`) + the
    /// `~/.tokentrimmer/` files. Errors only if a stored file is corrupt.
    pub fn load(flag_key: Option<String>, flag_base: Option<String>) -> anyhow::Result<Self> {
        let dir = store::config_dir();
        let file_key = store::load_credentials(&dir)?;
        let file_base = store::load_config(&dir)?;
        let (api_key, key_source) =
            resolve_key(flag_key, std::env::var("TT_API_KEY").ok(), file_key);
        let (base_url, base_source) =
            resolve_base(flag_base, std::env::var("TT_API_BASE").ok(), file_base);
        Ok(Self {
            api_key,
            key_source,
            base_url,
            base_source,
        })
    }

    /// The API key as a plain `String` for the (String-typed) consumers.
    pub fn api_key_string(&self) -> Option<String> {
        self.api_key.as_ref().map(|s| s.expose().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_precedence_flag_env_file_none() {
        let (k, s) = resolve_key(
            Some("flagk".into()),
            Some("envk".into()),
            Some("filek".into()),
        );
        assert_eq!(k.unwrap().expose(), "flagk");
        assert_eq!(s, KeySource::Flag);

        let (k, s) = resolve_key(None, Some("envk".into()), Some("filek".into()));
        assert_eq!(k.unwrap().expose(), "envk");
        assert_eq!(s, KeySource::Env);

        let (k, s) = resolve_key(None, None, Some("filek".into()));
        assert_eq!(k.unwrap().expose(), "filek");
        assert_eq!(s, KeySource::File);

        let (k, s) = resolve_key(None, None, None);
        assert!(k.is_none());
        assert_eq!(s, KeySource::None);
    }

    #[test]
    fn blanks_are_treated_as_absent() {
        let (k, s) = resolve_key(Some("   ".into()), Some("envk".into()), None);
        assert_eq!(k.unwrap().expose(), "envk");
        assert_eq!(s, KeySource::Env);
    }

    #[test]
    fn base_precedence_and_default() {
        let (b, s) = resolve_base(
            Some("https://flag".into()),
            Some("https://env".into()),
            None,
        );
        assert_eq!(b, "https://flag");
        assert_eq!(s, BaseSource::Flag);

        let (b, s) = resolve_base(None, None, Some("https://file".into()));
        assert_eq!(b, "https://file");
        assert_eq!(s, BaseSource::File);

        let (b, s) = resolve_base(None, None, None);
        assert_eq!(b, DEFAULT_BASE_URL);
        assert_eq!(b, "https://api.tokentrimmer.com");
        assert_eq!(s, BaseSource::Default);
    }

    #[test]
    fn mask_hides_the_secret() {
        let masked = mask_key("tt_live_abcd1234efgh");
        assert_eq!(masked, "tt_live_abcd…");
        assert!(!masked.contains("1234efgh"));
    }
}
