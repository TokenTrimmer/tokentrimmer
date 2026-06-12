//! Execution ledger — a DISTINCT NON-TOKEN cost class.
//!
//! Every `run_query` call appends one JSONL line carrying
//! `unit: "execution"` and `priced_as_tokens: false`. Nothing in this lane
//! converts execution cost to USD or token counts — wall time and rows
//! scanned are metered as what they are. `rows_scanned` is exact for file
//! scans and `null` for Postgres in the MVP (no honest number exists; we
//! don't fabricate one — the same stance as the cost tools'
//! `UnconfiguredBackend`).
//!
//! Ledger write failures are NON-FATAL: a query result is never discarded
//! because the meter could not be written; the failure is logged.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// One `run_query` execution record. One call == one line (a `verify` call
/// is one line with `wall_ms` summed across both runs).
#[derive(Debug, Serialize)]
pub struct LedgerEntry {
    /// RFC 3339 timestamp.
    pub ts: String,
    pub tool: &'static str,
    /// Dataset ALIAS only — never a path or DSN.
    pub dataset: String,
    /// `"file"` or `"postgres"`.
    pub kind: &'static str,
    pub wall_ms: u64,
    /// Exact for file scans; `None` (JSON `null`) for Postgres AND for
    /// cache hits (a hit scans nothing — only a content-hash pass) — honest.
    pub rows_scanned: Option<u64>,
    pub result_rows: usize,
    pub result_bytes: usize,
    /// `"hit"`, `"miss"`, `"bypass"` (verify mode bypasses the cache), or
    /// `"none"` (Postgres: no cache exists for this kind — not a 0% hit
    /// rate).
    pub cache: &'static str,
    pub verify: bool,
    /// `Some(true/false)` when `verify` ran; `None` otherwise.
    pub verified: Option<bool>,
    /// Always `"execution"` — a distinct, non-token unit.
    pub unit: &'static str,
    /// Always `false` — execution cost is never priced as tokens.
    pub priced_as_tokens: bool,
}

/// Append-only JSONL execution ledger.
pub struct ExecutionLedger {
    path: PathBuf,
}

impl ExecutionLedger {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Resolve the ledger path: `TT_MCP_QUERY_LEDGER_PATH` or the
    /// cost-ledger convention default `.claude/query-ledger.jsonl`.
    pub fn default_path() -> PathBuf {
        std::env::var("TT_MCP_QUERY_LEDGER_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(".claude/query-ledger.jsonl"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one line. NON-FATAL on failure: warns and returns — the
    /// tool call still succeeds.
    pub fn append(&self, entry: &LedgerEntry) {
        let line = match serde_json::to_string(entry) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "query ledger: serialize failed (entry dropped)");
                return;
            }
        };
        let result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = result {
            tracing::warn!(
                error = %e,
                path = %self.path.display(),
                "query ledger: write failed (non-fatal; call still succeeds)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> LedgerEntry {
        LedgerEntry {
            ts: chrono::Utc::now().to_rfc3339(),
            tool: "run_query",
            dataset: "orders".into(),
            kind: "file",
            wall_ms: 12,
            rows_scanned: Some(105_000),
            result_rows: 3,
            result_bytes: 120,
            cache: "miss",
            verify: false,
            verified: None,
            unit: "execution",
            priced_as_tokens: false,
        }
    }

    #[test]
    fn appended_line_has_the_non_token_cost_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("query-ledger.jsonl");
        let ledger = ExecutionLedger::new(&path);
        ledger.append(&entry());

        let text = std::fs::read_to_string(&path).unwrap();
        let line: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(line["unit"], "execution");
        assert_eq!(line["priced_as_tokens"], false);
        assert_eq!(line["tool"], "run_query");
        assert_eq!(line["dataset"], "orders");
        assert_eq!(line["kind"], "file");
        assert_eq!(line["wall_ms"], 12);
        assert_eq!(line["rows_scanned"], 105_000);
        assert_eq!(line["result_rows"], 3);
        assert_eq!(line["result_bytes"], 120);
        assert_eq!(line["cache"], "miss");
        assert_eq!(line["verify"], false);
        assert_eq!(line["verified"], serde_json::Value::Null);
    }

    #[test]
    fn pg_rows_scanned_serializes_as_null_not_a_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("l.jsonl");
        let ledger = ExecutionLedger::new(&path);
        let mut e = entry();
        e.kind = "postgres";
        e.rows_scanned = None; // no fabricated numbers
        ledger.append(&e);
        let text = std::fs::read_to_string(&path).unwrap();
        let line: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert!(line["rows_scanned"].is_null());
    }

    #[test]
    fn appends_accumulate_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("l.jsonl");
        let ledger = ExecutionLedger::new(&path);
        ledger.append(&entry());
        ledger.append(&entry());
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
    }

    /// Unwritable ledger path → warn + the append (and hence the tool call)
    /// still succeeds. No panic, no error.
    #[test]
    fn unwritable_path_is_non_fatal() {
        let dir = tempfile::tempdir().unwrap();
        // The parent directory does not exist → open fails.
        let ledger = ExecutionLedger::new(dir.path().join("missing-dir/l.jsonl"));
        ledger.append(&entry()); // must not panic
    }
}
