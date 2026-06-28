//! HTTP route handlers. One file per endpoint family — keeps each well under
//! the 800-line cap enforced by pre-edit-guard.

pub mod agent_run;
pub(crate) mod agent_run_budget;
pub(crate) mod agent_run_store;
pub mod batches;
pub mod chat;
pub mod embeddings;
pub(crate) mod gateway_tools;
pub mod health;
pub mod messages;
pub mod metrics;
pub mod models;
pub mod panel;
pub mod preview;
pub mod ready;
pub mod responses;
pub mod routes_api;
pub mod sse;
pub mod workflows;

/// Graft the `tokentrimmer.panel` attribution from a chat-completions response body
/// onto a transcoded target-shape body. The chat handler grafts `tokentrimmer.panel`
/// as a top-level key (chat.rs); the transcoders deserialize into the typed
/// `ChatCompletionResponse` (which drops unknown top-level keys), so we re-extract it
/// from the raw bytes here and re-attach it to the target body. No-op when absent
/// (off-by-default) or when `out` is not a JSON object.
pub(crate) fn graft_tokentrimmer_panel(out: &mut serde_json::Value, chat_body: &[u8]) {
    let Ok(val) = serde_json::from_slice::<serde_json::Value>(chat_body) else {
        return;
    };
    let Some(panel) = val
        .get("tokentrimmer")
        .and_then(|t| t.get("panel"))
        .cloned()
    else {
        return;
    };
    if let Some(obj) = out.as_object_mut() {
        obj.insert("tokentrimmer".into(), serde_json::json!({ "panel": panel }));
    }
}
