//! Agentic tool-calling for `tt chat`: a client-side loop that advertises the
//! stateless `tt-mcp` tools, executes the model's `tool_calls` locally, and
//! feeds results back until the model returns a text answer. Non-streamed.

use serde_json::{json, Value};

use tt_mcp::tools::find_route_for::FindRouteForTool;
use tt_mcp::tools::inspect_diff::InspectDiffTool;
use tt_mcp::tools::preview_cost::PreviewCostTool;
use tt_mcp::tools::Registry;
use tt_shared::messages::{Message, MessageContent, ToolCall, ToolCallFunction};

use super::{format_turn_footer, Conversation, Ledger, UsageInfo};
use crate::ui;

/// Hard cap on tool-call rounds per turn (loop guard).
const MAX_ROUNDS: usize = 6;

/// The 3 stateless, read-only tools `tt chat` exposes to the model.
#[must_use]
pub fn build_registry() -> Registry {
    let mut r = Registry::new();
    r.register(Box::new(FindRouteForTool));
    r.register(Box::new(PreviewCostTool));
    r.register(Box::new(InspectDiffTool));
    r
}

/// Build the OpenAI `tools` array from the registry's tool definitions.
#[must_use]
pub fn tools_json(reg: &Registry) -> Vec<Value> {
    reg.list()
        .into_iter()
        .map(|d| {
            json!({
                "type": "function",
                "function": {
                    "name": d.name,
                    "description": d.description,
                    "parameters": d.input_schema,
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_json_advertises_three_tools() {
        let reg = build_registry();
        let t = tools_json(&reg);
        assert_eq!(t.len(), 3);
        let names: Vec<&str> = t
            .iter()
            .map(|v| v["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"find_route_for"));
        assert!(names.contains(&"preview_cost"));
        assert!(names.contains(&"inspect_diff"));
        // schema carried through
        let fr = t
            .iter()
            .find(|v| v["function"]["name"] == "find_route_for")
            .unwrap();
        assert_eq!(
            fr["function"]["parameters"]["required"][0],
            "task_description"
        );
    }
}
