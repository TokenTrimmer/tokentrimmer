//! Dataset handles — opaque references to EXTERNAL (referenced, never
//! resident) data.
//!
//! The MCP caller (an LLM agent, whose arguments are attacker-influencable)
//! can only **name** a dataset by [`HandleAlias`]. Paths, DSNs, and pools
//! exist solely inside [`DatasetHandle`], which is built from operator boot
//! config and is unreachable from tool arguments — see the type-level gate
//! below.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

/// Caller-suppliable name of an operator-registered dataset.
///
/// INVARIANT: `[a-z0-9_-]{1,64}` — enforced in the only constructor
/// (`TryFrom<&str>`); `Deserialize` delegates to it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HandleAlias(String);

impl HandleAlias {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<&str> for HandleAlias {
    type Error = String;
    fn try_from(s: &str) -> Result<Self, Self::Error> {
        if s.is_empty() || s.len() > 64 {
            return Err(format!(
                "dataset alias must be 1..=64 bytes (got {})",
                s.len()
            ));
        }
        if !s
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            return Err("dataset alias must match [a-z0-9_-]{1,64}".into());
        }
        Ok(Self(s.to_owned()))
    }
}

impl<'de> Deserialize<'de> for HandleAlias {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::try_from(s.as_str()).map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for HandleAlias {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// File formats supported by the bounded aggregation executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileFormat {
    Csv,
    Jsonl,
}

impl FileFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Jsonl => "jsonl",
        }
    }
}

/// Where a dataset's bytes actually live. Private to the crate: callers can
/// never construct or observe one.
pub(crate) enum DataSource {
    /// A file under the operator-allowlisted `files.root`. `path` is the
    /// boot-time canonicalized location; `root` is kept so the executor can
    /// re-canonicalize and re-check containment at every call (symlink-swap
    /// defense).
    File {
        root: PathBuf,
        path: PathBuf,
        format: FileFormat,
    },
    /// A Postgres pool built at boot from an operator `dsn_env` env var.
    /// The DSN itself is never stored.
    Postgres { pool: sqlx::PgPool },
}

/// Opaque reference to EXTERNAL (referenced, never resident) data.
///
/// TYPE-LEVEL GATE: `DatasetHandle`
///   * does NOT implement [`serde::Deserialize`] (pinned by the
///     `compile_fail` doctest below),
///   * has all-private fields and NO public constructor,
///   * is obtainable ONLY via [`HandleRegistry::resolve`], whose contents
///     come from operator boot config — never from tool arguments.
///
/// Caller JSON can therefore only *select* data by alias; it can never
/// *carry* data into the executor. Inline-offload (paying tokens for rows
/// that are already resident in context, then paying again for the tool
/// round-trip) is unrepresentable at this boundary.
///
/// The invariant is pinned so it cannot regress silently:
///
/// ```compile_fail
/// fn requires_deserialize<T: serde::de::DeserializeOwned>() {}
/// requires_deserialize::<tt_mcp::query::DatasetHandle>();
/// ```
pub struct DatasetHandle {
    alias: HandleAlias,
    description: Option<String>,
    source: DataSource,
}

impl DatasetHandle {
    /// Crate-private constructor: only the operator-config loader
    /// (`QueryConfig::build_registry`) and in-crate tests build handles.
    pub(crate) fn new(
        alias: HandleAlias,
        description: Option<String>,
        source: DataSource,
    ) -> Self {
        Self {
            alias,
            description,
            source,
        }
    }

    pub fn alias(&self) -> &HandleAlias {
        &self.alias
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// `"file"` or `"postgres"` — safe for caller-visible output.
    pub fn kind_str(&self) -> &'static str {
        match self.source {
            DataSource::File { .. } => "file",
            DataSource::Postgres { .. } => "postgres",
        }
    }

    /// File format, when this is a file handle.
    pub fn file_format(&self) -> Option<FileFormat> {
        match self.source {
            DataSource::File { format, .. } => Some(format),
            DataSource::Postgres { .. } => None,
        }
    }

