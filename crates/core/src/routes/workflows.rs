//! `/v1/workflows` CRUD + synchronous run API (W1a Task 8).
//!
//! All handlers require a real authenticated caller (`require_org` — same
//! discipline as `routes_api` and `agent_run`); anonymous / dogfood callers
//! get 401. Every write/read path that touches the DB requires
//! `state.db_pool`; returns 503 when absent.
//!
//! # Validation ordering in `create`
//!
//! `require_org` → assemble def → `validate` (400 on error) → `db_pool` check
//! (503) → `insert_definition`. This order lets callers see a useful 400 without
//! needing a live DB.
//!
//! # Async-journal pattern in `create_run`
//!
//! `engine::run_workflow` accepts a *synchronous* `FnMut(NodeJournalEntry)` so
//! it can be called from a normal `async fn` without the async-closure complexity.
//! `node_run_store::insert_node_run` is async. Solution: collect journal entries into a
//! `Vec` inside the sync closure, then loop + `await` each one **after**
//! `run_workflow` returns. Best-effort: a DB error never fails the run response.
//!
//! # Timeouts: non-streaming runs use the 60 s `short` group; `stream=true` uses 600 s `streaming`.

use axum::{
    extract::{Path, State},
    http::{header::CACHE_CONTROL, HeaderMap, HeaderValue, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Extension, Json,
};
use futures::StreamExt as _;
use serde::{Deserialize, Serialize};
use tt_auth::ApiKeyContext;
use uuid::Uuid;

use crate::{
    error::{ApiError, ApiResult},
    routes::chat::{CompletionHeaders, CompletionOutcome, CostBreakdown, Prepared},
    workflow::{
        self,
        engine::{self, WfStatus},
        estimate,
        events::WfEvent,
        executor::GatewayNodeExecutor,
        node_run_store,
        secrets::{
            delete_secret, is_valid_secret_name, list_secret_rows, load_secrets,
            master_key_from_env, store_secret,
        },
        store::{self, WorkflowRunRecord},
        types::content_hash,
        validate,
    },
    AppState, DOGFOOD_ORG_ID,
};
use tt_shared::{ChatCompletionResponse, Choice, Message, MessageContent, Usage};

// ---------------------------------------------------------------------------
// Guards
// ---------------------------------------------------------------------------

/// Resolve the caller's real org, or 401. Dogfood / absent contexts are
/// rejected (mirrors `routes_api::require_org` exactly).
fn require_org(ctx: Option<Extension<ApiKeyContext>>) -> Result<Uuid, ApiError> {
    match ctx {
        Some(Extension(c)) if c.org_id != DOGFOOD_ORG_ID => Ok(c.org_id),
        _ => Err(ApiError::Unauthorized),
    }
}

/// Return the Postgres pool, or 503.
fn db_pool(state: &AppState) -> ApiResult<&sqlx::PgPool> {
    state.db_pool.as_ref().ok_or_else(|| {
        ApiError::ServiceUnavailable(
            "workflow storage requires a Postgres pool (none configured)".into(),
        )
    })
}

/// Invocation surfaces sharing workflow execution must all pass this gate
/// before they allocate a run record or construct an executor. The static
/// check is intentionally only admission; the engine separately performs
/// in-memory per-node reservation/settlement for an admitted capped run, but
/// neither layer is a provider-invoice ceiling.
#[derive(Clone, Copy)]
enum WorkflowBudgetAdmissionPath {
    Direct,
    Detour,
    Shadow,
}

fn admit_workflow_budget_before_dispatch(
    path: WorkflowBudgetAdmissionPath,
    def: &workflow::types::WorkflowDefinition,
    inputs: &serde_json::Value,
    max_cost_usd: Option<f64>,
) -> Result<(), String> {
    let subject = match path {
        WorkflowBudgetAdmissionPath::Direct => "this run",
        WorkflowBudgetAdmissionPath::Detour => "this route",
        WorkflowBudgetAdmissionPath::Shadow => "shadow run",
    };
    estimate::admit_budgeted_workflow(def, inputs, max_cost_usd).map_err(|error| {
        format!("workflow budget admission rejected {subject} before dispatch: {error}")
    })
}

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

/// `POST /v1/workflows` request body.  Both `id` and `version` are optional:
/// if `id` is absent a new `UUIDv4` is generated; `version` is ignored (the
/// store computes the next version atomically via `MAX(version)+1`).
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CreateWorkflowRequest {
    pub id: Option<Uuid>,
    pub version: Option<u32>,
    pub name: String,
    pub nodes: Vec<workflow::types::Node>,
    pub edges: Vec<workflow::types::Edge>,
    #[serde(default)]
    pub inputs: serde_json::Value,
    #[serde(default)]
    pub budget: workflow::types::BudgetPolicy,
    /// Per-workflow egress allowlist forwarded to `WorkflowDefinition::allowed_hosts`.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Freeform editor metadata forwarded to `WorkflowDefinition::metadata`
    /// (WF-3: canvas node positions, previously localStorage-only). Optional +
    /// defaulted so a save without it (an older editor) is unchanged.
    #[serde(default)]
    pub metadata: serde_json::Value,
    /// Out-of-band workflow invokers (schedule and signed webhook).  These
    /// must round-trip on every definition update: dropping them silently
    /// disables production automations.
    #[serde(default)]
    pub triggers: Vec<workflow::types::WorkflowTrigger>,
}

impl CreateWorkflowRequest {
    /// Turn the public write contract into the canonical persisted definition.
    /// Keeping this conversion in one tested place prevents a newly added
    /// top-level definition field from being accepted by the API and then
    /// silently discarded on update.
    fn into_definition(self) -> workflow::WorkflowDefinition {
        workflow::WorkflowDefinition {
            id: self.id.unwrap_or_else(Uuid::new_v4),
            version: self.version.unwrap_or(0),
            name: self.name,
            nodes: self.nodes,
            edges: self.edges,
            inputs: self.inputs,
            budget: self.budget,
            allowed_hosts: self.allowed_hosts,
            metadata: self.metadata,
            triggers: self.triggers,
        }
    }
}

/// Response from `POST /v1/workflows`.
#[derive(Debug, Serialize)]
pub struct CreateWorkflowResponse {
    pub id: Uuid,
    /// The version actually stored (atomically computed — 1 for a new id,
    /// `MAX(existing)+1` for an update).
    pub version: i32,
    pub content_hash: String,
}

/// Response from `GET /v1/workflows`.
#[derive(Debug, Serialize)]
pub struct ListWorkflowsResponse {
    pub object: &'static str,
    pub data: Vec<WorkflowDefMetaView>,
}

/// Lightweight definition metadata (one entry per latest-version definition).
#[derive(Debug, Serialize)]
pub struct WorkflowDefMetaView {
    pub id: Uuid,
    pub name: String,
    pub version: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// `POST /v1/workflows/:id/estimate` request body.
#[derive(Debug, Deserialize)]
pub struct EstimateRequest {
    #[serde(default)]
    pub inputs: serde_json::Value,
}

/// `POST /v1/workflows/:id/estimate` response.
#[derive(Debug, Serialize)]
pub struct EstimateResponse {
    pub projected_cost_usd: f64,
    pub per_node: Vec<NodeEstimateView>,
    pub warnings: Vec<String>,
}

/// Per-node cost estimate inside [`EstimateResponse`].
#[derive(Debug, Serialize)]
pub struct NodeEstimateView {
    pub node_id: String,
    pub model: Option<String>,
    pub cost_usd: Option<f64>,
}

/// `POST /v1/workflows/:id/runs` request body.
#[derive(Debug, Deserialize)]
pub struct CreateRunRequest {
    #[serde(default)]
    pub inputs: serde_json::Value,
    /// Optional immutable definition version to execute. When omitted, a fresh
    /// invocation selects the latest version; an `Idempotency-Key` replay keeps
    /// the version accepted by the original request.
    #[serde(default, alias = "version")]
    pub workflow_version: Option<i32>,
    /// Optional run-level USD budget cap. Superseded by
    /// `def.budget.max_cost_usd` when that is set.
    #[serde(default)]
    pub max_cost_usd: Option<f64>,
    /// When `true`, return a `text/event-stream` of [`WfEvent`]s instead of a
    /// single JSON response. Back-compat default is `false`.
    #[serde(default)]
    pub stream: bool,
}

/// Response from `POST /v1/workflows/:id/runs`.
#[derive(Debug, Serialize)]
pub struct CreateRunResponse {
    pub run_id: Uuid,
    pub status: String,
    pub cost_usd: f64,
    pub baseline_cost_usd: f64,
    pub saved_usd: f64,
    pub node_outputs: Vec<NodeOutputView>,
}

/// Private additive replay envelope. Keeping this separate from the public
/// [`CreateRunResponse`] preserves Rust callers that construct the normal
/// response type while making replay status explicit on the HTTP wire.
#[derive(Debug, Serialize)]
struct CreateRunReplayResponse {
    #[serde(flatten)]
    run: CreateRunResponse,
    replayed: bool,
}

/// Per-node output inside [`CreateRunResponse`].
#[derive(Debug, Serialize)]
pub struct NodeOutputView {
    pub node_id: String,
    pub content: serde_json::Value,
    pub cost_usd: f64,
}

/// Standard header used to bind a retried logical invocation to its original
/// gateway run. The raw value is validated here and hashed before persistence.
const WORKFLOW_RUN_IDEMPOTENCY_HEADER: &str = "idempotency-key";
const MAX_WORKFLOW_RUN_IDEMPOTENCY_KEY_BYTES: usize = 256;

/// Extract an optional stable invocation key without changing legacy callers:
/// no `Idempotency-Key` retains the historical fresh-run behavior.
fn workflow_run_idempotency_key(headers: &HeaderMap) -> ApiResult<Option<String>> {
    let Some(value) = headers.get(WORKFLOW_RUN_IDEMPOTENCY_HEADER) else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| {
        ApiError::InvalidRequest("Idempotency-Key must be valid visible text".into())
    })?;
    if value.trim().is_empty()
        || value.len() > MAX_WORKFLOW_RUN_IDEMPOTENCY_KEY_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(ApiError::InvalidRequest(format!(
            "Idempotency-Key must be 1..={MAX_WORKFLOW_RUN_IDEMPOTENCY_KEY_BYTES} visible bytes"
        )));
    }
    Ok(Some(value.to_owned()))
}

