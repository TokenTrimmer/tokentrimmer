//! Workflow editor ↔ engine wire-shape contract test (P0-6).
//!
//! The dashboard's `WorkflowEditor.buildDef()` (cloud) emits a JSON object per
//! node that the public gateway deserializes as a `workflow::types::Node`
//! (internally-tagged `NodeKind`). Historically the two drifted in three ways
//! that made whole node classes unsaveable from the UI:
//!
//! - HTTP `headers`: the UI emitted a JSON object `{"k":"v"}`, but the engine
//!   expects `Vec<(String,String)>` (a JSON array of pairs). Even `{}` 400'd
//!   on save (a serde "invalid type: map, expected a sequence").
//! - Agent `tools`: the UI emitted `string[]`, but the engine expects
//!   `Vec<Tool>` (`[{type,function:{name,parameters}}]`).
//! - Loop `max_iters`: `#[serde(default)]` on `u32` → 0 when the field was
//!   absent, which `validate` rejects ("must be 1..=100").
//!
//! The dashboard fix (`normalizeNode` in buildDef) coerces the UI shapes to the
//! engine shapes before send. THIS test pins the engine-side contract: each
//! normalized (UI-emit, post-fix) JSON must deserialize; each buggy shape must
//! fail with a serde error (so a future editor regression is caught red).
//!
//! No DB needed — pure serde over JSON strings.

use tt_core::workflow::types::{ModelSelection, Node, NodeKind, WorkflowDefinition};
use uuid::Uuid;

/// A minimal valid workflow JSON wrapper around a node list (the shape
/// `buildDef()` emits: id + version + name + nodes + edges; the server fills
/// id/version client may send placeholders).
fn wf_json(nodes_json: &str) -> String {
    format!(
        r#"{{"id":"00000000-0000-0000-0000-000000000001","version":1,"name":"t","nodes":[{nodes_json}],"edges":[]}}"#
    )
}

fn parse(nodes_json: &str) -> Result<WorkflowDefinition, serde_json::Error> {
    serde_json::from_str::<WorkflowDefinition>(&wf_json(nodes_json))
}

#[test]
fn model_node_with_pinned_model_deserializes() {
    // The post-fix UI default: selection {type:'model', model:'...'}.
    let j = r#"{"id":"n1","type":"model","selection":{"type":"model","model":"gpt-4o-mini"},"prompt":"hi"}"#;
    let def = parse(j).expect("a pinned-model node deserializes");
    let Node { kind, .. } = &def.nodes[0];
    assert!(matches!(
        kind,
        NodeKind::Model {
            selection: ModelSelection::Model { .. },
            ..
        }
    ));
}

#[test]
fn model_node_with_route_ref_deserializes() {
    let j = r#"{"id":"n1","type":"model","selection":{"type":"route","route_ref":"cheap-for-short"},"prompt":"hi"}"#;
    let def = parse(j).expect("a route-ref selection deserializes");
    let Node { kind, .. } = &def.nodes[0];
    assert!(matches!(
        kind,
        NodeKind::Model {
            selection: ModelSelection::Route { .. },
            ..
        }
    ));
}

#[test]
fn http_node_with_pair_array_headers_deserializes() {
    // The post-fix UI shape: headers as [[k,v],…] (Vec<(String,String)>).
    let j = r#"{"id":"n1","type":"http","method":"GET","url":"https://example.com","headers":[["accept","application/json"]]}"#;
    let def = parse(j).expect("pair-array headers deserialize");
    match &def.nodes[0].kind {
        NodeKind::Http { headers, .. } => {
            assert_eq!(headers, &[("accept".into(), "application/json".into())])
        }
        other => panic!("expected Http, got {other:?}"),
    }
}

#[test]
fn http_node_with_no_headers_deserializes() {
    // headers omitted entirely → #[serde(default)] applies → empty Vec.
    let j = r#"{"id":"n1","type":"http","method":"GET","url":"https://example.com"}"#;
    let def = parse(j).expect("headerless HTTP node deserializes (default empty Vec)");
    match &def.nodes[0].kind {
        NodeKind::Http { headers, .. } => assert!(headers.is_empty()),
        other => panic!("expected Http, got {other:?}"),
    }
}

