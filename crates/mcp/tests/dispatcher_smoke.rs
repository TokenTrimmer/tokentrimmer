//! In-process dispatch: initialize, tools/list, tools/call(find_route_for).

use serde_json::json;
use tt_mcp::{protocol::JsonRpcRequest, tools::find_route_for::FindRouteForTool, Server};

#[tokio::test]
async fn lifecycle_initialize_list_call() {
    let mut server = Server::new();
    server.tools.register(Box::new(FindRouteForTool));

    let init = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        method: "initialize".into(),
        params: json!({}),
        id: Some(json!(1)),
    };
    let r = server.dispatch(init).await;
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["result"]["serverInfo"]["name"], "tt-mcp");

    let list = JsonRpcRequest {
        jsonrpc: "2.0".into(), method: "tools/list".into(),
        params: json!({}), id: Some(json!(2)),
    };
    let r = server.dispatch(list).await;
    let v = serde_json::to_value(&r).unwrap();
    let tools = v["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "find_route_for");

    let call = JsonRpcRequest {
        jsonrpc: "2.0".into(), method: "tools/call".into(),
        params: json!({ "name": "find_route_for", "arguments": { "task_description": "classify this email as spam" } }),
        id: Some(json!(3)),
    };
    let r = server.dispatch(call).await;
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["result"]["model"], "claude-haiku-4-5");
}