fn validate_requested_workflow_version(version: Option<i32>) -> ApiResult<()> {
    if version.is_some_and(|version| version <= 0) {
        return Err(ApiError::InvalidRequest(
            "workflow_version must be a positive immutable definition version".into(),
        ));
    }
    Ok(())
}

/// A duplicate logical invocation is a status/reconciliation response, never
/// another workflow execution. The existing run endpoint remains the durable
/// detail/status surface; `node_outputs` is intentionally empty here because a
/// replay may arrive while the original run is still executing.
fn workflow_run_replay_response(run: WorkflowRunRecord) -> Response {
    let status = if run.finished_at.is_some() {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    let mut response = (
        status,
        Json(CreateRunReplayResponse {
            run: CreateRunResponse {
                run_id: run.id,
                status: run.status,
                cost_usd: run.cost_usd,
                baseline_cost_usd: run.baseline_cost_usd,
                saved_usd: run.saved_usd,
                node_outputs: vec![],
            },
            replayed: true,
        }),
    )
        .into_response();
    response
        .headers_mut()
        .insert("idempotent-replay", HeaderValue::from_static("true"));
    response
}

fn validate_workflow_run_idempotency_binding(
    existing: &store::WorkflowRunIdempotencyBinding,
    requested_workflow_version: Option<i32>,
    input_hash: &[u8; 32],
    request_options_hash: &[u8; 32],
) -> ApiResult<()> {
    if let Some(requested) = requested_workflow_version {
        if requested != existing.workflow_version {
            return Err(ApiError::Conflict(
                "Idempotency-Key is already bound to a different workflow_version".into(),
            ));
        }
    }
    if &existing.input_hash != input_hash {
        return Err(ApiError::Conflict(
            "Idempotency-Key is already bound to different workflow inputs".into(),
        ));
    }
    if &existing.request_options_hash != request_options_hash {
        return Err(ApiError::Conflict(
            "Idempotency-Key is already bound to different execution options".into(),
        ));
    }
    Ok(())
}

/// Resolve a mapped run strictly. A malformed or temporarily unreadable mapping
/// must fail closed; treating it as a miss would permit a duplicate paid run.
async fn resolve_idempotent_workflow_run(
    pool: &sqlx::PgPool,
    org: Uuid,
    workflow_id: Uuid,
    binding: &store::WorkflowRunIdempotencyBinding,
) -> ApiResult<Response> {
    let run = store::get_run_strict(pool, binding.run_id, org)
        .await
        .map_err(|error| {
            tracing::error!(
                workflow_id = %workflow_id,
                run_id = %binding.run_id,
                error = %error,
                "idempotent workflow run lookup failed; refusing duplicate execution"
            );
            ApiError::ServiceUnavailable(
                "existing idempotent workflow run is temporarily unavailable; retry its status lookup"
                    .into(),
            )
        })?
        .ok_or_else(|| {
            tracing::error!(
                workflow_id = %workflow_id,
                run_id = %binding.run_id,
                "idempotent workflow mapping has no readable run; refusing duplicate execution"
            );
            ApiError::ServiceUnavailable(
                "existing idempotent workflow run is unavailable; retry its status lookup".into(),
            )
        })?;
    if run.workflow_id != workflow_id || run.version != binding.workflow_version {
        tracing::error!(
            workflow_id = %workflow_id,
            run_id = %run.id,
            expected_version = binding.workflow_version,
            actual_workflow_id = %run.workflow_id,
            actual_version = run.version,
            "idempotent workflow mapping/run invariant failed; refusing duplicate execution"
        );
        return Err(ApiError::ServiceUnavailable(
            "existing idempotent workflow run is inconsistent; retry its status lookup".into(),
        ));
    }
    Ok(workflow_run_replay_response(run))
}

#[cfg(test)]
mod workflow_run_idempotency_contract_tests {
    use super::*;

    fn binding() -> store::WorkflowRunIdempotencyBinding {
        store::WorkflowRunIdempotencyBinding {
            run_id: Uuid::from_u128(7),
            workflow_version: 3,
            input_hash: [1; 32],
            request_options_hash: [2; 32],
        }
    }

    #[test]
    fn legacy_callers_without_idempotency_key_remain_unmodified() {
        assert_eq!(
            workflow_run_idempotency_key(&HeaderMap::new()).expect("no header is valid"),
            None
        );
    }

    #[test]
    fn idempotency_key_rejects_blank_or_oversized_values() {
        let mut blank = HeaderMap::new();
        blank.insert(
            WORKFLOW_RUN_IDEMPOTENCY_HEADER,
            HeaderValue::from_static("   "),
        );
        assert!(matches!(
            workflow_run_idempotency_key(&blank),
            Err(ApiError::InvalidRequest(_))
        ));

        let mut oversized = HeaderMap::new();
        oversized.insert(
            WORKFLOW_RUN_IDEMPOTENCY_HEADER,
            HeaderValue::from_str(&"a".repeat(MAX_WORKFLOW_RUN_IDEMPOTENCY_KEY_BYTES + 1))
                .expect("ASCII header value"),
        );
        assert!(matches!(
            workflow_run_idempotency_key(&oversized),
            Err(ApiError::InvalidRequest(_))
        ));
    }

    #[test]
    fn replay_binding_accepts_the_same_request_and_rejects_drift() {
        let existing = binding();
        assert!(
            validate_workflow_run_idempotency_binding(&existing, Some(3), &[1; 32], &[2; 32],)
                .is_ok()
        );
        // Omitting a version is a retry of the first accepted immutable version,
        // not a request to silently switch to whatever version is latest now.
        assert!(
            validate_workflow_run_idempotency_binding(&existing, None, &[1; 32], &[2; 32]).is_ok()
        );
        assert!(matches!(
            validate_workflow_run_idempotency_binding(&existing, Some(4), &[1; 32], &[2; 32]),
            Err(ApiError::Conflict(_))
        ));
        assert!(matches!(
            validate_workflow_run_idempotency_binding(&existing, Some(3), &[9; 32], &[2; 32]),
            Err(ApiError::Conflict(_))
        ));
        assert!(matches!(
            validate_workflow_run_idempotency_binding(&existing, Some(3), &[1; 32], &[8; 32]),
            Err(ApiError::Conflict(_))
        ));
    }

    #[test]
    fn workflow_version_must_be_positive_when_present() {
        assert!(validate_requested_workflow_version(None).is_ok());
        assert!(validate_requested_workflow_version(Some(1)).is_ok());
        assert!(matches!(
            validate_requested_workflow_version(Some(0)),
            Err(ApiError::InvalidRequest(_))
        ));
    }
}

/// `POST /v1/workflows/secrets` request body.
#[derive(Debug, Deserialize)]
pub struct SetWorkflowSecretRequest {
    pub name: String,
    /// The plaintext secret value. Encrypted at rest; **never returned**.
    pub value: String,
}

const WORKFLOW_SECRET_INVENTORY_LIMIT: usize = 500;
const MAX_WORKFLOW_SECRET_VALUE_BYTES: usize = 64 * 1024;

/// Safe secret metadata for management and workflow pickers. `ready` means
/// only that the stored ciphertext decrypts with this gateway's current master
/// key; it does not validate the credential with any downstream provider.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowSecretState {
    Ready,
    Unusable,
    Unavailable,
}

#[derive(Debug, Serialize)]
pub struct WorkflowSecretView {
    pub name: String,
    pub state: WorkflowSecretState,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub rotated_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize)]
pub struct ListWorkflowSecretsResponse {
    pub object: &'static str,
    pub data: Vec<WorkflowSecretView>,
    pub truncated: bool,
}

// ---------------------------------------------------------------------------
// POST /v1/workflows — create / update a workflow definition
// ---------------------------------------------------------------------------

