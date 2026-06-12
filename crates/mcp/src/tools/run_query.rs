//! `run_query` — execute a bounded query against an operator-registered
//! dataset and return ONLY the computed result.
//!
//! Enforcement order in [`Tool::call`]:
//! 1. outer [`MAX_ARGS_BYTES`] gate on the serialized arguments — rejected
//!    before any dataset I/O or query parsing. (The JSON-RPC transport has
//!    necessarily parsed the request envelope by this point; the stdio
//!    transport caps each request line at 1 MiB and HTTP caps bodies at
//!    1 MiB, so that earlier parse is bounded too.)
//! 2. typed deserialization into `RunQueryArgs` (every field cap fires),
//! 3. alias resolution (unknown alias → InvalidParams, never echoing
//!    registry internals),
//! 4. kind/spec validation incl. the sqlparser gate.
//!
//! `verify: true` performs two fully fresh executions (cache bypassed),
//! compares them structurally and surfaces per-run blake3 result hashes —
//! a mismatch is reported, never swallowed (nondeterministic SQL like
//! `now()`/`random()`/un-`ORDER`ed `LIMIT` legitimately mismatches).
//! Every call appends one non-token ledger line (`unit: "execution"`,
//! `priced_as_tokens: false`); ledger write failures never fail the call.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::error::McpError;
use crate::protocol::ToolDef;
use crate::query::bounded::{MAX_ARGS_BYTES, MAX_QUERY_BYTES};
use crate::query::cache::{CacheKey, QueryCache};
use crate::query::handle::DataSource;
use crate::query::ledger::{ExecutionLedger, LedgerEntry};
use crate::query::{
    file_exec, DatasetHandle, HandleRegistry, QueryLimits, QueryOutcome, RunQueryArgs,
    ValidatedQuery,
};
use crate::tools::Tool;

/// Execution seam: the real executor delegates to [`crate::query::execute`];
/// tests inject counting/scripted fakes.
#[async_trait]
pub(crate) trait Executor: Send + Sync {
    async fn execute(
        &self,
        handle: &DatasetHandle,
        query: &ValidatedQuery,
        limits: &QueryLimits,
    ) -> Result<QueryOutcome, McpError>;
}

struct RealExecutor;

#[async_trait]
impl Executor for RealExecutor {
    async fn execute(
        &self,
        handle: &DatasetHandle,
        query: &ValidatedQuery,
        limits: &QueryLimits,
    ) -> Result<QueryOutcome, McpError> {
        crate::query::execute(handle, query, limits).await
    }
}

pub struct RunQueryTool {
    registry: Arc<HandleRegistry>,
    cache: Arc<QueryCache>,
    ledger: Arc<ExecutionLedger>,
    limits: QueryLimits,
    executor: Arc<dyn Executor>,
}

