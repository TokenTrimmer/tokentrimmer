//! End-to-end dispatch of the cost-control tools through the MCP server:
//! `tools/list` advertises them with valid object schemas, `tools/call`
//! reaches the backend, and `set_cost_limit` refuses to target another org.

use std::sync::Arc;

use serde_json::json;
use tt_mcp::{
    cost::{CostControlBackend, UnconfiguredBackend},
    protocol::JsonRpcRequest,
    tools::cost_control::{CheckBudgetRemainingTool, GetSpendTodayTool, SetCostLimitTool},
    Server,
};
use uuid::Uuid;

fn server_with_cost_tools(org: Uuid) -> Server {
    let backend: Arc<dyn CostControlBackend> = Arc::new(UnconfiguredBackend);
    let mut s = Server::new();
    s.tools.register(Box::new(GetSpendTodayTool {
        backend: backend.clone(),
        org_id: org,
    }));
    s.tools.register(Box::new(CheckBudgetRemainingTool {
        backend: backend.clone(),
        org_id: org,
    }));
    s.tools.register(Box::new(SetCostLimitTool {
        backend,
        org_id: org,
    }));
    s
}

fn call(name: &str, args: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".into(),
        method: "tools/call".into(),
        params: json!({ "name": name, "arguments": args }),
        id: Some(json!(1)),
    }
}

#[tokio::test]
async fn tools_list_advertises_all_three_with_object_schemas() {
    let server = server_with_cost_tools(Uuid::now_v7());
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        method: "tools/list".into(),
        params: json!({}),
        id: Some(json!(1)),
    };
    let resp = server.dispatch(req).await;
    let v = serde_json::to_value(&resp).unwrap();
    let tools = v["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in [
        "get_spend_today",
        "check_budget_remaining",
        "set_cost_limit",
    ] {
        assert!(names.contains(&expected), "missing tool {expected}");
    }
    for t in tools {
        assert_eq!(
            t["inputSchema"]["type"], "object",
            "tool {} must have an object inputSchema",
            t["name"]
        );
    }
}

#[tokio::test]
async fn get_spend_today_dispatches_and_is_org_scoped() {
    let org = Uuid::now_v7();
    let server = server_with_cost_tools(org);
    let resp = server.dispatch(call("get_spend_today", json!({}))).await;
    let v = serde_json::to_value(&resp).unwrap();
    assert!(v["error"].is_null(), "no error expected: {v}");
    assert_eq!(v["result"]["org_id"], org.to_string());
    assert_eq!(
        v["result"]["configured"], false,
        "unconfigured backend must be marked, not faked"
    );
}

#[tokio::test]
async fn check_budget_remaining_dispatches() {
    let org = Uuid::now_v7();
    let server = server_with_cost_tools(org);
    let resp = server
        .dispatch(call("check_budget_remaining", json!({})))
        .await;
    let v = serde_json::to_value(&resp).unwrap();
    assert_eq!(v["result"]["org_id"], org.to_string());
    assert!(v["result"]["remaining_usd"].is_null());
}

#[tokio::test]
async fn set_cost_limit_for_own_org_dispatches() {
    let org = Uuid::now_v7();
    let server = server_with_cost_tools(org);
    let resp = server
        .dispatch(call("set_cost_limit", json!({ "monthly_cap_usd": 42.0 })))
        .await;
    let v = serde_json::to_value(&resp).unwrap();
    assert!(v["error"].is_null(), "no error expected: {v}");
    assert_eq!(v["result"]["org_id"], org.to_string());
    assert_eq!(v["result"]["monthly_cap_usd"], 42.0);
    assert_eq!(
        v["result"]["applied"], false,
        "unconfigured backend must not claim to persist"
    );
}

#[tokio::test]
async fn set_cost_limit_for_another_org_is_unauthorized() {
    let bound = Uuid::now_v7();
    let other = Uuid::now_v7();
    let server = server_with_cost_tools(bound);
    let resp = server
        .dispatch(call(
            "set_cost_limit",
            json!({ "org_id": other.to_string(), "monthly_cap_usd": 1.0 }),
        ))
        .await;
    let v = serde_json::to_value(&resp).unwrap();
    assert!(
        v["result"].is_null(),
        "a foreign-org mutation must not succeed"
    );
    assert_eq!(
        v["error"]["code"], -32001,
        "cross-org set_cost_limit must be unauthorized"
    );
}
