//! Workflow definition types — pure serde data model, no DB or gateway logic.
//!
//! `WorkflowDefinition` is the canonical, version-stamped description of a workflow DAG.
//! `content_hash` provides a stable blake3 fingerprint used to detect definition changes
//! across saves and deployments.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Top-level definition
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowDefinition {
    pub id: Uuid,
    pub version: u32,
    pub name: String,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// Freeform input schema / defaults for the workflow.
    #[serde(default)]
    pub inputs: serde_json::Value,
    /// Budget guard applied to the whole workflow run.
    #[serde(default)]
    pub budget: BudgetPolicy,
}

// ---------------------------------------------------------------------------
// Nodes
// ---------------------------------------------------------------------------

/// A single DAG node.  The `id` field lives in the wrapper so every node
/// kind shares a common identifier without repetition.
///
/// # Serde approach
/// `#[serde(flatten)]` spreads `NodeKind`'s fields (including the `"type"`
/// tag) into the parent JSON object.  Combined with the internally-tagged
/// `NodeKind`, this round-trips cleanly for JSON.
///
/// NOTE: #[serde(flatten)] + internal tag works only with self-describing formats (JSON); do not serialize with CBOR/MessagePack.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    #[serde(flatten)]
    pub kind: NodeKind,
}

/// Discriminated node type.  The `"type"` key selects the variant;
/// variant-specific fields are inlined into the same JSON object via the
/// internal-tag mechanism.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeKind {
    /// Entry-point node; receives the workflow's external input.
    Trigger,

    /// Single LLM call.
    Model {
        selection: ModelSelection,
        prompt: String,
        #[serde(default)]
        max_cost_usd: Option<f64>,
    },

    /// Agentic multi-turn loop with tool access.
    Agent {
        selection: ModelSelection,
        prompt: String,
        /// Turn cap for the agent loop; None => the engine's default (DEFAULT_MAX_TURNS = 8, matching CreateRunRequest).
        #[serde(default)]
        max_turns: Option<u32>,
        #[serde(default)]
        max_cost_usd: Option<f64>,
        #[serde(default)]
        tools: Vec<tt_shared::messages::Tool>,
    },

    /// Deterministic expression transform (no LLM call).
    Transform { expr: String },

    /// Conditional branch; exactly one outgoing edge is followed at runtime.
    Branch {
        cond: String,
        when_true: String,
        when_false: String,
    },

    /// Terminal output-collection node.
    Output,
}

// ---------------------------------------------------------------------------
// Model selection
// ---------------------------------------------------------------------------

/// Determines which model (or routing rule) a `Model`/`Agent` node uses.
/// Serialised with a `"type"` discriminant matching the variant name in
/// snake_case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelSelection {
    /// A specific model id (e.g. `"claude-3-5-haiku-20241022"`).
    Model { model: String },
    /// A named TokenTrimmer route (resolved at runtime).
    Route { route_ref: String },
    /// Let the gateway pick the best model automatically.
    Auto,
}

// ---------------------------------------------------------------------------
// Edges
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    /// Optional jq/expression to map the source output before passing it on.
    #[serde(default)]
    pub map: Option<String>,
}

// ---------------------------------------------------------------------------
// Budget policy
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BudgetPolicy {
    /// Hard USD cap for the entire workflow run.
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
    /// What to do when the cap is exceeded.
    #[serde(default)]
    pub on_exceed: OnExceed,
}

/// Action taken when a budget limit is hit.  Only `Stop` is implemented in
/// W1a; warn/throttle/etc. are deferred.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum OnExceed {
    #[default]
    Stop,
}

// ---------------------------------------------------------------------------
// Node output (runtime; not persisted in the definition itself)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeOutput {
    pub content: serde_json::Value,
    pub cost_usd: f64,
    pub model_used: Option<String>,
}

// ---------------------------------------------------------------------------
// Content hash
// ---------------------------------------------------------------------------

