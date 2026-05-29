//! Last 7 days of .claude/cost-ledger.jsonl.

use async_trait::async_trait;
use serde_json::Value;

use crate::error::McpError;
use crate::protocol::ResourceDef;
use crate::resources::Resource;

pub struct CostLedgerResource;

#[async_trait]
impl Resource for CostLedgerResource {
    fn def(&self) -> ResourceDef {
        ResourceDef {
            uri: "mcp://tokentrimmer/cost-ledger/last-7d".into(),
            name: "Cost ledger (last 7 days)".into(),
            description: Some("JSONL of recent autopilot iteration costs.".into()),
            mime_type: "application/x-ndjson",
        }
    }
    async fn read(&self) -> Result<Value, McpError> {
        let path = std::env::var("TT_COST_LEDGER_PATH")
            .unwrap_or_else(|_| ".claude/cost-ledger.jsonl".into());
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        Ok(serde_json::json!({ "uri": self.def().uri, "text": body }))
    }
}
