//! End-to-end dispatch of the **write** tools (`add_route`, `apply_plan`)
//! through the MCP server, exercising the off-by-default safety gate and the
//! full request path to a mocked gateway/cloud.
//!
//! SAFETY contract under test:
//! - write disabled (the default): `tools/list` omits both tools, and
//!   `tools/call` on either returns `MethodNotFound` (-32601) — NOT
//!   `Unauthorized` (-32001).
//! - write enabled: both tools are listed; `add_route` POSTs `/v1/routes`,
//!   `apply_plan` POSTs `/v1/admin/plans/:id/apply`; a caller-supplied foreign
//!   `org_id` is rejected with -32001 before any network call.

use httpmock::prelude::*;
use serde_json::json;
use tt_mcp::{protocol::JsonRpcRequest, Server};
use uuid::Uuid;

fn list_req() -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".into(),
        method: "tools/list".into(),
        params: json!({}),
        id: Some(json!(1)),
    }
}

fn call_req(name: &str, args: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".into(),
        method: "tools/call".into(),
        params: json!({ "name": name, "arguments": args }),
        id: Some(json!(1)),
    }
}

/// Build a server with write tools registered (when `enabled`), pointing both
/// outbound calls at `base`.
fn server(enabled: bool, org: Uuid, base: &str) -> Server {
    let mut s = Server::new().with_write_enabled(enabled);
    s.register_write_tools(org, base, base, "tt_live_operator", reqwest::Client::new());
    s
}

async fn list_names(server: &Server) -> Vec<String> {
    let v = serde_json::to_value(server.dispatch(list_req()).await).unwrap();
    v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn write_disabled_omits_both_tools_from_list() {
    let s = server(false, Uuid::now_v7(), "http://unused.invalid");
    let names = list_names(&s).await;
    assert!(!names.contains(&"add_route".to_string()));
    assert!(!names.contains(&"apply_plan".to_string()));
}

#[tokio::test]
async fn write_disabled_tools_call_is_method_not_found_not_unauthorized() {
    let s = server(false, Uuid::now_v7(), "http://unused.invalid");
    for name in ["add_route", "apply_plan"] {
        let v = serde_json::to_value(
            s.dispatch(call_req(name, json!({ "name": "x", "then": {} })))
                .await,
        )
        .unwrap();
        assert_eq!(
            v["error"]["code"], -32601,
            "{name} must be MethodNotFound when write is disabled"
        );
    }
}

#[tokio::test]
async fn write_enabled_lists_both_tools() {
    let s = server(true, Uuid::now_v7(), "http://unused.invalid");
    let names = list_names(&s).await;
    assert!(names.contains(&"add_route".to_string()));
    assert!(names.contains(&"apply_plan".to_string()));
}

#[tokio::test]
async fn enabled_add_route_dispatches_to_gateway() {
    let mock = MockServer::start_async().await;
    let route_id = Uuid::now_v7();
    let m = mock
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/routes")
                .header("authorization", "Bearer tt_live_operator");
            then.status(201).json_body(json!({
                "id": route_id.to_string(),
                "name": "all->cheap",
                "priority": 100,
                "enabled": true,
                "when": {},
                "then": { "target_model": "gpt-4o-mini" }
            }));
        })
        .await;

    let s = server(true, Uuid::now_v7(), &mock.base_url());
    let v = serde_json::to_value(
        s.dispatch(call_req(
            "add_route",
            json!({ "name": "all->cheap", "then": { "target_model": "gpt-4o-mini" } }),
        ))
        .await,
    )
    .unwrap();
    m.assert_async().await;
    assert!(v["error"].is_null(), "no error expected: {v}");
    assert_eq!(v["result"]["created"], true);
    assert_eq!(v["result"]["route"]["id"], route_id.to_string());
}

#[tokio::test]
async fn enabled_apply_plan_dispatches_to_cloud() {
    let mock = MockServer::start_async().await;
    let plan_id = Uuid::now_v7();
    let org = Uuid::now_v7();
    let m = mock
        .mock_async(|when, then| {
            when.method(POST)
                .path(format!("/v1/admin/plans/{plan_id}/apply"))
                .query_param("org_id", org.to_string());
            then.status(200).json_body(json!({
                "id": plan_id.to_string(),
                "status": "applied"
            }));
        })
        .await;

    let s = server(true, org, &mock.base_url());
    let v = serde_json::to_value(
        s.dispatch(call_req(
            "apply_plan",
            json!({ "plan_id": plan_id.to_string() }),
        ))
        .await,
    )
    .unwrap();
    m.assert_async().await;
    assert!(v["error"].is_null(), "no error expected: {v}");
    assert_eq!(v["result"]["applied"], true);
    assert_eq!(v["result"]["plan"]["status"], "applied");
}

#[tokio::test]
async fn enabled_add_route_rejects_foreign_org() {
    let mock = MockServer::start_async().await;
    let m = mock
        .mock_async(|when, then| {
            when.method(POST).path("/v1/routes");
            then.status(201).body("{}");
        })
        .await;
    let bound = Uuid::now_v7();
    let other = Uuid::now_v7();
    let s = server(true, bound, &mock.base_url());
    let v = serde_json::to_value(
        s.dispatch(call_req(
            "add_route",
            json!({
                "name": "x",
                "org_id": other.to_string(),
                "then": { "target_model": "gpt-4o-mini" }
            }),
        ))
        .await,
    )
    .unwrap();
    assert_eq!(
        v["error"]["code"], -32001,
        "a foreign-org add_route must be unauthorized"
    );
    assert_eq!(m.hits_async().await, 0, "gateway must not be called");
}

#[tokio::test]
async fn enabled_apply_plan_rejects_foreign_org() {
    let mock = MockServer::start_async().await;
    let plan_id = Uuid::now_v7();
    let m = mock
        .mock_async(|when, then| {
            when.method(POST)
                .path(format!("/v1/admin/plans/{plan_id}/apply"));
            then.status(200).body("{}");
        })
        .await;
    let bound = Uuid::now_v7();
    let other = Uuid::now_v7();
    let s = server(true, bound, &mock.base_url());
    let v = serde_json::to_value(
        s.dispatch(call_req(
            "apply_plan",
            json!({ "plan_id": plan_id.to_string(), "org_id": other.to_string() }),
        ))
        .await,
    )
    .unwrap();
    assert_eq!(v["error"]["code"], -32001);
    assert_eq!(m.hits_async().await, 0, "cloud must not be called");
}