/// Returns a stable blake3 hex fingerprint of `def`.
///
/// The hash is computed over `serde_json::to_vec(def)`.  Struct fields
/// serialize in declaration order (deterministic in serde), so the output is
/// stable across re-parses of the same definition.  Note: `serde_json::Value`
/// map keys within `inputs` are sorted by serde_json's `BTreeMap` serializer
/// only when the `preserve_order` feature is **off** (the default); if
/// `preserve_order` is enabled the hash of `inputs` maps is insertion-order
/// dependent.  For W1a this is acceptable.
pub fn content_hash(def: &WorkflowDefinition) -> String {
    let canonical =
        serde_json::to_vec(def).expect("WorkflowDefinition is always JSON-serializable");
    blake3::hash(&canonical).to_hex().to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_roundtrips_and_hashes() {
        let json = r#"{"id":"00000000-0000-0000-0000-000000000000","version":1,"name":"t",
          "nodes":[{"id":"t","type":"trigger"},
                   {"id":"m","type":"model","selection":{"type":"route","route_ref":"r1"},"prompt":"{{input}}"},
                   {"id":"o","type":"output"}],
          "edges":[{"from":"t","to":"m"},{"from":"m","to":"o"}]}"#;
        let def: WorkflowDefinition = serde_json::from_str(json).unwrap();
        assert_eq!(def.nodes.len(), 3);
        assert!(matches!(def.nodes[1].kind, NodeKind::Model { .. }));
        let h1 = content_hash(&def);
        let reparsed: WorkflowDefinition =
            serde_json::from_str(&serde_json::to_string(&def).unwrap()).unwrap();
        assert_eq!(h1, content_hash(&reparsed));

        // Hash changes when a field changes.
        let mut def2 = def.clone();
        def2.name = "changed".to_string();
        assert_ne!(h1, content_hash(&def2));
    }

    #[test]
    fn all_node_kinds_serialize_correctly() {
        // Verify each variant serializes with the expected "type" tag.
        let nodes = vec![
            Node {
                id: "trigger".into(),
                kind: NodeKind::Trigger,
            },
            Node {
                id: "model".into(),
                kind: NodeKind::Model {
                    selection: ModelSelection::Model {
                        model: "claude-3-5-haiku-20241022".into(),
                    },
                    prompt: "hello".into(),
                    max_cost_usd: None,
                },
            },
            Node {
                id: "agent".into(),
                kind: NodeKind::Agent {
                    selection: ModelSelection::Route {
                        route_ref: "my-route".into(),
                    },
                    prompt: "act".into(),
                    max_turns: Some(5),
                    max_cost_usd: Some(0.10),
                    tools: vec![],
                },
            },
            Node {
                id: "transform".into(),
                kind: NodeKind::Transform {
                    expr: ".output | ascii_upcase".into(),
                },
            },
            Node {
                id: "branch".into(),
                kind: NodeKind::Branch {
                    cond: ".score > 0.5".into(),
                    when_true: "pass".into(),
                    when_false: "fail".into(),
                },
            },
            Node {
                id: "output".into(),
                kind: NodeKind::Output,
            },
        ];

        for node in &nodes {
            let s = serde_json::to_string(node).unwrap();
            let back: Node = serde_json::from_str(&s).unwrap();
            assert_eq!(back.id, node.id);
            // Re-serialize and compare JSON strings for structural equality.
            assert_eq!(
                serde_json::to_value(node).unwrap(),
                serde_json::to_value(&back).unwrap(),
            );
        }
    }

    #[test]
    fn model_selection_variants_roundtrip() {
        let variants: &[(&str, ModelSelection)] = &[
            (
                r#"{"type":"model","model":"gpt-4o"}"#,
                ModelSelection::Model {
                    model: "gpt-4o".into(),
                },
            ),
            (
                r#"{"type":"route","route_ref":"r1"}"#,
                ModelSelection::Route {
                    route_ref: "r1".into(),
                },
            ),
            (r#"{"type":"auto"}"#, ModelSelection::Auto),
        ];

        for (json, _expected) in variants {
            let ms: ModelSelection = serde_json::from_str(json).unwrap();
            let back = serde_json::to_string(&ms).unwrap();
            let ms2: ModelSelection = serde_json::from_str(&back).unwrap();
            assert_eq!(
                serde_json::to_value(&ms).unwrap(),
                serde_json::to_value(&ms2).unwrap(),
            );
        }
    }
}
