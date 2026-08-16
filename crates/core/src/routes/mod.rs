//! HTTP route handlers. One file per endpoint family — keeps each well under
//! the 800-line cap enforced by pre-edit-guard.
use axum::http::HeaderMap;

use crate::error::{ApiError, ApiResult};

pub mod account_purge;
pub mod agent_run;
pub(crate) mod agent_run_budget;
pub(crate) mod agent_run_store;
pub mod agent_run_transcript;
pub mod batches;
pub mod capabilities;
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
pub mod request_preflight;
pub mod responses;
pub mod routes_api;
pub mod spend_api;
pub mod sse;
pub mod workflow_releases;
pub mod workflow_runs;
pub mod workflow_variables;
pub mod workflow_versions;
pub mod workflows;
const IDEMPOTENCY_HEADER: &str = "idempotency-key";
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;

/// Parse the standard logical-request identity shared by workflows, direct
/// provider calls, and agent turns. Raw values are retained only for the
/// request lifetime; persistence paths store domain-separated digests.
pub(crate) fn idempotency_key_from_headers(headers: &HeaderMap) -> ApiResult<Option<String>> {
    let Some(value) = headers.get(IDEMPOTENCY_HEADER) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| ApiError::InvalidRequest("Idempotency-Key must be visible text".into()))?;
    if value.trim().is_empty()
        || value.len() > MAX_IDEMPOTENCY_KEY_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(ApiError::InvalidRequest(format!(
            "Idempotency-Key must be 1..={MAX_IDEMPOTENCY_KEY_BYTES} visible bytes"
        )));
    }
    Ok(Some(value.to_owned()))
}

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