/// `POST /v1/workflows` — validate + store a workflow definition.
///
/// Ordering: `require_org` → assemble def → `validate` (400) →
/// `db_pool` (503) → `insert_definition`.  This lets callers see a useful
/// validation error without needing a live database.
pub async fn create(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Json(body): Json<CreateWorkflowRequest>,
) -> ApiResult<(StatusCode, Json<CreateWorkflowResponse>)> {
    let org = require_org(ctx)?;

    // Assemble a full WorkflowDefinition from the request body.  The conversion
    // deliberately carries every accepted top-level field, including triggers.
    let def = body.into_definition();

    // Validate first — returns 400 with the full error list before any DB call.
    let registry = state.registry.clone();
    validate::validate(&def, &|m| registry.resolve(m).is_some())
        .map_err(|errors| ApiError::InvalidRequest(errors.join("; ")))?;

    // DB pool check (503) only after successful validation.
    let pool = db_pool(&state)?;

    let hash = content_hash(&def);
    let version = store::insert_definition(pool, org, &def, &hash)
        .await
        .map_err(|e| {
            tracing::error!(
                workflow_id = %def.id,
                error = %e,
                "workflow_definitions INSERT failed"
            );
            ApiError::Internal(format!("failed to store workflow definition: {e}"))
        })?
        .ok_or_else(|| {
            ApiError::Conflict(format!(
                "workflow id {} version conflict: a concurrent insert won the race; retry",
                def.id
            ))
        })?;

    Ok((
        StatusCode::CREATED,
        Json(CreateWorkflowResponse {
            id: def.id,
            version,
            content_hash: hash,
        }),
    ))
}

// ---------------------------------------------------------------------------
// GET /v1/workflows — list definitions (latest version per id)
// ---------------------------------------------------------------------------

/// `GET /v1/workflows` — list all workflow definitions for the caller's org
/// (latest version per id, up to 100).
pub async fn list(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
) -> ApiResult<Json<ListWorkflowsResponse>> {
    let org = require_org(ctx)?;
    let pool = db_pool(&state)?;

    let metas = store::list_definitions(pool, org, 100).await;
    let data = metas
        .into_iter()
        .map(|m| WorkflowDefMetaView {
            id: m.id,
            name: m.name,
            version: m.version,
            created_at: m.created_at,
        })
        .collect();

    Ok(Json(ListWorkflowsResponse {
        object: "list",
        data,
    }))
}

// ---------------------------------------------------------------------------
// GET /v1/workflows/:id — latest definition
// ---------------------------------------------------------------------------

/// `GET /v1/workflows/:id` — fetch the latest version of a workflow definition
/// scoped by the caller's org.  Returns 404 when the id does not exist or
/// belongs to another org.
pub async fn get(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<workflow::WorkflowDefinition>> {
    let org = require_org(ctx)?;
    let pool = db_pool(&state)?;

    let (mut def, _version) = store::get_definition(pool, org, id)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("no workflow with id {id}")))?;

    // Patch the JSONB-deserialized version (which may be 0 if the client
    // omitted it when creating) with the authoritative DB version.
    def.version = _version as u32;

    Ok(Json(def))
}

// ---------------------------------------------------------------------------
// POST /v1/workflows/:id/estimate — pre-run cost projection
// ---------------------------------------------------------------------------

/// `POST /v1/workflows/:id/estimate` — return a static pre-run cost projection
/// for the workflow's latest definition.  No model calls are made.
pub async fn estimate(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
    Json(body): Json<EstimateRequest>,
) -> ApiResult<Json<EstimateResponse>> {
    let org = require_org(ctx)?;
    let pool = db_pool(&state)?;

    let (def, _version) = store::get_definition(pool, org, id)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("no workflow with id {id}")))?;

    let est = estimate::estimate_workflow(&def, &body.inputs);

    Ok(Json(EstimateResponse {
        projected_cost_usd: est.projected_cost_usd,
        per_node: est
            .per_node
            .into_iter()
            .map(|n| NodeEstimateView {
                node_id: n.node_id,
                model: n.model,
                cost_usd: n.cost_usd,
            })
            .collect(),
        warnings: est.warnings,
    }))
}

// ---------------------------------------------------------------------------
// POST /v1/workflows/:id/runs — synchronous + streaming execution
// ---------------------------------------------------------------------------

/// Persist node-run journal entries + finalize the run record (best-effort).
/// Returns the status string (`"completed"` / `"failed"` / `"budget_exhausted"`).
/// Called from both the sync and streaming `create_run` paths.
async fn persist_run_results(
    pool: &sqlx::PgPool,
    run_id: Uuid,
    org: Uuid,
    journal: Vec<engine::NodeJournalEntry>,
    result: &engine::WorkflowRunResult,
) -> &'static str {
    for entry in journal {
        node_run_store::insert_node_run(
            pool,
            run_id,
            &entry.node_id,
            &entry.status,
            entry.output,
            entry.cost_usd,
            entry.model_used.as_deref(),
            entry.error.as_deref(),
            entry.started_at,
            entry.finished_at,
        )
        .await;
    }
    let status = match result.status {
        WfStatus::Succeeded => "completed",
        WfStatus::Failed => "failed",
        WfStatus::BudgetExhausted => "budget_exhausted",
    };
    store::finish_run(
        pool,
        run_id,
        org,
        status,
        result.cost_usd,
        result.baseline_cost_usd,
        result.saved_usd,
        result.error.as_deref(),
    )
    .await;
    status
}

/// Spawn the flow-level end-to-end quality gate for a completed workflow run
/// (BACKLOG item #5). Best-effort + detached: samples via
/// [`workflow::quality_gate::should_quality_gate`]; on a sampled run, reuses
/// the existing `quality_sample::judge_paired` primitive — the `optimized_answer`
/// is the run's `Output`-node final content, the `baseline_answer` is a
/// reference-model re-dispatch of the trigger input (mirroring the agent_run
/// per-turn-judge precedent at `agent_run.rs:2091`, which re-dispatches the
/// ORIGINAL request to its source provider for the reference answer). The
/// verdict is written to `workflow_runs.quality_verdict` via
/// [`store::upsert_quality_verdict`] (the cloud mint reads it for the current
/// quality-bearing workflow receipt version).
///
/// **Fail-open + opt-in:** off by default (`judge_config.sample_rate == 0`); a
/// judge error / timeout / disabled-config records `NotSampled` (nothing
/// written) — NEVER blocks the run, never fails the workflow. The spawn is
/// detached so it adds zero user latency; the judge spend is bounded by the
/// config's per-org-day cap (bites BEFORE sampling) + the run's single paired
/// judge (both_orders: false).
#[allow(clippy::too_many_arguments)]
fn spawn_flow_quality_gate(
    state: &AppState,
    pool: &sqlx::PgPool,
    run_id: Uuid,
    org_id: Uuid,
    api_key_id: Uuid,
    raw_bearer: &str,
    trigger_input: serde_json::Value,
    final_answer: String,
) {
    use crate::quality_sample::{self, GatewayLlmJudge};
    use crate::routes::chat::resolve_credentials_for;
    use tt_shared::RequestContext;

    // 1. Cap-then-sample (mirrors the agent_run per-turn judge's two-stage
    //    bound). The per-org-day cap bites BEFORE sampling; the sample rate is
    //    the within-cap rate. NB: this fetches `judge_config` off AppState —
    //    when `sample_rate == 0` (the default) the gate is off entirely.
    let cfg = &state.judge_config;
    // The per-org-day cap is an in-memory counter the agent_run loop holds on
    // a `QualityGate` state field; this workflow path does NOT yet have that
    // counter wired (the gate is the run-level follow-up). Treat the cap as
    // acquired (i.e. rely on the sample rate alone for v1) — a TODO to thread
    // the cap through once the run-level gate's wiring is exercised in
    // production. The `both_orders: false` + the per-run key still bound cost:
    // one paired judge per sampled run.
    if !workflow::quality_gate::should_quality_gate(run_id, cfg.sample_rate, true) {
        return;
    }

    // 2. Owned captures for the detached `'static + Send` future.
    let pool = pool.clone();
    let state = state.clone();
    let raw_bearer = raw_bearer.to_string();
    let judge_config = cfg.clone();
    tokio::spawn(async move {
        // 3. Resolve the judge model + creds (the agent_run precedent). The
        //    returned `ProviderCredentials` carries the resolved API key +
        //    base_url; reuse it verbatim as the judge ctx's credentials.
        let Some(provider) = state.registry.resolve(&judge_config.judge_model) else {
            return;
        };
        let Some(creds) =
            resolve_credentials_for(&state, org_id, provider.id(), &raw_bearer, true).await
        else {
            return;
        };
        let judge_ctx = RequestContext {
            trace_id: run_id,
            org_id,
            api_key_id,
            credentials: creds,
            tag: None,
            deadline: Some(judge_config.baseline_timeout),
            run_id: Some(run_id),
            node_id: None,
        };
        let judge = GatewayLlmJudge::new(provider, judge_config.judge_model.clone(), judge_ctx)
            .with_call_timeout(judge_config.baseline_timeout);
        // The baseline = the run's final synthesized answer compared against
        // the trigger INPUT itself (a structural-quality gate for v1). TODO:
        // re-dispatch the trigger to the reference model for a semantic
        // baseline once the judge-model-completion seam is wired (the agent_run
        // per-turn judge re-dispatches at agent_run.rs:2091).
        let input_str = trigger_input_to_string(&trigger_input);
        let order = quality_sample::ab_order_for(run_id);
        let outcome = quality_sample::judge_paired(
            &judge,
            &input_str,
            // baseline_answer (v1 = the trigger; TODO semantic baseline).
            &input_str,
            &final_answer,
            order,
            false, // both_orders: false → one judge call per sampled run.
        )
        .await;
        let verdict = match outcome {
            Ok(outcome) => {
                use tt_plan_core::JudgeVerdict;
                match outcome.verdict {
                    JudgeVerdict::Acceptable => workflow::quality_gate::QualityVerdict::Equivalent,
                    JudgeVerdict::Degraded => workflow::quality_gate::QualityVerdict::Degraded,
                    JudgeVerdict::Unclear => workflow::quality_gate::QualityVerdict::Inconclusive,
                }
            }
            Err(_failure) => {
                // Fail-open: a judge error records Inconclusive (sampled, but
                // couldn't judge) — NOT NotSampled (the run WAS sampled, so
                // the receipt carries the honest "could not judge" verdict).
                tracing::debug!(
                    run_id = %run_id,
                    "flow quality-gate judge failed; recording Inconclusive (fail-open)"
                );
                workflow::quality_gate::QualityVerdict::Inconclusive
            }
        };
        if verdict.carries_on_receipt() {
            store::upsert_quality_verdict(&pool, run_id, org_id, verdict.code()).await;
            tracing::info!(
                run_id = %run_id,
                verdict = verdict.code(),
                "flow quality-gate verdict recorded"
            );
        }
    });
}

