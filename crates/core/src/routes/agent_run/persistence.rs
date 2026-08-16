//! Resumable L1 state and durable run projections.

use super::*;
// ---------------------------------------------------------------------------
// Persisted run record + L1-backed run store helpers (slice 1b Task 2)
//
// Wired into the create/get handlers by slice-1b Tasks 3-4 (`create_run`
// persists a paused run; `get_run` fetches it). Resume (Task 5) consumes them
// too.
// ---------------------------------------------------------------------------

/// TTL for a persisted run record. A paused run is GETtable/resumable for this
/// long; after it the L1 store evicts the record (one hour).
const RUN_TTL_SECS: u64 = 3600;

/// Non-secret routing config carried across a pause so resume turns route
/// consistently. NEVER includes credentials.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredRouting {
    pub provider_pin: Option<String>,
    pub forced_route: Option<String>,
    pub tag: Option<String>,
}

/// Non-secret summarize policy resolved once from the run's (turn-0) route and
/// persisted with the run so resume drives the same policy. Tiny config only.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct SummarizeConfig {
    pub keep_recent_pairs: u32,
    pub clear_at_least_tokens: u32,
}

/// The full resumable run state persisted to the L1 store. NO secrets — only
/// the conversation transcript and the non-secret routing config.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredRun {
    pub id: uuid::Uuid,
    pub org_id: uuid::Uuid,
    pub status: RunStatus,
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<tt_shared::messages::Tool>,
    pub max_turns: u32,
    pub turns_done: u32,
    pub usage: RunUsage,
    pub pending_tool_calls: Vec<tt_shared::messages::ToolCall>,
    pub routing: StoredRouting,
    /// Tool-block watermark: count of leading `Message::Tool` blocks already
    /// summarized. Restored on resume so each block is summarized at most once.
    #[serde(default)]
    pub summarized_upto: u32,
    /// Accrued summarizer measurement tax (USD). `#[serde(default)]` for
    /// cross-deploy resume back-compat (a pre-2c-1 record has no key).
    #[serde(default)]
    pub summarizer_tax_usd: Option<f64>,
    /// Total cash settled by the run-scoped provider ledger. `None` marks a
    /// legacy record whose primary + summarizer totals must seed resume.
    #[serde(default)]
    pub run_budget_settled_micros: Option<u64>,
    /// Settled provider-attempt evidence retained across pause/resume.
    #[serde(default)]
    pub run_budget_components: Vec<tt_shared::context::RunBudgetComponent>,
    /// The run's pinned summarize policy (turn-0 route). `None` ⇒ summarize off.
    #[serde(default)]
    pub summarize: Option<SummarizeConfig>,
    /// Cost-admission guard (USD) carried across pause so resume applies the
    /// same cap. It is not a provider settlement guarantee.
    /// `#[serde(default)]` preserves cross-deploy back-compat.
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
    /// Terminal stop reason; `None` for paused (requires_action) runs.
    /// `#[serde(default)]` for cross-deploy back-compat.
    #[serde(default)]
    pub stop_reason: Option<StopReason>,
}

/// L1 key for a run record, scoped by org so a fetch with the wrong org misses.
pub(crate) fn run_key(org_id: uuid::Uuid, run_id: uuid::Uuid) -> String {
    format!("tt:runs:{org_id}:{run_id}")
}

impl StoredRun {
    /// Derive the HTTP `Run` view from a stored record (the `requires_action`
    /// response body). `turns` is the turns completed so far; no note.
    pub(crate) fn to_run(&self) -> Run {
        Run {
            id: self.id,
            status: self.status,
            messages: self.messages.clone(),
            turns: self.turns_done,
            usage: self.usage.clone(),
            note: None,
            summarizer_tax_usd: self.summarizer_tax_usd,
            stop_reason: self.stop_reason,
        }
    }
}

/// Persist (overwrite) a run record with the run TTL.
pub(crate) async fn store_run(
    cache: &dyn tt_cache::L1Cache,
    run: &StoredRun,
) -> Result<(), ApiError> {
    let bytes =
        serde_json::to_vec(run).map_err(|e| ApiError::Internal(format!("run serialize: {e}")))?;
    cache
        .set(&run_key(run.org_id, run.id), &bytes, RUN_TTL_SECS)
        .await
        .map_err(|e| ApiError::Internal(format!("run store: {e}")))?;
    Ok(())
}

