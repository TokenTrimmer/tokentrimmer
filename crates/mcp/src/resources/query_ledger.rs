//! Recent lines of the query execution ledger (mirrors
//! [`crate::resources::cost_ledger::CostLedgerResource`]).
//!
//! Execution cost is a DISTINCT NON-TOKEN class — every line carries
//! `unit: "execution"` and `priced_as_tokens: false` (see
//! [`crate::query::ledger`]). This resource is read-only and exposes the
//! ledger as ndjson for inspection by MCP clients.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::McpError;
use crate::protocol::ResourceDef;
use crate::resources::Resource;

/// Max lines returned (tail).
const MAX_LINES: usize = 200;

pub struct QueryLedgerResource {
    /// Ledger path, wired at registration from the same resolved path the
    /// [`crate::query::ledger::ExecutionLedger`] writer uses.
    pub path: PathBuf,
}

#[async_trait]
impl Resource for QueryLedgerResource {
    fn def(&self) -> ResourceDef {
        ResourceDef {
            uri: "mcp://tokentrimmer/query-ledger/recent".into(),
            name: "Query execution ledger (recent)".into(),
            description: Some(
                "JSONL of recent run_query executions (non-token cost: unit=execution, \
                 priced_as_tokens=false)."
                    .into(),
            ),
            mime_type: "application/x-ndjson",
        }
    }

    async fn read(&self) -> Result<Value, McpError> {
        let body = std::fs::read_to_string(&self.path).unwrap_or_default();
        let lines: Vec<&str> = body.lines().collect();
        let tail = lines[lines.len().saturating_sub(MAX_LINES)..].join("\n");
        Ok(serde_json::json!({ "uri": self.def().uri, "text": tail }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_tail_of_ledger_and_tolerates_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("query-ledger.jsonl");

        // Missing file → empty text, not an error.
        let r = QueryLedgerResource { path: path.clone() };
        let v = r.read().await.unwrap();
        assert_eq!(v["text"], "");

        let many: String = (0..MAX_LINES + 10)
            .map(|i| format!("{{\"n\":{i}}}\n"))
            .collect();
        std::fs::write(&path, many).unwrap();
        let v = r.read().await.unwrap();
        let text = v["text"].as_str().unwrap();
        assert_eq!(text.lines().count(), MAX_LINES, "tail capped");
        assert!(text
            .lines()
            .last()
            .unwrap()
            .contains(&format!("{}", MAX_LINES + 9)));
        assert_eq!(v["uri"], "mcp://tokentrimmer/query-ledger/recent");
    }
}