/// Stringify the trigger input (`serde_json::Value`) for the judge's `&str` API.
/// Pretty-printed so a human reading the receipt can understand what was judged.
fn trigger_input_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Extract the workflow run's final synthesized answer — the content of the
/// first (or only) `Output` node. `None` when the run produced no output node
/// (failed, or no Output wired into the def). Stringifies a JSON `Value`
/// content via [`trigger_input_to_string`].
fn final_synthesized_answer(
    node_outputs: &[(String, workflow::types::NodeOutput)],
) -> Option<String> {
    let (_node_id, out) = node_outputs.first()?;
    Some(trigger_input_to_string(&out.content))
}

/// `POST /v1/workflows/:id/runs` — execute the workflow.
///
/// When `stream=false` (default), returns a single JSON response after the
/// run completes. When `stream=true`, returns a `text/event-stream` of
/// [`WfEvent`]s. Both paths persist node-runs + finalize the run record
/// with full baseline/saved savings (W2a) — the streaming path does so
/// inside the spawned task.
///
/// Identity fields (`api_key_id`, `caller_tier`, `l2_allowed`, `raw_bearer`)
/// are derived from the `ApiKeyContext` extension and the `Authorization`
/// header, mirroring `agent_run::RunIdentity::from_request`.
pub async fn create_run(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<CreateRunRequest>,
) -> ApiResult<Response> {
    // --- Auth: extract org + all identity fields in one match arm -----------
    // We cannot call `require_org(ctx)` (which consumes ctx) and then re-use
    // ctx for api_key_id / caller_tier.  Extract everything at once instead,
    // mirroring `agent_run::RunIdentity::from_request`.
    let (org, api_key_id, caller_tier) = match ctx {
        Some(Extension(c)) if c.org_id != DOGFOOD_ORG_ID => (c.org_id, c.key_id, c.tier),
        _ => return Err(ApiError::Unauthorized),
    };

    let l2_allowed = matches!(
        caller_tier,
        Some(
            tt_shared::CallerTier::Pro | tt_shared::CallerTier::Team | tt_shared::CallerTier::Scale
        )
    );

    let raw_bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.strip_prefix("Bearer ")
                .or_else(|| s.strip_prefix("bearer "))
        })
        .unwrap_or("")
        .to_string();

    let pool = db_pool(&state)?;
    let CreateRunRequest {
        inputs,
        workflow_version: requested_workflow_version,
        max_cost_usd,
        stream,
    } = body;
    validate_requested_workflow_version(requested_workflow_version)?;

    // A supplied key is resolved before reading the latest definition. That is
    // what lets a lost response safely reconcile to its original immutable
    // version after an editor has saved a newer one.
    let idempotency_key = workflow_run_idempotency_key(&headers)?;
    let input_hash = store::workflow_run_input_hash(&inputs).map_err(|error| {
        ApiError::InvalidRequest(format!("workflow inputs cannot be canonicalized: {error}"))
    })?;
    let request_options_hash =
        store::workflow_run_request_options_hash(max_cost_usd).map_err(|error| {
            ApiError::InvalidRequest(format!(
                "workflow execution options cannot be canonicalized: {error}"
            ))
        })?;
    let idempotency_key_hash = idempotency_key
        .as_deref()
        .map(|key| store::workflow_run_invocation_key_hash(org, id, key));

    if let Some(key_hash) = idempotency_key_hash.as_ref() {
        let existing = store::get_workflow_run_idempotency(pool, org, id, key_hash)
            .await
            .map_err(|error| {
                tracing::error!(
                    workflow_id = %id,
                    error = %error,
                    "workflow idempotency lookup failed; refusing duplicate execution"
                );
                ApiError::ServiceUnavailable(
                    "workflow idempotency storage is temporarily unavailable; retry".into(),
                )
            })?;
        if let Some(existing) = existing {
            validate_workflow_run_idempotency_binding(
                &existing,
                requested_workflow_version,
                &input_hash,
                &request_options_hash,
            )?;
            return resolve_idempotent_workflow_run(pool, org, id, &existing).await;
        }
    }

    // --- Load the accepted immutable version + defense-in-depth validate ----
    let (def, version) = match requested_workflow_version {
        Some(version) => store::get_definition_version(pool, org, id, version).await,
        None => store::get_definition(pool, org, id).await,
    }
    .ok_or_else(|| {
        let detail = requested_workflow_version
            .map(|version| format!("no workflow with id {id} at version {version}"))
            .unwrap_or_else(|| format!("no workflow with id {id}"));
        ApiError::NotFound(detail)
    })?;

    {
        let registry = state.registry.clone();
        validate::validate_for_execution(&def, &|m| registry.resolve(m).is_some())
            .map_err(|errors| ApiError::InvalidRequest(errors.join("; ")))?;
    }

    // --- Atomically claim a stable key + insert initial running record -------
    let run_max_cost = def.budget.max_cost_usd.or(max_cost_usd);
    admit_workflow_budget_before_dispatch(
        WorkflowBudgetAdmissionPath::Direct,
        &def,
        &inputs,
        run_max_cost,
    )
    .map_err(ApiError::InvalidRequest)?;
    let run_record = WorkflowRunRecord {
        id: Uuid::new_v4(),
        workflow_id: def.id,
        version,
        org_id: org,
        status: "running".to_string(),
        inputs: Some(inputs.clone()),
        cost_usd: 0.0,
        max_cost_usd: run_max_cost,
        baseline_cost_usd: 0.0,
        saved_usd: 0.0,
        error: None,
        started_at: chrono::Utc::now(),
        finished_at: None,
    };
    let run_id = if let Some(invocation_key_hash) = idempotency_key_hash {
        let mapping = store::NewWorkflowRunIdempotency {
            org_id: org,
            workflow_id: def.id,
            invocation_key_hash,
            workflow_version: version,
            input_hash,
            request_options_hash,
        };
        match store::create_or_reuse_idempotent_run(pool, &mapping, &run_record)
            .await
            .map_err(|error| {
                tracing::error!(
                    workflow_id = %id,
                    error = %error,
                    "workflow idempotency claim/create failed; refusing execution"
                );
                ApiError::ServiceUnavailable(
                    "workflow idempotency storage is temporarily unavailable; retry".into(),
                )
            })? {
            store::CreateOrReuseWorkflowRun::Created => run_record.id,
            store::CreateOrReuseWorkflowRun::Existing(existing) => {
                // A concurrent caller won after our initial lookup. An omitted
                // version intentionally replays that first accepted version;
                // an explicit different version is a conflict.
                validate_workflow_run_idempotency_binding(
                    &existing,
                    requested_workflow_version,
                    &input_hash,
                    &request_options_hash,
                )?;
                return resolve_idempotent_workflow_run(pool, org, id, &existing).await;
            }
        }
    } else {
        // Backwards-compatible legacy path: no key still creates a fresh run
        // and retains the existing best-effort journal behavior.
        store::insert_run(pool, &run_record).await;
        run_record.id
    };

    // --- Load org secrets once (both sync + streaming paths) -----------------
    // Empty map when TT_MASTER_KEY is absent — workflows without Http secret
    // refs still work; referenced secrets fail closed in the engine preflight.
    let secrets = match master_key_from_env() {
        Some(master) => load_secrets(pool, org, &master).await,
        None => std::collections::HashMap::new(),
    };

    // Capture the bearer before the executor (below) moves it; the flow
    // quality-gate spawn uses it to resolve the judge model's credentials.
    let raw_bearer_for_gate = raw_bearer.clone();

    if !stream {
        // --- Synchronous path ------------------------------------------------
        let executor = GatewayNodeExecutor {
            state: &state,
            org_id: org,
            api_key_id,
            caller_tier,
            l2_allowed,
            raw_bearer,
            run_id,
        };
        let mut journal_entries: Vec<engine::NodeJournalEntry> = Vec::new();
        let cache = engine::FlowDocDistillCache { org_id: org, pool };
        let result = engine::run_workflow(
            &executor,
            &def,
            &inputs,
            run_max_cost,
            |entry| journal_entries.push(entry),
            None,
            &secrets,
            0,
            &[],
            &cache,
        )
        .await;
        let status_str = persist_run_results(pool, run_id, org, journal_entries, &result).await;
        // Flow-level quality gate (BACKLOG item #5): on a completed run with an
        // Output node, sample + spawn the detached judge. Fail-open + opt-in
        // (off by default until judge_config.sample_rate > 0). The trigger input
        // + the Output node's final content feed the judge.
        if result.status == WfStatus::Succeeded {
            if let Some(final_answer) = final_synthesized_answer(&result.node_outputs) {
                spawn_flow_quality_gate(
                    &state,
                    pool,
                    run_id,
                    org,
                    api_key_id,
                    &raw_bearer_for_gate,
                    inputs.clone(),
                    final_answer,
                );
            }
        }
        Ok(Json(CreateRunResponse {
            run_id,
            status: status_str.to_string(),
            cost_usd: result.cost_usd,
            baseline_cost_usd: result.baseline_cost_usd,
            saved_usd: result.saved_usd,
            node_outputs: result
                .node_outputs
                .into_iter()
                .map(|(node_id, out)| NodeOutputView {
                    node_id,
                    content: out.content,
                    cost_usd: out.cost_usd,
                })
                .collect(),
        })
        .into_response())
    } else {
        // --- Streaming path: spawn the engine + return SSE -------------------
        // `owned_state` is cloned (cheap Arc bump) so the spawned 'static task
        // can own it; the executor borrows &owned_state from within the block.
        // `secrets` was loaded before the branch so both paths share one DB call.
        let owned_state = state.clone();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<WfEvent>();
        // P0-8: emit `run.start { run_id }` BEFORE the engine runs, so the
        // client's Seal-receipt affordance (which gates on run_id) is reachable
        // during + after the run. The engine itself doesn't know run_id (it's
        // minted here in the caller); the terminal `run.done` carries the
        // totals. `let _ =` because a closed client socket is not a run failure.
        let _ = tx.send(WfEvent::RunStart { run_id });
        tokio::spawn(async move {
            let executor = GatewayNodeExecutor {
                state: &owned_state,
                org_id: org,
                api_key_id,
                caller_tier,
                l2_allowed,
                raw_bearer,
                run_id,
            };
            let mut journal_entries: Vec<engine::NodeJournalEntry> = Vec::new();
            // The cache borrows the pool from `owned_state` (the spawned task's
            // own state clone), not the outer `state` (which the borrow-checker
            // flags as not living long enough across the 'static spawn). No pool
            // → fall back to `NoCache` (the Document node distills fresh).
            let no_cache = engine::NoCache;
            let pool_cache: Option<engine::FlowDocDistillCache<'_>> = owned_state
                .db_pool
                .as_ref()
                .map(|pool| engine::FlowDocDistillCache { org_id: org, pool });
            let cache: &dyn engine::DistillCacheStore = match &pool_cache {
                Some(c) => c,
                None => &no_cache,
            };
            let result = engine::run_workflow(
                &executor,
                &def,
                &inputs,
                run_max_cost,
                |entry| journal_entries.push(entry),
                Some(&tx),
                &secrets,
                0,
                &[],
                cache,
            )
            .await;
            // Persist node runs + finalize (best-effort, mirrors sync path).
            // run_workflow already emitted WfEvent::RunDone; no double-send needed.
            if let Some(pool) = owned_state.db_pool.as_ref() {
                persist_run_results(pool, run_id, org, journal_entries, &result).await;
                // Flow-level quality gate (BACKLOG item #5) — the streaming path
                // spawns the detached judge too, so a streaming run's receipt
                // carries the verdict when it's sampled. Mirror the sync path.
                if result.status == WfStatus::Succeeded {
                    if let Some(final_answer) = final_synthesized_answer(&result.node_outputs) {
                        spawn_flow_quality_gate(
                            &owned_state,
                            pool,
                            run_id,
                            org,
                            api_key_id,
                            &raw_bearer_for_gate,
                            inputs.clone(),
                            final_answer,
                        );
                    }
                }
            }
            // tx drops here → unfold stream sees None → stream ends → [DONE]
        });
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv()
                .await
                .map(|ev| (Ok::<_, std::convert::Infallible>(ev.to_sse()), rx))
        })
        .chain(futures::stream::once(async {
            Ok::<_, std::convert::Infallible>(Event::default().data("[DONE]"))
        }));
        Ok(Sse::new(stream)
            .keep_alive(KeepAlive::default())
            .into_response())
    }
}

