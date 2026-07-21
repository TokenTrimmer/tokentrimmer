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
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
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
    /// Per-workflow egress allowlist for Http nodes (default-deny).
    ///
    /// An Http node whose url host is not an exact member of this list is
    /// rejected at save-time by `validate`.  Empty (the default) means all
    /// Http nodes are rejected — populate this list to enable Http calls.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Freeform editor metadata (WF-3). The engine ignores this field entirely;
    /// it carries client-side data that should persist across devices/sessions —
    /// today, canvas node positions (`{"canvas_positions": {node_id: {x,y}}}`),
    /// previously localStorage-only + lost across browsers. Additive + defaulted
    /// so an older definition without it deserializes fine + the engine never
    /// reads it (the dashboard reads/writes it on save/load). Keep the shape
    /// loosely typed (`serde_json::Value`) so future editor metadata (selection,
    /// zoom) rides along without a schema bump.
    ///
    /// `skip_serializing_if = "Value::is_null"` keeps `content_hash` stable: a
    /// definition written before this field existed (no `metadata` key)
    /// deserializes to `Value::Null` + re-serializes WITHOUT the key — byte-
    /// identical to the original, so the content-addressed version dedup
    /// (`ON CONFLICT ... DO NOTHING` on `content_hash`) is unchanged.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
    /// CO-2: workflow invokers (alongside the human-Run path). Empty (the
    /// default) = only a human can start the workflow. `Schedule` fires on a
    /// fixed interval; `Webhook` fires via a signed webhook URL. The engine
    /// ignores this field (triggers are invokers, not nodes — `NodeKind::Trigger`
    /// stays the passive entry point that echoes `inputs`); the cloud's sweep +
    /// webhook endpoint read this to decide what to fire. `skip_serializing_if =
    /// Vec::is_empty` keeps `content_hash` stable (a definition without
    /// `triggers` round-trips byte-identical — the `metadata` precedent).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub triggers: Vec<WorkflowTrigger>,
}

/// A workflow invoker (CO-2). Distinct from `NodeKind::Trigger` (the passive
/// entry-point node that echoes the run's `inputs`); these are OUT-OF-BAND
/// invokers the cloud surfaces — a schedule a sweep fires, or a signed webhook
/// an external caller POSTs. The engine never reads them; the cloud's
/// `workflow_schedule` sweep + the `/v1/workflows/:id/webhooks/:token` endpoint
/// are the consumers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowTrigger {
    /// Fire on a fixed interval. `interval` is a duration string (`"1h"` /
    /// `"6h"` / `"1d"`) and new definitions must be at least one hour. The
    /// cloud dispatcher picks due work up on an approximate hourly sweep, not
    /// at an exact wall-clock time. No real-cron expressions in v1 (no cron
    /// crate dep — the cloud sweeps are fixed-`Duration` sleeps; mirroring that
    /// keeps the cadence discipline uniform).
    Schedule {
        interval: String,
        /// Optional immutable release environment resolved when this trigger
        /// occurrence is durably accepted. Omission preserves the historical
        /// latest-saved-definition behavior.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        environment: Option<WorkflowTriggerEnvironment>,
    },
    /// Fire via a signed webhook URL. `token_id` is the public trigger
    /// identifier embedded in the v2 capability path
    /// (`POST /v1/workflows/:id/webhooks/{org_uuid}.{token_id}.{hex_sig}`); the
    /// HMAC secret is derived server-side under a
    /// `wfwh2|{org_id}|{workflow_id}|{token_id}` domain. Binding the tenant is
    /// required because workflow ids are tenant-scoped. This reuses the
    /// badge-HMAC key + the `verify_receipt_share_url` idiom.
    Webhook {
        token_id: String,
        /// Optional immutable release environment resolved when this trigger
        /// occurrence is durably accepted. Omission preserves the historical
        /// latest-saved-definition behavior.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        environment: Option<WorkflowTriggerEnvironment>,
    },
}

/// Closed release selector available to automatic workflow triggers. This is
/// deliberately the same three-value wire vocabulary as direct and route
/// execution, while remaining in the pure definition model consumed by Cloud.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum WorkflowTriggerEnvironment {
    Development,
    Staging,
    Production,
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
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
pub struct Node {
    pub id: String,
    #[serde(flatten)]
    pub kind: NodeKind,
}