    pub(crate) fn source(&self) -> &DataSource {
        &self.source
    }
}

/// `Debug` is hand-written to redact the source: a derived impl would leak
/// the file path into logs/errors. Only the alias and kind are printed.
impl std::fmt::Debug for DatasetHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatasetHandle")
            .field("alias", &self.alias)
            .field("kind", &self.kind_str())
            .finish_non_exhaustive()
    }
}

/// Boot-built map of alias → handle. The ONLY source of [`DatasetHandle`]s.
pub struct HandleRegistry {
    handles: HashMap<HandleAlias, DatasetHandle>,
}

impl HandleRegistry {
    /// Crate-private: registries are built exclusively by
    /// `QueryConfig::build_registry` (and in-crate tests).
    pub(crate) fn from_handles(handles: Vec<DatasetHandle>) -> Self {
        Self {
            handles: handles
                .into_iter()
                .map(|h| (h.alias.clone(), h))
                .collect(),
        }
    }

    /// The ONLY way to obtain a [`DatasetHandle`].
    pub fn resolve(&self, alias: &HandleAlias) -> Option<&DatasetHandle> {
        self.handles.get(alias)
    }

    /// Iterate registered handles (for `list_datasets`), sorted by alias so
    /// output is deterministic.
    pub fn iter_sorted(&self) -> Vec<&DatasetHandle> {
        let mut v: Vec<&DatasetHandle> = self.handles.values().collect();
        v.sort_by(|a, b| a.alias.as_str().cmp(b.alias.as_str()));
        v
    }

    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alias_accepts_valid_charset() {
        for ok in ["orders", "orders-2025_q1", "a", &"x".repeat(64)] {
            assert!(HandleAlias::try_from(ok).is_ok(), "{ok} should be valid");
        }
    }

    #[test]
    fn alias_rejects_bad_charset_empty_and_oversize() {
        for bad in [
            "",
            "Orders",          // uppercase
            "or ders",         // space
            "../etc",          // traversal chars
            "a.b",             // dot
            "a/b",             // slash
            "naïve",           // non-ascii
            &"x".repeat(65),   // too long
        ] {
            assert!(
                HandleAlias::try_from(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn alias_deserialize_delegates_to_ctor() {
        assert!(serde_json::from_value::<HandleAlias>(serde_json::json!("OK")).is_err());
        assert!(serde_json::from_value::<HandleAlias>(serde_json::json!("ok")).is_ok());
    }

    fn file_handle(alias: &str) -> DatasetHandle {
        DatasetHandle::new(
            HandleAlias::try_from(alias).unwrap(),
            Some("test data".into()),
            DataSource::File {
                root: PathBuf::from("/secret/root"),
                path: PathBuf::from("/secret/root/orders.csv"),
                format: FileFormat::Csv,
            },
        )
    }

    #[test]
    fn registry_resolves_known_and_rejects_unknown() {
        let reg = HandleRegistry::from_handles(vec![file_handle("orders")]);
        assert!(reg.resolve(&HandleAlias::try_from("orders").unwrap()).is_some());
        assert!(reg.resolve(&HandleAlias::try_from("nope").unwrap()).is_none());
    }

    /// Debug output must not leak the file path (it ends up in logs/errors).
    #[test]
    fn handle_debug_redacts_source_path() {
        let dbg = format!("{:?}", file_handle("orders"));
        assert!(!dbg.contains("/secret"), "Debug leaked the path: {dbg}");
        assert!(dbg.contains("orders"));
        assert!(dbg.contains("file"));
    }

    #[test]
    fn iter_sorted_is_alias_ordered() {
        let reg = HandleRegistry::from_handles(vec![file_handle("zebra"), file_handle("apple")]);
        let aliases: Vec<&str> = reg
            .iter_sorted()
            .iter()
            .map(|h| h.alias().as_str())
            .collect();
        assert_eq!(aliases, vec!["apple", "zebra"]);
    }
}