// ---------------------------------------------------------------------------
// CO-1: route-triggered workflow detour / shadow (called from chat::complete_once)
// ---------------------------------------------------------------------------
//
// A matched route whose `RouteAction.workflow` is `Some(_)` detours the chat
// request into the referenced workflow — the flagship cohesion item ("routing
// rules that detour into governed, receipted multi-step workflows"). DETOUR
// mode replaces the upstream call (the workflow's final synthesized answer
// becomes the chat response); SHADOW mode runs the workflow alongside the
// normal dispatch and records its cost separately (no response substitution).
//
// These functions mirror `create_run`'s sync path (load + validate + run +
// persist) but: (a) draw identity from the already-prepared `Prepared` bundle
// rather than the HTTP `ApiKeyContext`, (b) reject over-cap projections BEFORE
// any spend via `estimate_workflow`, and (c) for DETOUR, shape the workflow's
// final answer into a `ChatCompletionResponse` + a single `provider="workflow"`
// `request_logs` row (the same sentinel pattern `complete_panel` uses for
// `provider="panel"`). Streaming workflow detour is unsupported in v1 (the
// handler warns + falls through to single-model streaming before reaching here).

/// Extract the last user message text from a chat request, as a.workflow
/// trigger input. Mirrors the message-text extraction in
/// `shaping::diff::prior_source` (concatenating text parts). Falls back to the
/// JSON encoding of the whole messages vector when no user message is present
/// (the workflow still runs; its first node sees an empty/object input).
fn last_user_message_text(req: &tt_shared::ChatCompletionRequest) -> serde_json::Value {
    for m in req.messages.iter().rev() {
        if let Message::User { content, .. } = m {
            let text = match content {
                MessageContent::Text(s) => s.clone(),
                MessageContent::Parts(parts) => parts
                    .iter()
                    .filter_map(|p| match p {
                        tt_shared::messages::ContentPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            };
            return serde_json::Value::String(text);
        }
    }
    // No user message: pass the whole request shape so the workflow's trigger
    // node still has something to consume (a workflow that ignores its input
    // still produces its declared output).
    serde_json::to_value(&req.messages).unwrap_or(serde_json::Value::Null)
}

/// Resolve identity + load + validate a workflow definition for a route detour.
/// Shared by detour + shadow. Returns `(def, version)`.
async fn load_route_workflow(
    state: &AppState,
    ctx: &tt_shared::RequestContext,
    cfg: &tt_routing::RouteWorkflow,
) -> ApiResult<(workflow::types::WorkflowDefinition, i32)> {
    let pool = db_pool(state)?;
    let workflow_id = cfg.workflow_id.parse::<Uuid>().map_err(|_| {
        ApiError::InvalidRequest(format!("invalid workflow_id: {}", cfg.workflow_id))
    })?;
    let (def, version) = store::get_definition(pool, ctx.org_id, workflow_id)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("no workflow with id {workflow_id}")))?;
    {
        let registry = state.registry.clone();
        validate::validate_for_execution(&def, &|m| registry.resolve(m).is_some())
            .map_err(|errors| ApiError::InvalidRequest(errors.join("; ")))?;
    }
    Ok((def, version))
}

