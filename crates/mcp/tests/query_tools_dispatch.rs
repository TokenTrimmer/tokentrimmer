//! End-to-end dispatch of the **query-offload** tools (`run_query`,
//! `list_datasets`) through the MCP server, exercising the config-is-the-gate
//! posture and the full request path over a real tempdir CSV.
//!
//! SAFETY contract under test (mirrors `write_tools_dispatch.rs`):
//! - no query config (the default): `tools/list` omits both tools, and
//!   `tools/call` on either returns `MethodNotFound` (-32601) — NOT
//!   `Unauthorized` (-32001); the query-ledger resource is absent too.
//! - with a loaded config: both tools are listed; `run_query` returns ONLY
//!   the computed aggregate (never raw rows beyond it); `list_datasets`
//!   leaks no root path, file path, or DSN env-var name; the query-ledger
//!   resource serves the appended execution lines.

use std::sync::Arc;

use serde_json::json;
use tt_mcp::protocol::JsonRpcRequest;
use tt_mcp::query::cache::QueryCache;
use tt_mcp::query::ledger::ExecutionLedger;
use tt_mcp::query::QueryConfig;
use tt_mcp::Server;

fn req(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".into(),
        method: method.into(),
        params,
        id: Some(json!(1)),
    }
}

fn call_req(name: &str, args: serde_json::Value) -> JsonRpcRequest {
    req("tools/call", json!({ "name": name, "arguments": args }))
}

