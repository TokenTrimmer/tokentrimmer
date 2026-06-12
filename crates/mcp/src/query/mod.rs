//! Query-offload: execute bounded queries against operator-registered
//! datasets so the DATA stays external and only the COMPUTED RESULT enters
//! model context.
//!
//! # Why
//!
//! A model asked to analyze data pays input tokens for every row it reads.
//! If the data stays EXTERNAL (a file or DB this module queries) and only
//! the computed aggregate enters context, the data token class is deleted.
//! The trap is INLINE-OFFLOAD — running compute over data already resident
//! in the prompt — which is worse than doing nothing (you pay the rows AND
//! the tool round-trip). The gate against it is **type-level**, three
//! layers:
//!
//! 1. [`handle::DatasetHandle`] implements no `Deserialize`, has private
//!    fields and no public constructor — obtainable only via
//!    [`handle::HandleRegistry::resolve`], whose contents come from operator
//!    boot config. Pinned by a `compile_fail` doctest.
//! 2. Every caller string is a bounded newtype ([`bounded::BoundedQuery`]
//!    ≤ 4096 B, [`bounded::BoundedStr`] fields in
//!    [`spec::AggregationSpec`]).
//! 3. An outer [`bounded::MAX_ARGS_BYTES`] gate on the serialized arguments
//!    object rejects any smuggled blob before parsing or I/O.
//!
//! # Security model (threat: the caller is an LLM agent; its arguments are
//! attacker-influencable via prompt injection)
//!
//! * **Alias indirection**: no tool parameter is a path, URL, or DSN —
//!   callers can only *name* operator-registered dataset aliases. Path
//!   traversal and SSRF are unrepresentable at the tool boundary.
//!   `tt_shared::url_guard` is not invoked in this MVP because no
//!   caller-reachable URL exists; any FUTURE handle kind that accepts a URL
//!   (e.g. `kind = "https"`) MUST validate it with
//!   `tt_shared::url_guard::validate_provider_url(raw, false)`.
//! * **File handles**: paths are operator-config-only, relative to a
//!   required allowlisted root, canonicalized at boot AND re-canonicalized
//!   at every call with a containment check (symlink-swap defense; the
//!   residual open-vs-check TOCTOU on platforms without O_NOFOLLOW-dir
//!   semantics is accepted residual risk).
//! * **Postgres handles**: sqlparser exactly-one-SELECT allowlist + escape
//!   function denylist → `READ ONLY` transaction + `SET LOCAL
//!   statement_timeout` → row/byte caps while streaming → always ROLLBACK.
//!   The REAL boundary is the operator-granted role behind `dsn_env`: use a
//!   dedicated read-only role. Layers above are defense in depth.
//! * **No arbitrary code execution** — explicit non-goal. This server runs
//!   on an operator's machine; an arbitrary code/shell primitive reachable
//!   from model-controlled tool calls is an unacceptable escalation, and
//!   sandboxing it properly is a project, not a feature. The savings case
//!   holds with query-offload alone.
//! * **Result caps are hard errors**, never silent truncation: an over-cap
//!   result tells the model to aggregate tighter. This is both the honesty
//!   guarantee and the anti-exfil-into-context guarantee.
//!
//! # Data residency
//!
//! Execution is **local by design** — dataset rows never leave the
//! operator's machine. Only the computed result enters model context, and
//! *that* result is then sent to the model provider like any other tool
//! result: size `max_result_rows`/`max_result_bytes` with the understanding
//! that aggregates DO leave the machine. A hosted/provider-side
//! code-execution variant would route raw data through provider
//! infrastructure and is **not ZDR-eligible** — recorded as a hard
//! constraint on any future hosted variant.
//!
//! # Gating
//!
//! There is no boolean flag: the operator config file
//! (`tt mcp --query-config` / `TT_MCP_QUERY_CONFIG`) is itself the explicit
//! opt-in. No config → the tools are never registered (`tools/list` omits
//! them; `tools/call` returns `MethodNotFound`). A supplied-but-invalid
//! config fails boot (fail closed).

pub mod bounded;
pub mod cache;
pub mod file_exec;
pub mod handle;
pub mod ledger;
pub mod pg_exec;
pub mod spec;

use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::error::McpError;
pub use handle::{DatasetHandle, FileFormat, HandleAlias, HandleRegistry};
pub use spec::{AggregationSpec, QuerySpec, RunQueryArgs};