impl RunQueryTool {
    pub fn new(
        registry: Arc<HandleRegistry>,
        cache: Arc<QueryCache>,
        ledger: Arc<ExecutionLedger>,
        limits: QueryLimits,
    ) -> Self {
        Self {
            registry,
            cache,
            ledger,
            limits,
            executor: Arc::new(RealExecutor),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_executor(mut self, executor: Arc<dyn Executor>) -> Self {
        self.executor = executor;
        self
    }
}

/// Serialized byte size of the result rows (the over-cap unit and the
/// `result_bytes` meter use the same definition).
fn rows_bytes(outcome: &QueryOutcome) -> usize {
    serde_json::to_vec(&outcome.rows)
        .map(|b| b.len())
        .unwrap_or(0)
}

#[async_trait]
impl Tool for RunQueryTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "run_query",
            description: "Run a bounded query against an OPERATOR-REGISTERED dataset and return only the computed result (the data itself never enters context). Accepts ONLY dataset aliases from list_datasets — never paths, URLs, DSNs, or inline data; oversized arguments are rejected outright. Postgres datasets take `query` (a single SELECT, read-only, time-limited); file datasets take `aggregate` (count/sum/avg/min/max/count_distinct with bounded where/group_by). Results are capped: an over-cap result is an error asking for tighter aggregation, never a truncation. Set `verify: true` to re-execute and compare (catches nondeterminism/data drift).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "dataset": {
                        "type": "string",
                        "maxLength": 64,
                        "description": "Alias of an operator-registered dataset (see list_datasets)."
                    },
                    "query": {
                        "type": "string",
                        "maxLength": MAX_QUERY_BYTES,
                        "description": "Single SELECT statement (postgres datasets only). Mutually exclusive with `aggregate`."
                    },
                    "aggregate": {
                        "type": "object",
                        "description": "Aggregation spec for file datasets: { op: count|sum|avg|min|max|count_distinct, column?, where?: [{column, op: eq|ne|lt|lte|gt|gte, value}], group_by?: [..], limit? }. `limit` returns the FIRST N groups in group-key sort order — NOT top-N by aggregate value. An empty match still returns one row when group_by is absent (count 0, sum 0, avg/min/max null). Mutually exclusive with `query`.",
                        "properties": {
                            "op": { "type": "string", "enum": ["count", "sum", "avg", "min", "max", "count_distinct"] },
                            "column": { "type": "string", "maxLength": 128 },
                            "where": { "type": "array", "maxItems": 8 },
                            "group_by": { "type": "array", "maxItems": 4, "items": { "type": "string", "maxLength": 128 } },
                            "limit": { "type": "integer" }
                        },
                        "required": ["op"],
                        "additionalProperties": false
                    },
                    "verify": {
                        "type": "boolean",
                        "description": "Execute twice (cache bypassed) and compare results; mismatches are surfaced with per-run hashes."
                    }
                },
                "required": ["dataset"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, params: Value) -> Result<Value, McpError> {
        // (1) Outer gate BEFORE any dataset I/O or query parsing: the whole
        // arguments object is size-capped, so a smuggled data blob in any
        // field (even an unknown one) never reaches an executor. (The
        // transport has already parsed the JSON-RPC envelope — that parse is
        // bounded by the transport-level line/body caps, not by this gate.)
        let serialized_len = serde_json::to_string(&params)
            .map(|s| s.len())
            .unwrap_or(usize::MAX);
        if serialized_len > MAX_ARGS_BYTES {
            return Err(McpError::InvalidParams(format!(
                "arguments are {serialized_len} bytes; max {MAX_ARGS_BYTES} — run_query \
                 accepts dataset ALIASES and short queries, never inline data"
            )));
        }

        // (2) Typed deserialization: every bounded-field cap fires here.
        let args: RunQueryArgs =
            serde_json::from_value(params).map_err(|e| McpError::InvalidParams(e.to_string()))?;
        let (alias, spec, verify) = args.into_parts().map_err(McpError::InvalidParams)?;

        // (3) Alias resolution. The error names the caller's alias only —
        // never paths, roots, or DSN material.
        let handle = self.registry.resolve(&alias).ok_or_else(|| {
            McpError::InvalidParams(format!(
                "unknown dataset alias `{alias}` — call list_datasets for the \
                 operator-registered datasets"
            ))
        })?;

        // (4) Kind/spec match + SQL parser gate.
        let validated = crate::query::validate(handle, &spec)?;

        let started = Instant::now();

        // Cache key exists for FILE handles only; Postgres results are never
        // cached (no honest content fingerprint — see query::cache docs).
        let cache_key = match (handle.source(), &validated) {
            (DataSource::File { root, path, .. }, ValidatedQuery::Aggregate(agg)) => {
                let canonical = file_exec::checked_canonical(root, path)?;
                Some(CacheKey::for_file(&canonical, agg)?)
            }
            _ => None,
        };

        // Cache state for the meter. Postgres handles have NO cache at all —
        // reported as "none" (not "miss") so a ledger consumer can tell
        // "consulted and missed" from "uncacheable".
        let mut cache_state: &'static str = match (&cache_key, verify) {
            (None, _) => "none",
            (Some(_), true) => "bypass",
            (Some(_), false) => "miss",
        };

        let mut verified: Option<bool> = None;
        let mut runs: Option<Vec<Value>> = None;
        let mut warning: Option<&'static str> = None;

        let outcome = if verify {
            // Two fully fresh executions; cache bypassed for both.
            let first = self
                .executor
                .execute(handle, &validated, &self.limits)
                .await?;
            let second = self
                .executor
                .execute(handle, &validated, &self.limits)
                .await?;
            let matched = first == second;
            verified = Some(matched);
            runs = Some(vec![
                json!({ "result_hash": first.result_hash() }),
                json!({ "result_hash": second.result_hash() }),
            ]);
            if let Some(key) = &cache_key {
                if matched {
                    self.cache.insert(key.clone(), first.clone());
                } else {
                    self.cache.remove(key);
                }
            }
            if !matched {
                warning = Some(
                    "results differed between paired executions; query may be \
                     non-deterministic or data changed",
                );
            }
            first
        } else if let Some(key) = &cache_key {
            match self.cache.get(key) {
                Some(hit) => {
                    cache_state = "hit";
                    hit
                }
                None => {
                    let out = self
                        .executor
                        .execute(handle, &validated, &self.limits)
                        .await?;
                    self.cache.insert(key.clone(), out.clone());
                    out
                }
            }
        } else {
            self.executor
                .execute(handle, &validated, &self.limits)
                .await?
        };

        let wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let result_bytes = rows_bytes(&outcome);

        // On a cache hit NO rows are scanned (only a content-hash pass
        // proving the file is byte-identical) — repeating the original run's
        // count would over-state this call's scan work, so hits report null.
        let rows_scanned = if cache_state == "hit" {
            None
        } else {
            outcome.rows_scanned
        };

        // Non-token execution meter: one call == one line. Never fatal.
        self.ledger.append(&LedgerEntry {
            ts: chrono::Utc::now().to_rfc3339(),
            tool: "run_query",
            dataset: alias.as_str().to_owned(),
            kind: handle.kind_str(),
            wall_ms,
            rows_scanned,
            result_rows: outcome.rows.len(),
            result_bytes,
            cache: cache_state,
            verify,
            verified,
            unit: "execution",
            priced_as_tokens: false,
        });

        let mut envelope = json!({
            "dataset": alias.as_str(),
            "columns": outcome.columns,
            "rows": outcome.rows,
            "row_count": outcome.rows.len(),
            "skipped_rows": outcome.skipped_rows,
            "verified": verified,
            "execution": {
                "wall_ms": wall_ms,
                "rows_scanned": rows_scanned,
                "cache": cache_state,
                "unit": "execution",
                "priced_as_tokens": false
            }
        });
        if let Some(runs) = runs {
            envelope["runs"] = Value::Array(runs);
        }
        if let Some(w) = warning {
            envelope["warning"] = json!(w);
        }
        Ok(envelope)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;
    use crate::query::handle::HandleAlias;
    use crate::query::QueryConfig;

    /// Scripted executor: counts invocations; returns queued outcomes (or a
    /// default) so verify-mismatch paths are testable deterministically.
    struct FakeExecutor {
        calls: AtomicUsize,
        script: Mutex<Vec<QueryOutcome>>,
    }

    impl FakeExecutor {
        fn new(script: Vec<QueryOutcome>) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                script: Mutex::new(script),
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Executor for FakeExecutor {
        async fn execute(
            &self,
            _handle: &DatasetHandle,
            _query: &ValidatedQuery,
            _limits: &QueryLimits,
        ) -> Result<QueryOutcome, McpError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut script = self.script.lock().unwrap();
            Ok(if script.is_empty() {
                default_outcome()
            } else {
                script.remove(0)
            })
        }
    }

    fn default_outcome() -> QueryOutcome {
        QueryOutcome {
            columns: vec!["count".into()],
            rows: vec![vec![json!(4)]],
            skipped_rows: 0,
            rows_scanned: Some(4),
        }
    }

    /// Real registry built through the real config loader over a tempdir.
    fn file_registry(dir: &tempfile::TempDir) -> Arc<HandleRegistry> {
        std::fs::write(
            dir.path().join("orders.csv"),
            "region,amount\nemea,1\nus,2\n",
        )
        .unwrap();
        let toml = format!(
            r#"
            [files]
            root = "{}"
            [[dataset]]
            alias = "orders"
            kind = "file"
            path = "orders.csv"
            format = "csv"
            "#,
            dir.path().display()
        );
        let cfg = QueryConfig::from_toml_str(&toml, &|_| None).unwrap();
        let registry = futures::executor::block_on(cfg.build_registry()).unwrap();
        Arc::new(registry)
    }

    fn tool(
        registry: Arc<HandleRegistry>,
        ledger_path: &std::path::Path,
        exec: Arc<FakeExecutor>,
    ) -> RunQueryTool {
        RunQueryTool::new(
            registry,
            Arc::new(QueryCache::default()),
            Arc::new(ExecutionLedger::new(ledger_path)),
            QueryLimits::default(),
        )
        .with_executor(exec)
    }

    /// Outer MAX_ARGS_BYTES gate: oversized arguments → InvalidParams with
    /// ZERO executor invocations (no parse, no I/O).
    #[tokio::test]
    async fn oversized_arguments_rejected_before_any_execution() {
        let dir = tempfile::tempdir().unwrap();
        let exec = FakeExecutor::new(vec![]);
        let t = tool(
            file_registry(&dir),
            &dir.path().join("l.jsonl"),
            exec.clone(),
        );

        let blob = "x".repeat(MAX_ARGS_BYTES + 1);
        let err = t
            .call(json!({ "dataset": "orders", "aggregate": { "op": "count" }, "note": blob }))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::InvalidParams(_)));
        assert!(err.to_string().contains("never inline data"), "{err}");
        assert_eq!(exec.calls(), 0, "executor must never run");
    }

    /// Unknown alias → InvalidParams naming only the caller's alias — no
    /// path, root, or DSN material.
    #[tokio::test]
    async fn unknown_alias_error_does_not_leak_registry_internals() {
        let dir = tempfile::tempdir().unwrap();
        let exec = FakeExecutor::new(vec![]);
        let t = tool(
            file_registry(&dir),
            &dir.path().join("l.jsonl"),
            exec.clone(),
        );

        let err = t
            .call(json!({ "dataset": "nope", "aggregate": { "op": "count" } }))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("nope"));
        assert!(
            !msg.contains(dir.path().to_str().unwrap()),
            "must not echo the root/path: {msg}"
        );
        assert_eq!(exec.calls(), 0);
    }

    #[tokio::test]
    async fn sql_on_a_file_handle_is_kind_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let exec = FakeExecutor::new(vec![]);
        let t = tool(
            file_registry(&dir),
            &dir.path().join("l.jsonl"),
            exec.clone(),
        );

        let err = t
            .call(json!({ "dataset": "orders", "query": "SELECT 1" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("aggregate"), "{err}");
        assert_eq!(exec.calls(), 0);
    }

    /// verify with a deterministic executor → verified:true, equal hashes,
    /// two executions, ONE ledger line with verify:true.
    #[tokio::test]
    async fn verify_deterministic_result_is_verified_true() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("l.jsonl");
        let exec = FakeExecutor::new(vec![default_outcome(), default_outcome()]);
        let t = tool(file_registry(&dir), &ledger_path, exec.clone());

        let v = t
            .call(json!({ "dataset": "orders", "aggregate": { "op": "count" }, "verify": true }))
            .await
            .unwrap();
        assert_eq!(v["verified"], json!(true));
        let runs = v["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0]["result_hash"], runs[1]["result_hash"]);
        assert!(runs[0]["result_hash"]
            .as_str()
            .unwrap()
            .starts_with("blake3:"));
        assert!(v.get("warning").is_none());
        assert_eq!(v["execution"]["cache"], "bypass");
        assert_eq!(exec.calls(), 2, "two fully fresh executions");

        let text = std::fs::read_to_string(&ledger_path).unwrap();
        assert_eq!(text.lines().count(), 1, "one call == one ledger line");
        let line: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(line["verify"], json!(true));
        assert_eq!(line["verified"], json!(true));
        assert_eq!(line["cache"], "bypass");
    }

    /// verify mismatch → verified:false, BOTH hashes surfaced, warning set,
    /// first run's result returned — surfaced, never swallowed.
    #[tokio::test]
    async fn verify_mismatch_is_surfaced_not_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        let first = default_outcome();
        let second = QueryOutcome {
            rows: vec![vec![json!(5)]],
            ..default_outcome()
        };
        let exec = FakeExecutor::new(vec![first.clone(), second.clone()]);
        let t = tool(
            file_registry(&dir),
            &dir.path().join("l.jsonl"),
            exec.clone(),
        );

        let v = t
            .call(json!({ "dataset": "orders", "aggregate": { "op": "count" }, "verify": true }))
            .await
            .unwrap();
        assert_eq!(v["verified"], json!(false));
        let runs = v["runs"].as_array().unwrap();
        assert_ne!(runs[0]["result_hash"], runs[1]["result_hash"]);
        assert_eq!(runs[0]["result_hash"], json!(first.result_hash()));
        assert_eq!(runs[1]["result_hash"], json!(second.result_hash()));
        assert!(v["warning"].as_str().unwrap().contains("differed"));
        assert_eq!(v["rows"], json!(first.rows), "first run's result returned");
        assert_eq!(exec.calls(), 2);
    }

    /// Cache: same file content + same spec → second call is a hit with the
    /// executor invoked exactly once; verify bypasses and refreshes.
    #[tokio::test]
    async fn file_cache_hits_and_verify_bypasses() {
        let dir = tempfile::tempdir().unwrap();
        let exec = FakeExecutor::new(vec![]);
        let t = tool(
            file_registry(&dir),
            &dir.path().join("l.jsonl"),
            exec.clone(),
        );
        let args = json!({ "dataset": "orders", "aggregate": { "op": "count" } });

        let v1 = t.call(args.clone()).await.unwrap();
        assert_eq!(v1["execution"]["cache"], "miss");
        assert_eq!(exec.calls(), 1);

        let v2 = t.call(args.clone()).await.unwrap();
        assert_eq!(v2["execution"]["cache"], "hit");
        assert_eq!(exec.calls(), 1, "hit must not re-execute");
        assert_eq!(v2["rows"], v1["rows"]);

        // verify bypasses the cache read: two more executions.
        let v3 = t
            .call(json!({ "dataset": "orders", "aggregate": { "op": "count" }, "verify": true }))
            .await
            .unwrap();
        assert_eq!(v3["execution"]["cache"], "bypass");
        assert_eq!(exec.calls(), 3);
    }

    /// Any byte change to the file changes the blake3 key → miss.
    #[tokio::test]
    async fn file_content_change_invalidates_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let exec = FakeExecutor::new(vec![]);
        let registry = file_registry(&dir);
        let t = tool(registry, &dir.path().join("l.jsonl"), exec.clone());
        let args = json!({ "dataset": "orders", "aggregate": { "op": "count" } });

        t.call(args.clone()).await.unwrap();
        assert_eq!(exec.calls(), 1);
        std::fs::write(dir.path().join("orders.csv"), "region,amount\nemea,9\n").unwrap();
        let v = t.call(args).await.unwrap();
        assert_eq!(v["execution"]["cache"], "miss");
        assert_eq!(exec.calls(), 2, "changed content must re-execute");
    }

    /// Postgres handles never produce cache entries: repeated identical
    /// calls re-execute every time, and the meter reports cache "none"
    /// (uncacheable), never "miss" (which would read as a 0% hit rate).
    #[tokio::test]
    async fn pg_results_are_never_cached() {
        use crate::query::handle::DataSource;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://ro@localhost:1/never-connected")
            .unwrap();
        let registry = Arc::new(HandleRegistry::from_handles(vec![DatasetHandle::new(
            HandleAlias::try_from("warehouse").unwrap(),
            None,
            DataSource::Postgres { pool },
        )]));
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("l.jsonl");
        let exec = FakeExecutor::new(vec![]);
        let t = tool(registry, &ledger_path, exec.clone());
        let args = json!({ "dataset": "warehouse", "query": "SELECT 1" });

        let v1 = t.call(args.clone()).await.unwrap();
        let v2 = t.call(args).await.unwrap();
        assert_eq!(exec.calls(), 2, "pg calls must never be served from cache");
        assert_eq!(v1["execution"]["cache"], "none");
        assert_eq!(v2["execution"]["cache"], "none");

        let text = std::fs::read_to_string(&ledger_path).unwrap();
        for line in text.lines() {
            let line: Value = serde_json::from_str(line).unwrap();
            assert_eq!(line["cache"], "none", "pg has no cache to hit OR miss");
        }
    }

    /// Cache-hit lines must not repeat the original run's rows_scanned: no
    /// rows are scanned on a hit (only a content-hash pass), so both the
    /// envelope and the ledger report null — summing rows_scanned across
    /// ledger lines must equal actual scan work.
    #[tokio::test]
    async fn cache_hit_reports_null_rows_scanned() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("l.jsonl");
        let exec = FakeExecutor::new(vec![]);
        let t = tool(file_registry(&dir), &ledger_path, exec.clone());
        let args = json!({ "dataset": "orders", "aggregate": { "op": "count" } });

        let v1 = t.call(args.clone()).await.unwrap();
        assert_eq!(v1["execution"]["rows_scanned"], json!(4), "miss is exact");
        let v2 = t.call(args).await.unwrap();
        assert_eq!(v2["execution"]["cache"], "hit");
        assert!(
            v2["execution"]["rows_scanned"].is_null(),
            "hit scanned nothing: {v2}"
        );

        let text = std::fs::read_to_string(&ledger_path).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["rows_scanned"], json!(4));
        assert!(lines[1]["rows_scanned"].is_null());
        assert_eq!(lines[1]["cache"], "hit");
    }

    /// Ledger line: alias only (no path/root), exact file rows_scanned,
    /// non-token unit; pg lines carry rows_scanned: null.
    #[tokio::test]
    async fn ledger_line_carries_alias_only_and_non_token_unit() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("l.jsonl");
        let exec = FakeExecutor::new(vec![]);
        let t = tool(file_registry(&dir), &ledger_path, exec.clone());

        t.call(json!({ "dataset": "orders", "aggregate": { "op": "count" } }))
            .await
            .unwrap();
        let text = std::fs::read_to_string(&ledger_path).unwrap();
        let line: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(line["dataset"], "orders");
        assert_eq!(line["kind"], "file");
        assert_eq!(line["unit"], "execution");
        assert_eq!(line["priced_as_tokens"], json!(false));
        assert_eq!(line["rows_scanned"], json!(4));
        assert_eq!(line["result_rows"], json!(1));
        assert!(
            !text.contains(dir.path().to_str().unwrap()),
            "ledger must not contain the path: {text}"
        );
    }

    /// Unwritable ledger → warn only; the call still succeeds.
    #[tokio::test]
    async fn unwritable_ledger_does_not_fail_the_call() {
        let dir = tempfile::tempdir().unwrap();
        let exec = FakeExecutor::new(vec![]);
        let t = tool(
            file_registry(&dir),
            &dir.path().join("no-such-dir/l.jsonl"),
            exec.clone(),
        );
        let v = t
            .call(json!({ "dataset": "orders", "aggregate": { "op": "count" } }))
            .await
            .unwrap();
        assert_eq!(v["row_count"], json!(1));
    }
}