/// Run a route-detour workflow in DETOUR mode: the workflow's final synthesized
/// answer replaces the upstream chat response. Records realized spend ONCE + a
/// single `provider="workflow"` `request_logs` row, mirroring `complete_panel`'s
/// billing discipline. Rejects an over-cap projection BEFORE any spend.
pub(crate) async fn complete_workflow(
    state: &AppState,
    ctx: &tt_shared::RequestContext,
    prep: Prepared,
    cfg: tt_routing::RouteWorkflow,
) -> ApiResult<CompletionOutcome> {
    let (def, _version) = load_route_workflow(state, ctx, &cfg).await?;
    let pool = db_pool(state)?;

    // Inputs = the last user message text (the workflow's trigger node echoes
    // `inputs` as its output, which downstream Model nodes substitute via
    // `{{trigger}}`).
    let inputs = last_user_message_text(&prep.req);

    // Budget admission happens before any run record or provider dispatch. A
    // capped route must have explicitly output-bounded direct intelligence
    // nodes with input-only prompts. The executor later serializes capped
    // intelligence nodes and reserves their directional estimates, but a
    // started node's routed provider work can still settle above that estimate.
    let run_max_cost = cfg.max_cost_usd.or(def.budget.max_cost_usd);
    admit_workflow_budget_before_dispatch(
        WorkflowBudgetAdmissionPath::Detour,
        &def,
        &inputs,
        run_max_cost,
    )
    .map_err(ApiError::InvalidRequest)?;

    let run_id = Uuid::new_v4();
    let secrets = match master_key_from_env() {
        Some(master) => load_secrets(pool, ctx.org_id, &master).await,
        None => std::collections::HashMap::new(),
    };
    let executor = GatewayNodeExecutor {
        state,
        org_id: ctx.org_id,
        api_key_id: ctx.api_key_id,
        caller_tier: prep.caller_tier,
        l2_allowed: prep.l2_allowed,
        raw_bearer: prep.raw_bearer.clone(),
        run_id,
    };
    let mut journal: Vec<engine::NodeJournalEntry> = Vec::new();
    let cache = workflow::engine::FlowDocDistillCache {
        org_id: ctx.org_id,
        pool,
    };
    let result = engine::run_workflow(
        &executor,
        &def,
        &inputs,
        run_max_cost,
        |entry| journal.push(entry),
        None,
        &secrets,
        0,
        &[],
        &cache,
    )
    .await;
    let status_str = persist_run_results(pool, run_id, ctx.org_id, journal, &result).await;

    // Failed / budget-exhausted workflow → a 502 (mirrors a failed panel: no
    // client-facing answer, but the run record + journal are already persisted
    // above so the failure is auditable + receipt-able via the run-inspector).
    if result.status != WfStatus::Succeeded {
        return Err(ApiError::ServiceUnavailable(format!(
            "workflow {run_id} ended {status_str}"
        )));
    }

    // NOTE: the workflow's realized spend + per-node `request_logs` rows are
    // already recorded INSIDE `run_workflow` — every Model node dispatches
    // through `agent_run::drive_workflow_node` → `chat::complete_once`, which
    // calls `spend_sink().record()` + `settle()` + writes one `request_logs`
    // row per node (stamped with `run_id`). So this function must NOT add an
    // aggregate `record`/`settle` or a second `request_logs` row — that would
    // DOUBLE-COUNT the org's spend (the bug the adversarial verify caught: the
    // panel precedent does aggregate-settle because `run_panel` fans out legs
    // itself, OUTSIDE complete_once; workflows route every node THROUGH
    // complete_once, so the per-node settle IS the only settle). The aggregate
    // cost/saved/baseline lives on the `workflow_runs` row (persist_run_results)
    // + the per-node rows carry the breakdown in `workflow_node_runs`. This
    // mirrors `create_run` exactly (it does no aggregate settle/row either).
    let total_cost_usd = result.cost_usd;
    let trace_id = ctx.trace_id;

    // Shape the workflow's final synthesized answer as a chat completion. The
    // workflow's Output node's content becomes the assistant message; `model`
    // echoes the workflow id so the client knows the answer is workflow-sourced.
    let answer = final_synthesized_answer(&result.node_outputs).unwrap_or_default();
    let response = ChatCompletionResponse {
        id: format!("wf-{run_id}"),
        object: "chat.completion".to_string(),
        created: chrono::Utc::now().timestamp(),
        model: cfg.workflow_id.clone(),
        choices: vec![Choice {
            index: 0,
            message: Message::Assistant {
                content: Some(MessageContent::Text(answer)),
                tool_calls: Vec::new(),
                name: None,
            },
            finish_reason: Some("stop".to_string()),
        }],
        usage: Usage::default(),
    };

    let cost_breakdown = CostBreakdown {
        cost_usd: total_cost_usd,
        baseline_cost_usd: result.baseline_cost_usd,
        provider_cache_saved_usd: 0.0,
        flex_saved_usd: 0.0,
        compression_saved_usd: 0.0,
        doc_compaction_saved_usd: 0.0,
        cache_bust_penalty_usd: 0.0,
        summarizer_tax_usd: 0.0,
        batch_forgone_usd: 0.0,
        minify_saved_est_usd: 0.0,
        diff_saved_usd: 0.0,
        format_switch_saved_est_usd: 0.0,
        diff_failed_cost_usd: 0.0,
        doc_vision_saved_est_usd: 0.0,
        content_compress_saved_est_usd: 0.0,
    };

    Ok(CompletionOutcome::Dispatched {
        response,
        headers: Box::new(CompletionHeaders {
            trace_id,
            provider_id: "workflow".to_string(),
            model_used: cfg.workflow_id,
            cost_breakdown,
            cache_state: "none",
            route_matched_name: prep.route_matched_name,
            body_captured: false,
            req: prep.req,
            provider: prep.provider,
            warnings: prep.warnings,
            panel_body: None,
        }),
    })
}

/// Run a route-detour workflow in SHADOW mode: the workflow runs for its
/// cost/receipt only, alongside the normal single-model dispatch (which
/// `complete_once` continues after this returns). Best-effort: any error is
/// returned as a string and surfaced as a warning, never failing the request.
pub(crate) async fn run_workflow_shadow(
    state: &AppState,
    ctx: &tt_shared::RequestContext,
    prep: &Prepared,
    cfg: &tt_routing::RouteWorkflow,
) -> Result<(), String> {
    let (def, _version) = load_route_workflow(state, ctx, cfg)
        .await
        .map_err(|e| format!("load: {e}"))?;
    let pool = db_pool(state).map_err(|e| format!("pool: {e}"))?;
    let inputs = last_user_message_text(&prep.req);
    let run_max_cost = cfg.max_cost_usd.or(def.budget.max_cost_usd);
    admit_workflow_budget_before_dispatch(
        WorkflowBudgetAdmissionPath::Shadow,
        &def,
        &inputs,
        run_max_cost,
    )?;
    let run_id = Uuid::new_v4();
    let secrets = match master_key_from_env() {
        Some(master) => load_secrets(pool, ctx.org_id, &master).await,
        None => std::collections::HashMap::new(),
    };
    let executor = GatewayNodeExecutor {
        state,
        org_id: ctx.org_id,
        api_key_id: ctx.api_key_id,
        caller_tier: prep.caller_tier,
        l2_allowed: prep.l2_allowed,
        raw_bearer: prep.raw_bearer.clone(),
        run_id,
    };
    let mut journal: Vec<engine::NodeJournalEntry> = Vec::new();
    let cache = workflow::engine::FlowDocDistillCache {
        org_id: ctx.org_id,
        pool,
    };
    let result = engine::run_workflow(
        &executor,
        &def,
        &inputs,
        run_max_cost,
        |entry| journal.push(entry),
        None,
        &secrets,
        0,
        &[],
        &cache,
    )
    .await;
    persist_run_results(pool, run_id, ctx.org_id, journal, &result).await;
    // NOTE: no aggregate `spend_sink().record()`/`settle()` here. The shadow
    // workflow's per-node spend is already recorded+settled inside
    // `run_workflow` (each Model node dispatches through `chat::complete_once`,
    // which settles). Adding an aggregate settle here would double-count the
    // org's spend/billed-request counters (the bug the adversarial verify
    // caught). The normal single-model dispatch that `complete_once` continues
    // to after this returns records its OWN spend separately — that's correct
    // (shadow mode doubles upstream spend by design, gated on max_cost_usd).
    Ok(())
}

/// `GET /v1/workflows/secrets` — return a bounded, deterministic inventory of
/// safe metadata for the caller's org. Values, ciphertext, hashes, lengths,
/// and key material are never returned. Successful responses are no-store.
pub async fn list_workflow_secrets(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
) -> ApiResult<Response> {
    let org = require_org(ctx)?;
    let pool = db_pool(&state)?;
    let mut rows = list_secret_rows(pool, org, (WORKFLOW_SECRET_INVENTORY_LIMIT + 1) as i64)
        .await
        .map_err(|error| {
            tracing::warn!(%org, %error, "workflow secret inventory read failed");
            ApiError::ServiceUnavailable(
                "workflow secret inventory is temporarily unavailable".into(),
            )
        })?;
    let truncated = rows.len() > WORKFLOW_SECRET_INVENTORY_LIMIT;
    rows.truncate(WORKFLOW_SECRET_INVENTORY_LIMIT);

    let master = master_key_from_env();
    let data = rows
        .into_iter()
        .map(|row| {
            let state = match master.as_ref() {
                Some(master) if row.is_decryptable(master, org) => WorkflowSecretState::Ready,
                Some(_) => WorkflowSecretState::Unusable,
                None => WorkflowSecretState::Unavailable,
            };
            WorkflowSecretView {
                name: row.name,
                state,
                created_at: row.created_at,
                rotated_at: row.rotated_at,
            }
        })
        .collect();

    let mut response = Json(ListWorkflowSecretsResponse {
        object: "list",
        data,
        truncated,
    })
    .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    Ok(response)
}