use handle::DataSource;

// ── Limits ───────────────────────────────────────────────────────────────

/// Hard ceiling for `statement_timeout_ms` (operator-raisable up to here).
pub const MAX_STATEMENT_TIMEOUT_MS: u64 = 60_000;
/// Hard ceiling for `max_result_rows`.
pub const MAX_RESULT_ROWS_CEILING: usize = 10_000;
/// Hard ceiling for `max_result_bytes`.
pub const MAX_RESULT_BYTES_CEILING: usize = 1_048_576;

/// Execution limits, operator-tunable below hard ceilings.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct QueryLimits {
    /// Postgres `SET LOCAL statement_timeout` (ms).
    pub statement_timeout_ms: u64,
    /// Max result rows before a hard `result_too_large` error.
    pub max_result_rows: usize,
    /// Max serialized result bytes before a hard `result_too_large` error.
    pub max_result_bytes: usize,
}

impl Default for QueryLimits {
    fn default() -> Self {
        Self {
            statement_timeout_ms: 5_000,
            max_result_rows: 100,
            max_result_bytes: 65_536,
        }
    }
}

impl QueryLimits {
    fn validate(&self) -> Result<(), String> {
        if self.statement_timeout_ms == 0 || self.statement_timeout_ms > MAX_STATEMENT_TIMEOUT_MS {
            return Err(format!(
                "limits.statement_timeout_ms must be 1..={MAX_STATEMENT_TIMEOUT_MS}"
            ));
        }
        if self.max_result_rows == 0 || self.max_result_rows > MAX_RESULT_ROWS_CEILING {
            return Err(format!(
                "limits.max_result_rows must be 1..={MAX_RESULT_ROWS_CEILING}"
            ));
        }
        if self.max_result_bytes == 0 || self.max_result_bytes > MAX_RESULT_BYTES_CEILING {
            return Err(format!(
                "limits.max_result_bytes must be 1..={MAX_RESULT_BYTES_CEILING}"
            ));
        }
        Ok(())
    }
}

// ── Operator config ──────────────────────────────────────────────────────