#[test]
#[should_panic(expected = "invalid type: map")]
fn http_node_with_object_headers_is_rejected_by_serde() {
    // THE BUG: a JSON object `{"k":"v"}` where Vec<(String,String)> expects a
    // sequence. The dashboard fix normalizes to pairs before send; if a future
    // regression emits an object, this fails red at the engine boundary.
    let j = r#"{"id":"n1","type":"http","method":"GET","url":"https://example.com","headers":{"accept":"application/json"}}"#;
    let _ = parse(j).unwrap();
}

#[test]
#[should_panic(expected = "invalid type: map")]
fn http_node_with_empty_object_headers_is_rejected() {
    // Even `{}` fails (#[serde(default)] only fires on an absent field).
    let j = r#"{"id":"n1","type":"http","method":"GET","url":"https://example.com","headers":{}}"#;
    let _ = parse(j).unwrap();
}

#[test]
fn agent_node_with_tool_objects_deserializes() {
    // The post-fix UI shape: tools as [{type:"function",function:{name,parameters:{}}}].
    let j = r#"{"id":"n1","type":"agent","selection":{"type":"model","model":"gpt-4o-mini"},"prompt":"hi","max_turns":3,"tools":[{"type":"function","function":{"name":"web_search","parameters":{}}}]}"#;
    let def = parse(j).expect("tool-object array deserializes");
    match &def.nodes[0].kind {
        NodeKind::Agent { tools, .. } => assert_eq!(tools.len(), 1),
        other => panic!("expected Agent, got {other:?}"),
    }
}

#[test]
#[should_panic(expected = "invalid type")]
fn agent_node_with_string_tools_is_rejected_by_serde() {
    // THE BUG: `["web_search"]` (string[]) where Vec<Tool> expects objects.
    let j = r#"{"id":"n1","type":"agent","selection":{"type":"model","model":"gpt-4o-mini"},"prompt":"hi","tools":["web_search"]}"#;
    let _ = parse(j).unwrap();
}

#[test]
fn loop_node_with_valid_max_iters_deserializes() {
    // The post-fix UI default: max_iters:3 (not 0).
    let j = r#"{"id":"n1","type":"loop","body_workflow_id":"00000000-0000-0000-0000-000000000002","cond":"{{x}} > 0","max_iters":3}"#;
    let def = parse(j).expect("a loop with max_iters deserializes");
    match &def.nodes[0].kind {
        NodeKind::Loop { max_iters, .. } => assert_eq!(*max_iters, 3),
        other => panic!("expected Loop, got {other:?}"),
    }
}

#[test]
fn loop_node_without_max_iters_defaults_to_zero_then_validate_rejects() {
    // Absent max_iters → #[serde(default)] → 0. Deserialization SUCCEEDS (serde
    // doesn't know the 1..=100 rule); the validate step catches it. This test
    // pins that the field deserializes to 0 (so the dashboard's addNode seeding
    // max_iters:3 is what makes a fresh node valid — the engine default is 0).
    let j = r#"{"id":"n1","type":"loop","body_workflow_id":"00000000-0000-0000-0000-000000000002","cond":"{{x}} > 0"}"#;
    let def = parse(j).expect("serde accepts absent max_iters (defaults to 0)");
    match &def.nodes[0].kind {
        NodeKind::Loop { max_iters, .. } => assert_eq!(*max_iters, 0),
        other => panic!("expected Loop, got {other:?}"),
    }
}

#[test]
fn trigger_node_deserializes() {
    let j = r#"{"id":"n1","type":"trigger"}"#;
    let def = parse(j).expect("a trigger node deserializes");
    assert!(matches!(def.nodes[0].kind, NodeKind::Trigger));
}

#[test]
fn branch_node_deserializes() {
    let j = r#"{"id":"n1","type":"branch","cond":"{{x}} == 1","when_true":"a","when_false":"b"}"#;
    let def = parse(j).expect("a branch node deserializes");
    assert!(matches!(def.nodes[0].kind, NodeKind::Branch { .. }));
}

#[test]
fn workflow_definition_with_uuid_id_parses() {
    // The top-level id is a Uuid — a real UUID must parse.
    let j = r#"{"id":"12345678-1234-1234-1234-123456789012","version":1,"name":"t","nodes":[{"id":"n1","type":"trigger"}],"edges":[]}"#;
    let def = serde_json::from_str::<WorkflowDefinition>(j).expect("parses with a real UUID");
    assert_eq!(
        def.id,
        Uuid::parse_str("12345678-1234-1234-1234-123456789012").unwrap()
    );
}