/// Discriminated node type.  The `"type"` key selects the variant;
/// variant-specific fields are inlined into the same JSON object via the
/// internal-tag mechanism.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeKind {
    /// Entry-point node; receives the workflow's external input.
    Trigger,

    /// Single LLM call.
    Model {
        selection: ModelSelection,
        prompt: String,
        /// Optional per-call output-token ceiling.  Omitted definitions retain
        /// legacy provider-default behavior; capped workflow admission requires
        /// this to be explicitly set.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_output_tokens: Option<u32>,
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
        /// Optional ceiling for each model turn's completion.  It does not
        /// bound the number of turns; see `max_turns`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_output_tokens: Option<u32>,
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

    /// Outbound HTTP call to an allowlisted external API.
    ///
    /// The `url` host must be a literal hostname that appears in
    /// `WorkflowDefinition::allowed_hosts` (default-deny).  Only the
    /// path, query-string, headers, and body may contain `{{template}}`
    /// tokens; the host must be a static literal so the allowlist is
    /// unambiguous.
    Http {
        method: String,
        url: String,
        #[serde(default)]
        headers: Vec<(String, String)>,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        max_response_bytes: Option<usize>,
    },

    /// Execute another stored workflow as a nested child (W3a-3).
    ///
    /// `version` is accepted for forward-compatibility but UNUSED at MVP —
    /// `load_subworkflow` always returns the latest version.
    /// The parent's remaining budget cap is passed to the child; cost and
    /// baseline roll up into the parent totals so `saved_usd` derives
    /// correctly without double-counting.
    SubWorkflow {
        workflow_id: Uuid,
        #[serde(default)]
        version: Option<u32>,
    },

    /// Bounded loop — runs the body sub-workflow up to `max_iters` times,
    /// re-checking `cond` (Branch syntax) before each iteration.
    /// Termination GUARANTEED by `max_iters`; `cond` is early-exit.
    Loop {
        body_workflow_id: Uuid,
        cond: String,
        #[serde(default)]
        max_iters: u32,
    },

    /// D6 — distill a document's text layer to text the downstream nodes can use.
    ///
    /// Reuses the gateway's Document Lane seam (`document_lane::seam`) — the same
    /// extraction the gateway runs on a routed request — so the workflow + the
    /// gateway agree byte-for-byte on what a doc distills to. The node hashes
    /// the source bytes (blake3) + checks a per-org content-addressed distillation
    /// reuse cache (`flow_doc_distill_cache`): a hit returns the cached text
    /// ($0, no sidecar call); a miss calls the sidecar, stores the result, +
    /// returns it. Fail-loud: a cache-miss with the sidecar unreachable/erroring
    /// emits an error `NodeOutput` (no distilled text) — the node never silently
    /// emits raw bytes.
    ///
    /// v1 scope: `source` must be a `DocumentSource::Base64` (inline bytes). URLs
    /// are rejected (the seam's v1 doesn't fetch — same posture). `cache_key`, if
    /// `Some`, is an optional caller-supplied key (e.g. `"{{trigger.input_id}}"`)
    /// composed into the cache key for explicit reuse control; `None` keys on
    /// the content hash alone.
    Document {
        source: tt_shared::messages::DocumentSource,
        #[serde(default)]
        cache_key: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// Model selection
// ---------------------------------------------------------------------------

/// Determines which model (or routing rule) a `Model`/`Agent` node uses.
/// Serialised with a `"type"` discriminant matching the variant name in
/// snake_case.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
pub enum OnExceed {
    #[default]
    Stop,
}

// ---------------------------------------------------------------------------
// Node output (runtime; not persisted in the definition itself)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct NodeOutput {
    pub content: serde_json::Value,
    pub cost_usd: f64,
    /// Baseline (unoptimized) cost for this node's LLM call(s), sourced from
    /// `RunUsage.baseline_cost_usd`. Zero for non-LLM nodes (Trigger, Transform,
    /// Branch, Output).  `#[serde(default)]` for back-compat.
    #[serde(default)]
    pub baseline_cost_usd: f64,
    pub model_used: Option<String>,
    /// D6 — ISOLATED, ESTIMATED vision-avoided saving the node's distillation
    /// represents (the Document node): priced from the D4c-v2 seam's bookkeeping
    /// via `document_projection::project` (raw image tokens the request WOULD
    /// have sent vs the distilled text tokens, Gemini guard). $0 for every other
    /// node + a cache-hit Document node (no distillation → no saving). NEVER
    /// folded into `cost_usd`/`baseline_cost_usd`/`saved_usd` (a counterfactual,
    /// not invoice-reconcilable — mirrors the gateway `CostBreakdown` field).
    #[serde(default)]
    pub doc_vision_saved_est_usd: f64,
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
    fn output_cap_is_additive_and_roundtrips_when_present() {
        let legacy = r#"{"id":"00000000-0000-0000-0000-000000000000","version":1,"name":"t",
          "nodes":[{"id":"t","type":"trigger"},
                   {"id":"m","type":"model","selection":{"type":"model","model":"gpt-4o-mini"},"prompt":"{{input}}"},
                   {"id":"a","type":"agent","selection":{"type":"model","model":"gpt-4o-mini"},"prompt":"{{input}}","tools":[]},
                   {"id":"o","type":"output"}],
          "edges":[{"from":"t","to":"m"},{"from":"m","to":"a"},{"from":"a","to":"o"}]}"#;
        let def: WorkflowDefinition = serde_json::from_str(legacy).unwrap();
        assert!(matches!(
            &def.nodes[1].kind,
            NodeKind::Model {
                max_output_tokens: None,
                ..
            }
        ));
        assert!(matches!(
            &def.nodes[2].kind,
            NodeKind::Agent {
                max_output_tokens: None,
                ..
            }
        ));
        let legacy_reencoded = serde_json::to_string(&def).unwrap();
        assert!(
            !legacy_reencoded.contains("max_output_tokens"),
            "omitted legacy fields must remain omitted on storage round-trip"
        );

        let capped = r#"{"id":"00000000-0000-0000-0000-000000000000","version":1,"name":"t",
          "nodes":[{"id":"t","type":"trigger"},
                   {"id":"m","type":"model","selection":{"type":"model","model":"gpt-4o-mini"},"prompt":"{{input}}","max_output_tokens":64},
                   {"id":"o","type":"output"}],
          "edges":[{"from":"t","to":"m"},{"from":"m","to":"o"}]}"#;
        let def: WorkflowDefinition = serde_json::from_str(capped).unwrap();
        assert!(matches!(
            &def.nodes[1].kind,
            NodeKind::Model {
                max_output_tokens: Some(64),
                ..
            }
        ));
        assert!(
            serde_json::to_string(&def)
                .unwrap()
                .contains("\"max_output_tokens\":64"),
            "explicit output caps are persisted in definition JSON"
        );
    }

    #[test]
    fn definition_roundtrips_out_of_band_triggers() {
        // Schedules and signed webhooks are behavior-bearing definition
        // fields, not editor-only decoration.  Their canonical JSON and the
        // content hash must survive a deserialize/serialize cycle.
        let json = r#"{"id":"00000000-0000-0000-0000-000000000000","version":1,"name":"automated",
          "nodes":[{"id":"t","type":"trigger"},{"id":"o","type":"output"}],
          "edges":[{"from":"t","to":"o"}],
          "triggers":[
            {"type":"schedule","interval":"6h","environment":"staging"},
            {"type":"webhook","token_id":"ops_sync_1","environment":"production"}
          ]}"#;
        let def: WorkflowDefinition = serde_json::from_str(json).unwrap();
        assert_eq!(
            def.triggers,
            vec![
                WorkflowTrigger::Schedule {
                    interval: "6h".to_string(),
                    environment: Some(WorkflowTriggerEnvironment::Staging),
                },
                WorkflowTrigger::Webhook {
                    token_id: "ops_sync_1".to_string(),
                    environment: Some(WorkflowTriggerEnvironment::Production),
                },
            ]
        );

        let hash = content_hash(&def);
        let serialized = serde_json::to_string(&def).unwrap();
        assert!(serialized.contains("\"triggers\""));
        assert!(serialized.contains("\"environment\":\"staging\""));
        let reparsed: WorkflowDefinition = serde_json::from_str(&serialized).unwrap();
        assert_eq!(reparsed.triggers, def.triggers);
        assert_eq!(content_hash(&reparsed), hash);

        let mut human_run_only = def.clone();
        human_run_only.triggers.clear();
        assert_ne!(
            content_hash(&human_run_only),
            hash,
            "trigger changes must create a distinct immutable definition version"
        );

        let legacy_json = r#"{"id":"00000000-0000-0000-0000-000000000000","version":1,"name":"legacy","nodes":[],"edges":[],"triggers":[{"type":"schedule","interval":"1h"}]}"#;
        let legacy: WorkflowDefinition = serde_json::from_str(legacy_json).unwrap();
        let legacy_serialized = serde_json::to_value(legacy).unwrap();
        assert!(legacy_serialized["triggers"][0]
            .get("environment")
            .is_none());

        for environment in ["development", "staging", "production"] {
            let trigger: WorkflowTrigger = serde_json::from_value(serde_json::json!({
                "type": "webhook",
                "token_id": "closed",
                "environment": environment,
            }))
            .unwrap();
            assert_eq!(
                serde_json::to_value(trigger).unwrap()["environment"],
                environment
            );
        }
        assert!(
            serde_json::from_value::<WorkflowTrigger>(serde_json::json!({
                "type": "schedule",
                "interval": "1h",
                "environment": "preview",
            }))
            .is_err()
        );
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
                    max_output_tokens: None,
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
                    max_output_tokens: None,
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
            Node {
                id: "http".into(),
                kind: NodeKind::Http {
                    method: "POST".into(),
                    url: "https://api.example.com/v1".into(),
                    headers: vec![("X-Custom".into(), "value".into())],
                    body: Some("{{input}}".into()),
                    max_response_bytes: Some(65536),
                },
            },
            Node {
                id: "sub_workflow".into(),
                kind: NodeKind::SubWorkflow {
                    workflow_id: Uuid::nil(),
                    version: None,
                },
            },
            Node {
                id: "document".into(),
                kind: NodeKind::Document {
                    source: tt_shared::messages::DocumentSource::Base64 {
                        media_type: "application/pdf".into(),
                        data: "JVBERi0=".into(),
                    },
                    cache_key: None,
                },
            },
            Node {
                id: "document_cached".into(),
                kind: NodeKind::Document {
                    source: tt_shared::messages::DocumentSource::Base64 {
                        media_type: "application/pdf".into(),
                        data: "JVBERi0=".into(),
                    },
                    cache_key: Some("{{trigger.input_id}}".into()),
                },
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
    fn metadata_is_absent_for_null_and_round_trips_when_present() {
        // WF-3: a Null metadata is omitted from the serialized form (so existing
        // persisted definitions — no metadata key — re-serialize byte-identically
        // + keep their content_hash after the field was added). And metadata
        // round-trips when present (canvas positions persist across devices) +
        // changes the hash (a layout change is a new version).
        let base = WorkflowDefinition {
            triggers: vec![],
            id: Uuid::nil(),
            version: 1,
            name: "m".into(),
            nodes: vec![Node {
                id: "t".into(),
                kind: NodeKind::Trigger,
            }],
            edges: Vec::new(),
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: Vec::new(),
            metadata: serde_json::Value::Null,
        };
        // Null metadata → the serialized form has NO "metadata" key (omitted).
        let v = serde_json::to_value(&base).unwrap();
        assert!(v.get("metadata").is_none(), "Null metadata must be omitted");

        // Re-parsing the serialized form → a def whose hash is identical (the
        // content-addressed dedup stays stable across re-parses — the invariant
        // the versioned-definition storage relies on).
        let reparsed: WorkflowDefinition =
            serde_json::from_str(&serde_json::to_string(&base).unwrap()).unwrap();
        assert_eq!(content_hash(&base), content_hash(&reparsed));

        // Present metadata → the key round-trips through serialize/deserialize.
        let mut with_meta = base.clone();
        with_meta.metadata = serde_json::json!({"canvas_positions": {"n1": {"x": 10, "y": 20}}});
        let s = serde_json::to_string(&with_meta).unwrap();
        assert!(s.contains("canvas_positions"));
        let reparsed_m: WorkflowDefinition = serde_json::from_str(&s).unwrap();
        assert_eq!(
            reparsed_m.metadata,
            serde_json::json!({"canvas_positions": {"n1": {"x": 10, "y": 20}}})
        );
        // A present metadata → a different content_hash (a layout change is a
        // new version, NOT silently deduped).
        assert_ne!(content_hash(&with_meta), content_hash(&base));
    }

    #[test]
    fn document_node_roundtrips_and_is_content_stable() {
        // A Document node deserializes from the canonical wire shape + the
        // content_hash is stable across re-parses + differs when the source
        // bytes change (the cache key is content-addressed on those bytes).
        let json = r#"{"id":"d","type":"document","source":{"type":"base64","media_type":"application/pdf","data":"JVBERi0="}}"#;
        let node: Node = serde_json::from_str(json).unwrap();
        let NodeKind::Document { source, cache_key } = &node.kind else {
            panic!("expected a Document node");
        };
        assert!(matches!(
            source,
            tt_shared::messages::DocumentSource::Base64 { media_type, data }
                if media_type == "application/pdf" && data == "JVBERi0="
        ));
        assert!(cache_key.is_none());

        // Re-serialize + re-parse → identical hash (deterministic serialization).
        let def = WorkflowDefinition {
            triggers: vec![],
            id: Uuid::nil(),
            version: 1,
            name: "doc-flow".into(),
            nodes: vec![node.clone()],
            edges: Vec::new(),
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: Vec::new(),
            metadata: serde_json::Value::Null,
        };
        let h1 = content_hash(&def);
        let reparsed: WorkflowDefinition =
            serde_json::from_str(&serde_json::to_string(&def).unwrap()).unwrap();
        assert_eq!(h1, content_hash(&reparsed));

        // Different source bytes → different hash (the content-addressed key).
        let mut def2 = def.clone();
        if let NodeKind::Document {
            source: tt_shared::messages::DocumentSource::Base64 { data, .. },
            ..
        } = &mut def2.nodes[0].kind
        {
            *data = "different".into();
        }
        assert_ne!(h1, content_hash(&def2));
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