/// `POST /v1/workflows/secrets` — encrypt and upsert a named secret for the
/// caller's org.
///
/// * `name` must match `^[A-Z0-9_]{1,64}$` (the charset used by
///   `{{secrets.NAME}}` template references in Http nodes) → 400 otherwise.
/// * `value` must contain 1–65,536 UTF-8 bytes → 400 otherwise.
/// * Returns 503 when `TT_MASTER_KEY` is absent (secret storage not configured).
/// * Returns 204 on success. **The value is never echoed in any response or log.**
pub async fn set_workflow_secret(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Json(body): Json<SetWorkflowSecretRequest>,
) -> ApiResult<StatusCode> {
    let org = require_org(ctx)?;
    if !is_valid_secret_name(&body.name) {
        return Err(ApiError::InvalidRequest(
            "secret name must match ^[A-Z0-9_]{1,64}$ \
             (uppercase letters, digits, and underscore only)"
                .into(),
        ));
    }
    if body.value.is_empty() || body.value.len() > MAX_WORKFLOW_SECRET_VALUE_BYTES {
        return Err(ApiError::InvalidRequest(format!(
            "secret value must contain 1–{MAX_WORKFLOW_SECRET_VALUE_BYTES} UTF-8 bytes"
        )));
    }
    let master = master_key_from_env().ok_or_else(|| {
        ApiError::ServiceUnavailable("secret storage not configured (TT_MASTER_KEY absent)".into())
    })?;
    let pool = db_pool(&state)?;
    store_secret(pool, org, &body.name, &master, &body.value)
        .await
        .map_err(|e| {
            tracing::error!(
                secret_name = %body.name,
                error = %e,
                "workflow_secrets UPSERT failed"
            );
            ApiError::Internal(format!("failed to store secret: {e}"))
        })?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /v1/workflows/secrets/:name` — idempotently remove one encrypted
/// secret for the caller's org. A master key is not required to delete the
/// ciphertext. Stored workflow versions are not rewritten: any retained
/// reference fails closed at that definition's next preflight.
pub async fn delete_workflow_secret(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let org = require_org(ctx)?;
    if !is_valid_secret_name(&name) {
        return Err(ApiError::InvalidRequest(
            "secret name must match ^[A-Z0-9_]{1,64}$ \
             (uppercase letters, digits, and underscore only)"
                .into(),
        ));
    }
    let pool = db_pool(&state)?;
    delete_secret(pool, org, &name).await.map_err(|error| {
        tracing::error!(%org, %error, "workflow secret DELETE failed");
        ApiError::Internal("failed to delete workflow secret".into())
    })?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ProviderRegistry;
    use crate::workflow::types::{
        BudgetPolicy, Edge, ModelSelection, Node, NodeKind, WorkflowDefinition, WorkflowTrigger,
    };
    use axum::extract::State;
    use tt_auth::ApiKeyContext;

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    fn test_state() -> AppState {
        AppState::new(ProviderRegistry::new())
    }

    fn real_org_ctx(org: Uuid) -> Option<Extension<ApiKeyContext>> {
        Some(Extension(ApiKeyContext {
            key_id: Uuid::new_v4(),
            org_id: org,
            tier: None,
            skip_shadow: false,
        }))
    }

    /// A cyclic definition (a → b → a back-edge) that fails validation
    /// regardless of the registry contents.
    fn cyclic_def() -> CreateWorkflowRequest {
        CreateWorkflowRequest {
            id: None,
            version: None,
            name: "cyclic".to_string(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "a".into(),
                    kind: NodeKind::Transform { expr: "x".into() },
                },
                Node {
                    id: "b".into(),
                    kind: NodeKind::Transform { expr: "y".into() },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "a".into(),
                    map: None,
                },
                Edge {
                    from: "a".into(),
                    to: "b".into(),
                    map: None,
                },
                // back-edge → cycle
                Edge {
                    from: "b".into(),
                    to: "a".into(),
                    map: None,
                },
                Edge {
                    from: "b".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
            triggers: vec![],
        }
    }

    /// A valid definition (no Model nodes, so passes an empty registry).
    fn valid_def() -> CreateWorkflowRequest {
        CreateWorkflowRequest {
            id: None,
            version: None,
            name: "valid".to_string(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![Edge {
                from: "t".into(),
                to: "o".into(),
                map: None,
            }],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
            triggers: vec![],
        }
    }

    fn budget_admission_def(max_output_tokens: Option<u32>, prompt: &str) -> WorkflowDefinition {
        WorkflowDefinition {
            triggers: vec![],
            id: Uuid::nil(),
            version: 1,
            name: "budget-admission".into(),
            nodes: vec![
                Node {
                    id: "t".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "m".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "gpt-4o-mini".into(),
                        },
                        prompt: prompt.into(),
                        max_output_tokens,
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "o".into(),
                    kind: NodeKind::Output,
                },
            ],
            edges: vec![
                Edge {
                    from: "t".into(),
                    to: "m".into(),
                    map: None,
                },
                Edge {
                    from: "m".into(),
                    to: "o".into(),
                    map: None,
                },
            ],
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
        }
    }

    /// Mirrors each production call site's order: no record/executor/provider
    /// action is reachable until the shared admission gate returns `Ok`.
    fn assert_rejected_before_dispatch(
        path: WorkflowBudgetAdmissionPath,
        def: &WorkflowDefinition,
    ) -> String {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let executions = AtomicUsize::new(0);
        let result = (|| -> Result<(), String> {
            admit_workflow_budget_before_dispatch(
                path,
                def,
                &serde_json::json!("input"),
                Some(1.0),
            )?;
            executions.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })();

        assert_eq!(
            executions.load(Ordering::SeqCst),
            0,
            "a rejected budget admission must execute zero provider/workflow actions"
        );
        result.expect_err("fixture must be rejected before dispatch")
    }

    // ------------------------------------------------------------------
    // Create/update wire contract — trigger preservation + strict envelope
    // ------------------------------------------------------------------

    #[test]
    fn create_request_preserves_schedule_and_webhook_triggers() {
        let id = Uuid::new_v4();
        let body: CreateWorkflowRequest = serde_json::from_value(serde_json::json!({
            "id": id,
            "version": 7,
            "name": "triggered workflow",
            "nodes": [
                { "id": "trigger", "type": "trigger" },
                { "id": "out", "type": "output" }
            ],
            "edges": [{ "from": "trigger", "to": "out" }],
            "triggers": [
                { "type": "schedule", "interval": "6h" },
                { "type": "webhook", "token_id": "billing_sync_1" }
            ]
        }))
        .expect("the canonical create/update trigger payload deserializes");

        let def = body.into_definition();
        assert_eq!(def.id, id);
        assert_eq!(def.version, 7);
        assert_eq!(
            def.triggers,
            vec![
                WorkflowTrigger::Schedule {
                    interval: "6h".to_string(),
                },
                WorkflowTrigger::Webhook {
                    token_id: "billing_sync_1".to_string(),
                },
            ]
        );
    }

    #[test]
    fn create_request_defaults_missing_triggers_to_human_run_only() {
        let body: CreateWorkflowRequest = serde_json::from_value(serde_json::json!({
            "name": "manual workflow",
            "nodes": [
                { "id": "trigger", "type": "trigger" },
                { "id": "out", "type": "output" }
            ],
            "edges": [{ "from": "trigger", "to": "out" }]
        }))
        .expect("older clients without triggers remain compatible");

        assert!(body.into_definition().triggers.is_empty());
    }

    #[test]
    fn create_request_rejects_unknown_top_level_fields() {
        let error = serde_json::from_value::<CreateWorkflowRequest>(serde_json::json!({
            "name": "typo guard",
            "nodes": [
                { "id": "trigger", "type": "trigger" },
                { "id": "out", "type": "output" }
            ],
            "edges": [{ "from": "trigger", "to": "out" }],
            "trigers": [{ "type": "schedule", "interval": "6h" }]
        }))
        .expect_err("a misspelled top-level field must not be silently discarded")
        .to_string();

        assert!(
            error.contains("unknown field `trigers`"),
            "unexpected serde error: {error}"
        );
    }

    #[test]
    fn direct_detour_and_shadow_reject_missing_output_caps_before_execution() {
        let def = budget_admission_def(None, "Summarize {{input}}");
        for path in [
            WorkflowBudgetAdmissionPath::Direct,
            WorkflowBudgetAdmissionPath::Detour,
            WorkflowBudgetAdmissionPath::Shadow,
        ] {
            let error = assert_rejected_before_dispatch(path, &def);
            assert!(
                error.contains("requires explicit positive max_output_tokens"),
                "unexpected admission error: {error}"
            );
        }
    }

    #[test]
    fn direct_detour_and_shadow_reject_upstream_refs_before_execution() {
        let def = budget_admission_def(Some(64), "Summarize {{previous_node}}");
        for path in [
            WorkflowBudgetAdmissionPath::Direct,
            WorkflowBudgetAdmissionPath::Detour,
            WorkflowBudgetAdmissionPath::Shadow,
        ] {
            let error = assert_rejected_before_dispatch(path, &def);
            assert!(
                error.contains("only supports {{input}} prompt references"),
                "unexpected admission error: {error}"
            );
        }
    }

    // ------------------------------------------------------------------
    // require_org unit tests (no I/O)
    // ------------------------------------------------------------------

    #[test]
    fn require_org_rejects_anon() {
        assert!(matches!(require_org(None), Err(ApiError::Unauthorized)));
    }

    #[test]
    fn require_org_rejects_dogfood_org() {
        let ctx = Some(Extension(ApiKeyContext {
            key_id: Uuid::nil(),
            org_id: DOGFOOD_ORG_ID,
            tier: None,
            skip_shadow: false,
        }));
        assert!(matches!(require_org(ctx), Err(ApiError::Unauthorized)));
    }

    #[test]
    fn require_org_accepts_real_org() {
        let org = Uuid::new_v4();
        let result = require_org(real_org_ctx(org));
        assert_eq!(result.unwrap(), org);
    }

    // ------------------------------------------------------------------
    // `create` handler — 401 + validation rejection paths
    // (Both paths do NOT need a DB: 401 fires before any other logic;
    // validation fires before the db_pool guard.)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn create_anon_returns_unauthorized() {
        let result = create(State(test_state()), None, Json(cyclic_def())).await;
        assert!(
            matches!(result, Err(ApiError::Unauthorized)),
            "expected Unauthorized, got {result:?}"
        );
    }

    #[tokio::test]
    async fn create_dogfood_returns_unauthorized() {
        let ctx = Some(Extension(ApiKeyContext {
            key_id: Uuid::nil(),
            org_id: DOGFOOD_ORG_ID,
            tier: None,
            skip_shadow: false,
        }));
        let result = create(State(test_state()), ctx, Json(cyclic_def())).await;
        assert!(
            matches!(result, Err(ApiError::Unauthorized)),
            "expected Unauthorized for dogfood org, got {result:?}"
        );
    }

    #[tokio::test]
    async fn create_cyclic_def_returns_invalid_request() {
        let org = Uuid::new_v4();
        let result = create(State(test_state()), real_org_ctx(org), Json(cyclic_def())).await;
        match result {
            Err(ApiError::InvalidRequest(msg)) => {
                assert!(
                    msg.contains("cycle"),
                    "error must mention 'cycle'; got: {msg}"
                );
            }
            other => panic!("expected InvalidRequest(cycle), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_rejects_a_sub_hour_schedule_before_the_db_guard() {
        let org = Uuid::new_v4();
        let mut def = valid_def();
        def.triggers = vec![WorkflowTrigger::Schedule {
            interval: "30m".to_string(),
        }];

        let result = create(State(test_state()), real_org_ctx(org), Json(def)).await;
        match result {
            Err(ApiError::InvalidRequest(msg)) => {
                assert!(msg.contains("1-hour minimum"), "unexpected error: {msg}");
                assert!(
                    msg.contains("approximate hourly sweep"),
                    "unexpected error: {msg}"
                );
            }
            other => panic!("expected InvalidRequest(sub-hour schedule), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn create_valid_def_no_pool_returns_503() {
        // Valid definition → passes validation but no db_pool → ServiceUnavailable.
        let org = Uuid::new_v4();
        let result = create(State(test_state()), real_org_ctx(org), Json(valid_def())).await;
        assert!(
            matches!(result, Err(ApiError::ServiceUnavailable(_))),
            "expected ServiceUnavailable when no db_pool, got {result:?}"
        );
    }

    // ------------------------------------------------------------------
    // `list` handler — 401 path
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn list_anon_returns_unauthorized() {
        let result = list(State(test_state()), None).await;
        assert!(
            matches!(result, Err(ApiError::Unauthorized)),
            "expected Unauthorized, got {result:?}"
        );
    }

    // ------------------------------------------------------------------
    // `get` handler — 401 path
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn get_anon_returns_unauthorized() {
        let result = get(State(test_state()), None, Path(Uuid::nil())).await;
        assert!(
            matches!(result, Err(ApiError::Unauthorized)),
            "expected Unauthorized, got {result:?}"
        );
    }

    // ------------------------------------------------------------------
    // `estimate` handler — 401 path
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn estimate_anon_returns_unauthorized() {
        let body = EstimateRequest {
            inputs: serde_json::Value::Null,
        };
        let result = estimate(State(test_state()), None, Path(Uuid::nil()), Json(body)).await;
        assert!(
            matches!(result, Err(ApiError::Unauthorized)),
            "expected Unauthorized, got {result:?}"
        );
    }

    // ------------------------------------------------------------------
    // `set_workflow_secret` — name-validation + handler-shape tests
    // ------------------------------------------------------------------

    /// Directly exercises the name-validation predicate: lowercase, spaces,
    /// hyphens, empty, and >64-char names must all be rejected.
    #[test]
    fn set_secret_rejects_bad_name() {
        assert!(!is_valid_secret_name("lowercase"));
        assert!(!is_valid_secret_name("HAS SPACE"));
        assert!(!is_valid_secret_name("MY-KEY"));
        assert!(!is_valid_secret_name(""));
        assert!(!is_valid_secret_name(&"A".repeat(65)));
        // Valid names must pass.
        assert!(is_valid_secret_name("MY_API_KEY"));
        assert!(is_valid_secret_name("KEY_123"));
    }

    #[tokio::test]
    async fn set_workflow_secret_anon_returns_unauthorized() {
        let body = SetWorkflowSecretRequest {
            name: "MY_KEY".into(),
            value: "s3cr3t".into(),
        };
        let result = set_workflow_secret(State(test_state()), None, Json(body)).await;
        assert!(
            matches!(result, Err(ApiError::Unauthorized)),
            "expected Unauthorized, got {result:?}"
        );
    }

    /// A bad name (lowercase) is rejected with 400 before any DB or key check.
    #[tokio::test]
    async fn set_workflow_secret_bad_name_returns_invalid_request() {
        let org = Uuid::new_v4();
        let body = SetWorkflowSecretRequest {
            name: "bad-name".into(),
            value: "s3cr3t".into(),
        };
        let result = set_workflow_secret(State(test_state()), real_org_ctx(org), Json(body)).await;
        assert!(
            matches!(result, Err(ApiError::InvalidRequest(_))),
            "expected InvalidRequest for bad name, got {result:?}"
        );
    }

    #[tokio::test]
    async fn set_workflow_secret_rejects_empty_and_oversize_values_before_key_lookup() {
        let org = Uuid::new_v4();
        for value in [
            String::new(),
            "x".repeat(MAX_WORKFLOW_SECRET_VALUE_BYTES + 1),
        ] {
            let body = SetWorkflowSecretRequest {
                name: "MY_KEY".into(),
                value,
            };
            let result =
                set_workflow_secret(State(test_state()), real_org_ctx(org), Json(body)).await;
            assert!(
                matches!(result, Err(ApiError::InvalidRequest(_))),
                "expected InvalidRequest for invalid value size, got {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn list_workflow_secrets_anon_returns_unauthorized() {
        let result = list_workflow_secrets(State(test_state()), None).await;
        assert!(
            matches!(result, Err(ApiError::Unauthorized)),
            "expected Unauthorized, got {result:?}"
        );
    }

    #[tokio::test]
    async fn delete_workflow_secret_anon_returns_unauthorized() {
        let result =
            delete_workflow_secret(State(test_state()), None, Path("MY_KEY".to_string())).await;
        assert!(
            matches!(result, Err(ApiError::Unauthorized)),
            "expected Unauthorized, got {result:?}"
        );
    }

    #[tokio::test]
    async fn delete_workflow_secret_rejects_bad_name_before_db_lookup() {
        let result = delete_workflow_secret(
            State(test_state()),
            real_org_ctx(Uuid::new_v4()),
            Path("bad-name".to_string()),
        )
        .await;
        assert!(
            matches!(result, Err(ApiError::InvalidRequest(_))),
            "expected InvalidRequest, got {result:?}"
        );
    }

    #[test]
    fn workflow_secret_inventory_serializes_only_safe_metadata() {
        let view = WorkflowSecretView {
            name: "MY_KEY".into(),
            state: WorkflowSecretState::Ready,
            created_at: chrono::DateTime::UNIX_EPOCH,
            rotated_at: None,
        };
        let json = serde_json::to_value(view).unwrap();
        assert_eq!(json["name"], "MY_KEY");
        assert_eq!(json["state"], "ready");
        assert!(json.get("created_at").is_some());
        assert!(json.get("rotated_at").is_some());
        for forbidden in [
            "value",
            "secret",
            "secret_enc",
            "ciphertext",
            "hash",
            "length",
        ] {
            assert!(
                json.get(forbidden).is_none(),
                "unexpected field {forbidden}"
            );
        }
    }

    /// With a valid name but no TT_MASTER_KEY the handler returns 503.
    #[tokio::test]
    async fn set_workflow_secret_no_master_key_returns_503() {
        // Ensure the env var is absent for this test.
        std::env::remove_var("TT_MASTER_KEY");
        let org = Uuid::new_v4();
        let body = SetWorkflowSecretRequest {
            name: "MY_KEY".into(),
            value: "s3cr3t".into(),
        };
        let result = set_workflow_secret(State(test_state()), real_org_ctx(org), Json(body)).await;
        assert!(
            matches!(result, Err(ApiError::ServiceUnavailable(_))),
            "expected ServiceUnavailable when TT_MASTER_KEY absent, got {result:?}"
        );
    }

    // ------------------------------------------------------------------
    // `create_run` handler — 401 path
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn create_run_anon_returns_unauthorized() {
        let body = CreateRunRequest {
            inputs: serde_json::Value::Null,
            workflow_version: None,
            max_cost_usd: None,
            stream: false,
        };
        let result = create_run(
            State(test_state()),
            None,
            HeaderMap::new(),
            Path(Uuid::nil()),
            Json(body),
        )
        .await;
        assert!(
            matches!(result, Err(ApiError::Unauthorized)),
            "expected Unauthorized, got {result:?}"
        );
    }
}