async fn list_names(server: &Server) -> Vec<String> {
    let v = serde_json::to_value(server.dispatch(req("tools/list", json!({}))).await).unwrap();
    v["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

/// Build a server whose query tools are registered through the REAL config
/// loader over a tempdir CSV (the only public path to a HandleRegistry).
async fn server_with_config(dir: &tempfile::TempDir) -> (Server, std::path::PathBuf) {
    std::fs::write(
        dir.path().join("orders.csv"),
        "region,amount\nemea,10.5\napac,2\nemea,4.5\nus,1\n",
    )
    .unwrap();
    let toml = format!(
        r#"
        [files]
        root = "{}"
        [[dataset]]
        alias = "orders"
        kind = "file"
        path = "orders.csv"
        format = "csv"
        description = "test orders"
        "#,
        dir.path().display()
    );
    let cfg = QueryConfig::from_toml_str(&toml, &|_| None).unwrap();
    let registry = Arc::new(cfg.build_registry().await.unwrap());
    let ledger_path = dir.path().join("query-ledger.jsonl");
    let mut server = Server::new();
    server.register_query_tools(
        registry,
        Arc::new(QueryCache::default()),
        Arc::new(ExecutionLedger::new(&ledger_path)),
        cfg.limits(),
    );
    (server, ledger_path)
}

#[tokio::test]
async fn no_config_omits_query_tools_from_list() {
    let server = Server::new();
    let names = list_names(&server).await;
    assert!(!names.contains(&"run_query".to_string()));
    assert!(!names.contains(&"list_datasets".to_string()));
}

#[tokio::test]
async fn no_config_tools_call_is_method_not_found_not_unauthorized() {
    let server = Server::new();
    for name in ["run_query", "list_datasets"] {
        let v = serde_json::to_value(
            server
                .dispatch(call_req(name, json!({ "dataset": "orders" })))
                .await,
        )
        .unwrap();
        assert_eq!(
            v["error"]["code"], -32601,
            "{name} must be MethodNotFound without a query config"
        );
        assert_ne!(
            v["error"]["code"], -32001,
            "{name} must NOT be Unauthorized"
        );
    }
}

#[tokio::test]
async fn no_config_omits_the_query_ledger_resource() {
    let server = Server::new();
    let v = serde_json::to_value(server.dispatch(req("resources/list", json!({}))).await).unwrap();
    let uris: Vec<&str> = v["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap())
        .collect();
    assert!(!uris.iter().any(|u| u.contains("query-ledger")));
}

#[tokio::test]
async fn with_config_lists_both_tools_and_the_resource() {
    let dir = tempfile::tempdir().unwrap();
    let (server, _) = server_with_config(&dir).await;
    let names = list_names(&server).await;
    assert!(names.contains(&"run_query".to_string()));
    assert!(names.contains(&"list_datasets".to_string()));

    let v = serde_json::to_value(server.dispatch(req("resources/list", json!({}))).await).unwrap();
    let uris: Vec<String> = v["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap().to_string())
        .collect();
    assert!(uris.contains(&"mcp://tokentrimmer/query-ledger/recent".to_string()));
}

/// Full e2e: dispatch run_query over a real CSV → ONLY the computed
/// aggregate comes back (3 group rows, not 4 data rows), then the ledger
/// resource serves the metered line.
#[tokio::test]
async fn e2e_run_query_returns_only_the_computed_aggregate() {
    let dir = tempfile::tempdir().unwrap();
    let (server, ledger_path) = server_with_config(&dir).await;

    let v = serde_json::to_value(
        server
            .dispatch(call_req(
                "run_query",
                json!({
                    "dataset": "orders",
                    "aggregate": { "op": "sum", "column": "amount", "group_by": ["region"] }
                }),
            ))
            .await,
    )
    .unwrap();
    assert!(v["error"].is_null(), "no error expected: {v}");
    let result = &v["result"];
    assert_eq!(result["columns"], json!(["region", "sum_amount"]));
    assert_eq!(
        result["rows"],
        json!([["apac", 2.0], ["emea", 15.0], ["us", 1.0]]),
        "only the computed aggregate enters context"
    );
    assert_eq!(result["row_count"], 3);
    assert_eq!(result["execution"]["rows_scanned"], 4);
    assert_eq!(result["execution"]["unit"], "execution");
    assert_eq!(result["execution"]["priced_as_tokens"], json!(false));

    // The raw data rows must NOT be anywhere in the response.
    let serialized = serde_json::to_string(&v).unwrap();
    assert!(!serialized.contains("10.5"), "raw row leaked: {serialized}");

    // The execution was metered; the resource serves the line.
    let r = serde_json::to_value(
        server
            .dispatch(req(
                "resources/read",
                json!({ "uri": "mcp://tokentrimmer/query-ledger/recent" }),
            ))
            .await,
    )
    .unwrap();
    let text = r["result"]["text"].as_str().unwrap();
    assert_eq!(text.lines().count(), 1);
    let line: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(line["dataset"], "orders");
    assert_eq!(line["unit"], "execution");
    assert_eq!(line["priced_as_tokens"], json!(false));
    assert!(ledger_path.exists());
}

/// list_datasets must enumerate WITHOUT leaking the root path, file path,
/// or any DSN env-var name.
#[tokio::test]
async fn list_datasets_leaks_no_paths_or_dsn_material() {
    let dir = tempfile::tempdir().unwrap();
    let (server, _) = server_with_config(&dir).await;

    let v =
        serde_json::to_value(server.dispatch(call_req("list_datasets", json!({}))).await).unwrap();
    assert!(v["error"].is_null(), "no error expected: {v}");
    let ds = &v["result"]["datasets"][0];
    assert_eq!(ds["alias"], "orders");
    assert_eq!(ds["columns"], json!(["region", "amount"]));

    let serialized = serde_json::to_string(&v["result"]).unwrap();
    let root = dir.path().canonicalize().unwrap();
    assert!(
        !serialized.contains(root.to_str().unwrap()),
        "root path leaked: {serialized}"
    );
    assert!(
        !serialized.contains(dir.path().to_str().unwrap()),
        "root path leaked: {serialized}"
    );
    assert!(
        !serialized.contains("orders.csv"),
        "file path leaked: {serialized}"
    );
    assert!(
        !serialized.contains("dsn"),
        "dsn material leaked: {serialized}"
    );
}

/// A smuggled blob through dispatch is rejected with InvalidParams (-32602)
/// before any execution.
#[tokio::test]
async fn e2e_smuggled_blob_is_invalid_params() {
    let dir = tempfile::tempdir().unwrap();
    let (server, ledger_path) = server_with_config(&dir).await;

    let blob = "region,amount\n".repeat(1000); // ~14 KiB CSV
    let v = serde_json::to_value(
        server
            .dispatch(call_req(
                "run_query",
                json!({ "dataset": "orders", "query": blob }),
            ))
            .await,
    )
    .unwrap();
    assert_eq!(
        v["error"]["code"], -32602,
        "smuggling must be InvalidParams: {v}"
    );
    assert!(
        !ledger_path.exists(),
        "nothing may be executed or metered for a rejected call"
    );
}
