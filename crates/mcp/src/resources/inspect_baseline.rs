//! Current inspect baseline JSON for the working tree.

use async_trait::async_trait;
use serde_json::Value;

use crate::error::McpError;
use crate::protocol::ResourceDef;
use crate::resources::Resource;

pub struct InspectBaselineResource;

#[async_trait]
impl Resource for InspectBaselineResource {
    fn def(&self) -> ResourceDef {
        ResourceDef {
            uri: "mcp://tokentrimmer/inspect/baseline".into(),
            name: "Inspect baseline".into(),
            description: Some("Current `tt inspect` findings on the working tree.".into()),
            mime_type: "application/json",
        }
    }
    async fn read(&self) -> Result<Value, McpError> {
        let mut engine = tt_inspect_core::Engine::new();
        for rule in tt_inspect_rules_tier1::all_rules() {
            engine.add_rule(rule);
        }
        let findings = engine.scan(std::path::Path::new("."));
        Ok(serde_json::json!({ "uri": self.def().uri, "findings": findings }))
    }
}