/// Fetch a run record scoped by (org, id). `None` when absent/expired.
pub(crate) async fn fetch_run(
    cache: &dyn tt_cache::L1Cache,
    org_id: uuid::Uuid,
    run_id: uuid::Uuid,
) -> Result<Option<StoredRun>, ApiError> {
    match cache
        .get(&run_key(org_id, run_id))
        .await
        .map_err(|e| ApiError::Internal(format!("run fetch: {e}")))?
    {
        Some(bytes) => {
            Ok(Some(serde_json::from_slice(&bytes).map_err(|e| {
                ApiError::Internal(format!("run deserialize: {e}"))
            })?))
        }
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Durable-record view types + `GET /v1/agent/runs` list (W0b Task 6)
// ---------------------------------------------------------------------------

/// Lightweight durable summary of a run — only the fields persisted in
/// Postgres. The transcript (`messages`) is NOT included; it only lives in
/// Redis while the run is in-flight. Callers that need the transcript should
/// `GET /v1/agent/runs/:id` while the run is still within its Redis TTL.
#[derive(Debug, serde::Serialize)]
pub struct DurableRunView {
    pub id: uuid::Uuid,
    /// Snake-case status string from the DB: `running`, `completed`,
    /// `incomplete`, `failed`, or `requires_action`.
    pub status: String,
    pub model: String,
    pub turns: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
    pub cost_usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

/// Response body for `GET /v1/agent/runs` — an OpenAI-style `list` envelope.
#[derive(Debug, serde::Serialize)]
pub struct ListRunsResponse {
    pub object: &'static str,
    pub data: Vec<DurableRunView>,
}

/// Map a durable [`AgentRunRecord`] (read from Postgres) to a full [`Run`]
/// view for `GET /v1/agent/runs/:id` Postgres fallback.
///
/// The transcript (`messages`) is NOT durable; only available while the run
/// is in Redis. This returns `messages: vec![]` and a `note` explaining the
/// absence. All identity + terminal-state fields are carried over.
pub(crate) fn durable_record_to_run(rec: &crate::routes::agent_run_store::AgentRunRecord) -> Run {
    let status = match rec.status.as_str() {
        "completed" => RunStatus::Completed,
        "incomplete" => RunStatus::Incomplete,
        "failed" => RunStatus::Failed,
        "requires_action" => RunStatus::RequiresAction,
        // "running" or unknown: the run never reached a terminal state (e.g.
        // it crashed without a `finish_agent_run` call). Map to Incomplete so
        // the caller sees a coherent non-running status.
        _ => RunStatus::Incomplete,
    };
    let stop_reason = rec.stop_reason.as_deref().and_then(|s| match s {
        "max_turns" => Some(crate::routes::agent_run_budget::StopReason::MaxTurns),
        "budget_exhausted" => Some(crate::routes::agent_run_budget::StopReason::BudgetExhausted),
        "budget_breach" => Some(crate::routes::agent_run_budget::StopReason::BudgetBreach),
        "runaway" => Some(crate::routes::agent_run_budget::StopReason::Runaway),
        _ => None,
    });
    Run {
        id: rec.id,
        status,
        messages: vec![],
        turns: rec.turns as u32,
        usage: RunUsage {
            cost_usd: rec.cost_usd,
            ..Default::default()
        },
        note: Some(
            "durable view: transcript not available (Redis TTL expired or run in-flight)".into(),
        ),
        summarizer_tax_usd: None,
        stop_reason,
    }
}

/// `GET /v1/agent/runs` — list up to 50 of the caller's agent runs, newest
/// first. Returns durable identity + terminal state only (no transcript —
/// that only lives in Redis). Requires Postgres (503 if absent) and a real
/// authenticated org (401 for anonymous / dogfood callers).
pub async fn list_runs(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
) -> ApiResult<Json<ListRunsResponse>> {
    let org = match ctx {
        Some(Extension(c)) if c.org_id != crate::DOGFOOD_ORG_ID => c.org_id,
        _ => return Err(ApiError::Unauthorized),
    };
    let pool = state.db_pool.as_ref().ok_or_else(|| {
        ApiError::ServiceUnavailable(
            "agent run list requires a Postgres pool (none configured)".into(),
        )
    })?;
    const LIST_LIMIT: i64 = 50;
    let records = crate::routes::agent_run_store::list_agent_runs(pool, org, LIST_LIMIT).await;
    let data = records
        .into_iter()
        .map(|r| DurableRunView {
            id: r.id,
            status: r.status,
            model: r.model,
            turns: r.turns,
            max_turns: r.max_turns,
            max_cost_usd: r.max_cost_usd,
            cost_usd: r.cost_usd,
            stop_reason: r.stop_reason,
            tag: r.tag,
        })
        .collect();
    Ok(Json(ListRunsResponse {
        object: "list",
        data,
    }))
}