/// Errors loading/validating the operator query config. All fail boot.
#[derive(Debug, Error)]
pub enum QueryConfigError {
    #[error("query config io: {0}")]
    Io(#[from] std::io::Error),
    #[error("query config parse: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("query config invalid: {0}")]
    Invalid(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    limits: QueryLimits,
    files: Option<RawFiles>,
    #[serde(default, rename = "dataset")]
    datasets: Vec<RawDataset>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFiles {
    root: PathBuf,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum RawDataset {
    File {
        alias: String,
        path: PathBuf,
        format: FileFormat,
        #[serde(default)]
        description: Option<String>,
    },
    Postgres {
        alias: String,
        /// Name of the env var holding the DSN — credentials never live in
        /// the TOML (it may be committed).
        dsn_env: String,
        #[serde(default)]
        description: Option<String>,
    },
}

enum DatasetDecl {
    File {
        alias: HandleAlias,
        path: PathBuf,
        format: FileFormat,
        description: Option<String>,
    },
    Postgres {
        alias: HandleAlias,
        /// DSN value resolved from the env at load time. Never logged.
        dsn: String,
        description: Option<String>,
    },
}

/// Validated operator query config. Loading it is the explicit opt-in that
/// enables the query tools; any validation failure fails boot.
pub struct QueryConfig {
    limits: QueryLimits,
    files_root: Option<PathBuf>,
    datasets: Vec<DatasetDecl>,
}

/// Hand-written `Debug`: a derived impl would print resolved DSNs
/// (credentials). Only structure-level facts are shown.
impl std::fmt::Debug for QueryConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryConfig")
            .field("limits", &self.limits)
            .field("datasets", &self.datasets.len())
            .finish_non_exhaustive()
    }
}

impl QueryConfig {
    /// Load + validate the operator TOML at `path`. Any failure is a boot
    /// error (fail closed). DSN env vars are resolved here so a missing one
    /// fails boot, not first call.
    pub fn load(path: &Path) -> Result<Self, QueryConfigError> {
        let text = std::fs::read_to_string(path)?;
        Self::from_toml_str(&text, &|name| std::env::var(name).ok())
    }

    /// Parse + validate from TOML text, resolving DSN env vars through
    /// `env` (a seam so tests don't mutate process env).
    pub fn from_toml_str(
        text: &str,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, QueryConfigError> {
        let raw: RawConfig = toml::from_str(text)?;
        let invalid = |m: String| QueryConfigError::Invalid(m);

        raw.limits.validate().map_err(invalid)?;

        if raw.datasets.is_empty() {
            return Err(invalid("config declares no [[dataset]] entries".into()));
        }

        let files_root = raw.files.map(|f| f.root);
        if let Some(root) = &files_root {
            if !root.is_absolute() {
                return Err(invalid(format!(
                    "files.root must be an absolute path (got {})",
                    root.display()
                )));
            }
        }

        let mut seen = std::collections::HashSet::new();
        let mut datasets = Vec::with_capacity(raw.datasets.len());
        for ds in raw.datasets {
            let decl = match ds {
                RawDataset::File {
                    alias,
                    path,
                    format,
                    description,
                } => {
                    let alias = HandleAlias::try_from(alias.as_str()).map_err(invalid)?;
                    if files_root.is_none() {
                        return Err(invalid(format!(
                            "dataset `{alias}` is a file dataset but files.root is not set \
                             (files.root is REQUIRED and has no default)"
                        )));
                    }
                    if path.is_absolute() {
                        return Err(invalid(format!(
                            "dataset `{alias}`: path must be RELATIVE to files.root"
                        )));
                    }
                    if path
                        .components()
                        .any(|c| !matches!(c, Component::Normal(_)))
                    {
                        return Err(invalid(format!(
                            "dataset `{alias}`: path must not contain `..` or other \
                             non-normal components"
                        )));
                    }
                    DatasetDecl::File {
                        alias,
                        path,
                        format,
                        description,
                    }
                }
                RawDataset::Postgres {
                    alias,
                    dsn_env,
                    description,
                } => {
                    let alias = HandleAlias::try_from(alias.as_str()).map_err(invalid)?;
                    let dsn = env(&dsn_env).filter(|v| !v.is_empty()).ok_or_else(|| {
                        QueryConfigError::Invalid(format!(
                            "dataset `{alias}`: dsn_env `{dsn_env}` is not set in the \
                             environment (fail closed)"
                        ))
                    })?;
                    DatasetDecl::Postgres {
                        alias,
                        dsn,
                        description,
                    }
                }
            };
            let alias = match &decl {
                DatasetDecl::File { alias, .. } | DatasetDecl::Postgres { alias, .. } => {
                    alias.clone()
                }
            };
            if !seen.insert(alias.clone()) {
                return Err(invalid(format!("duplicate dataset alias `{alias}`")));
            }
            datasets.push(decl);
        }

        Ok(Self {
            limits: raw.limits,
            files_root,
            datasets,
        })
    }

    pub fn limits(&self) -> QueryLimits {
        self.limits
    }

    /// Build the boot-time [`HandleRegistry`]: canonicalize the root and
    /// every file path (with a containment check), and connect every
    /// Postgres pool. Any failure is a boot error (fail closed).
    pub async fn build_registry(&self) -> Result<HandleRegistry, QueryConfigError> {
        let root = match &self.files_root {
            Some(r) => Some(r.canonicalize().map_err(|e| {
                QueryConfigError::Invalid(format!(
                    "files.root {} cannot be canonicalized: {e}",
                    r.display()
                ))
            })?),
            None => None,
        };

        let mut handles = Vec::with_capacity(self.datasets.len());
        for decl in &self.datasets {
            match decl {
                DatasetDecl::File {
                    alias,
                    path,
                    format,
                    description,
                } => {
                    let root = root.as_ref().expect("validated: file dataset has a root");
                    let joined = root.join(path);
                    let canonical = joined.canonicalize().map_err(|e| {
                        QueryConfigError::Invalid(format!(
                            "dataset `{alias}`: file cannot be canonicalized: {e}"
                        ))
                    })?;
                    if !canonical.starts_with(root) {
                        return Err(QueryConfigError::Invalid(format!(
                            "dataset `{alias}`: file resolves outside files.root"
                        )));
                    }
                    handles.push(DatasetHandle::new(
                        alias.clone(),
                        description.clone(),
                        DataSource::File {
                            root: root.clone(),
                            path: canonical,
                            format: *format,
                        },
                    ));
                }
                DatasetDecl::Postgres {
                    alias,
                    dsn,
                    description,
                } => {
                    let pool = sqlx::postgres::PgPoolOptions::new()
                        .max_connections(2)
                        .connect(dsn)
                        .await
                        .map_err(|e| {
                            QueryConfigError::Invalid(format!(
                                "dataset `{alias}`: postgres connect failed: {e}"
                            ))
                        })?;
                    handles.push(DatasetHandle::new(
                        alias.clone(),
                        description.clone(),
                        DataSource::Postgres { pool },
                    ));
                }
            }
        }
        Ok(HandleRegistry::from_handles(handles))
    }
}

// ── Execution seam ───────────────────────────────────────────────────────

/// A query that passed kind-matching + (for SQL) the sqlparser gate. The
/// executors only accept this type, so unvalidated SQL is unrepresentable at
/// the execution seam.
#[derive(Debug, Clone)]
pub(crate) enum ValidatedQuery {
    Sql(pg_exec::ValidatedSql),
    Aggregate(AggregationSpec),
}

/// Validate `spec` against `handle`'s kind (SQL ↔ postgres, aggregate ↔
/// file) and run the SQL allowlist gate. The only producer of
/// [`ValidatedQuery`].
pub(crate) fn validate(
    handle: &DatasetHandle,
    spec: &QuerySpec,
) -> Result<ValidatedQuery, McpError> {
    match (handle.source(), spec) {
        (DataSource::Postgres { .. }, QuerySpec::Sql(q)) => {
            Ok(ValidatedQuery::Sql(pg_exec::validate_sql(q)?))
        }
        (DataSource::File { .. }, QuerySpec::Aggregate(a)) => {
            Ok(ValidatedQuery::Aggregate(a.clone()))
        }
        (DataSource::File { .. }, QuerySpec::Sql(_)) => Err(McpError::InvalidParams(
            "this dataset is a file dataset: use `aggregate`, not `query` (SQL)".into(),
        )),
        (DataSource::Postgres { .. }, QuerySpec::Aggregate(_)) => Err(McpError::InvalidParams(
            "this dataset is a postgres dataset: use `query` (SQL), not `aggregate`".into(),
        )),
    }
}

/// Computed result of a query execution. Note what is ABSENT from the
/// executor inputs producing this: no parameter carries caller data bytes —
/// there is no path from tool params to resident data.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct QueryOutcome {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    /// Malformed input rows counted and surfaced — never silently dropped.
    pub skipped_rows: u64,
    /// Rows read from the source. Exact for file scans; `None` for Postgres
    /// in the MVP (no honest number exists without pg_stat_statements — we
    /// don't fabricate one).
    pub rows_scanned: Option<u64>,
}

impl QueryOutcome {
    /// blake3 hash of the canonical serialization, for verify-mode
    /// comparison surfacing.
    pub fn result_hash(&self) -> String {
        let bytes = serde_json::to_vec(self).expect("QueryOutcome serializes");
        format!("blake3:{}", blake3::hash(&bytes).to_hex())
    }
}

/// Execute a validated query against a referenced dataset. The executor
/// seam: `handle` is resolved from operator config, `query` is bounded +
/// parser-validated — neither can carry caller data.
pub(crate) async fn execute(
    handle: &DatasetHandle,
    query: &ValidatedQuery,
    limits: &QueryLimits,
) -> Result<QueryOutcome, McpError> {
    match (handle.source(), query) {
        (DataSource::File { root, path, format }, ValidatedQuery::Aggregate(spec)) => {
            file_exec::run(root, path, *format, spec, limits)
        }
        (DataSource::Postgres { pool }, ValidatedQuery::Sql(sql)) => {
            pg_exec::run(pool, sql, limits).await
        }
        _ => Err(McpError::Internal(
            "query/handle kind mismatch past validation".into(),
        )),
    }
}

/// Hard over-cap error: instructs tighter aggregation, never truncates.
pub(crate) fn result_too_large(what: &str, cap: usize) -> McpError {
    McpError::InvalidParams(format!(
        "result_too_large: result exceeds {what} cap ({cap}); tighten the aggregation \
         (more selective `where`, fewer groups, or aggregate further) — results are \
         never truncated"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn with_env(name: &str) -> impl Fn(&str) -> Option<String> + '_ {
        move |n: &str| (n == name).then(|| "postgres://ro@localhost/db".to_string())
    }

    const FILE_DS: &str = r#"
        [files]
        root = "/abs/data"

        [[dataset]]
        alias = "orders"
        kind = "file"
        path = "orders.csv"
        format = "csv"
    "#;

    #[test]
    fn valid_file_config_loads() {
        let cfg = QueryConfig::from_toml_str(FILE_DS, &no_env).unwrap();
        assert_eq!(cfg.limits().max_result_rows, 100, "defaults apply");
    }

    #[test]
    fn absolute_dataset_path_rejected() {
        let toml = r#"
            [files]
            root = "/abs/data"
            [[dataset]]
            alias = "orders"
            kind = "file"
            path = "/etc/passwd"
            format = "csv"
        "#;
        let err = QueryConfig::from_toml_str(toml, &no_env).unwrap_err();
        assert!(err.to_string().contains("RELATIVE"), "{err}");
    }

    #[test]
    fn parent_traversal_rejected() {
        let toml = r#"
            [files]
            root = "/abs/data"
            [[dataset]]
            alias = "orders"
            kind = "file"
            path = "../outside.csv"
            format = "csv"
        "#;
        let err = QueryConfig::from_toml_str(toml, &no_env).unwrap_err();
        assert!(err.to_string().contains(".."), "{err}");
    }

    #[test]
    fn file_dataset_without_root_rejected() {
        let toml = r#"
            [[dataset]]
            alias = "orders"
            kind = "file"
            path = "orders.csv"
            format = "csv"
        "#;
        let err = QueryConfig::from_toml_str(toml, &no_env).unwrap_err();
        assert!(err.to_string().contains("files.root"), "{err}");
    }

    #[test]
    fn relative_root_rejected() {
        let toml = r#"
            [files]
            root = "data"
            [[dataset]]
            alias = "orders"
            kind = "file"
            path = "orders.csv"
            format = "csv"
        "#;
        let err = QueryConfig::from_toml_str(toml, &no_env).unwrap_err();
        assert!(err.to_string().contains("absolute"), "{err}");
    }

    #[test]
    fn duplicate_alias_rejected() {
        let toml = r#"
            [files]
            root = "/abs/data"
            [[dataset]]
            alias = "orders"
            kind = "file"
            path = "a.csv"
            format = "csv"
            [[dataset]]
            alias = "orders"
            kind = "file"
            path = "b.csv"
            format = "csv"
        "#;
        let err = QueryConfig::from_toml_str(toml, &no_env).unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn unset_dsn_env_fails_boot() {
        let toml = r#"
            [[dataset]]
            alias = "warehouse"
            kind = "postgres"
            dsn_env = "TT_TEST_UNSET_DSN"
        "#;
        let err = QueryConfig::from_toml_str(toml, &no_env).unwrap_err();
        assert!(err.to_string().contains("TT_TEST_UNSET_DSN"), "{err}");
        // ... and the DSN VALUE must never appear in any error/log path:
        // it is read but stored privately; with the env set the load passes.
        assert!(QueryConfig::from_toml_str(toml, &with_env("TT_TEST_UNSET_DSN")).is_ok());
    }

    #[test]
    fn limits_above_hard_ceilings_rejected() {
        for (field, bad) in [
            ("statement_timeout_ms", "60001"),
            ("max_result_rows", "10001"),
            ("max_result_bytes", "1048577"),
        ] {
            let toml = format!(
                r#"
                [limits]
                {field} = {bad}
                [files]
                root = "/abs/data"
                [[dataset]]
                alias = "orders"
                kind = "file"
                path = "a.csv"
                format = "csv"
            "#
            );
            let err = QueryConfig::from_toml_str(&toml, &no_env).unwrap_err();
            assert!(err.to_string().contains(field), "{field}: {err}");
        }
    }

    #[test]
    fn empty_config_rejected() {
        let err = QueryConfig::from_toml_str("", &no_env).unwrap_err();
        assert!(err.to_string().contains("no [[dataset]]"), "{err}");
    }

    #[test]
    fn bad_alias_charset_rejected() {
        let toml = r#"
            [files]
            root = "/abs/data"
            [[dataset]]
            alias = "Orders!"
            kind = "file"
            path = "a.csv"
            format = "csv"
        "#;
        assert!(QueryConfig::from_toml_str(toml, &no_env).is_err());
    }
}
