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
//! `store::insert_node_run` is async. Solution: collect journal entries into a
//! `Vec` inside the sync closure, then loop + `await` each one **after**
//! `run_workflow` returns. Best-effort: a DB error never fails the run response.
//!
//! # Timeouts: non-streaming runs use the 60 s `short` group; `stream=true` uses 600 s `streaming`.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
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
    workflow::{
        self,
        engine::{self, WfStatus},
        estimate,
        events::WfEvent,
        executor::GatewayNodeExecutor,
        secrets::{is_valid_secret_name, load_secrets, master_key_from_env, store_secret},
        store::{self, WorkflowRunRecord},
        types::content_hash,
        validate,
    },
    AppState, DOGFOOD_ORG_ID,
};

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

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

/// `POST /v1/workflows` request body.  Both `id` and `version` are optional:
/// if `id` is absent a new `UUIDv4` is generated; `version` is ignored (the
/// store computes the next version atomically via `MAX(version)+1`).
#[derive(Debug, Deserialize)]
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

/// Per-node output inside [`CreateRunResponse`].
#[derive(Debug, Serialize)]
pub struct NodeOutputView {
    pub node_id: String,
    pub content: serde_json::Value,
    pub cost_usd: f64,
}

/// `POST /v1/workflows/secrets` request body.
#[derive(Debug, Deserialize)]
pub struct SetWorkflowSecretRequest {
    pub name: String,
    /// The plaintext secret value. Encrypted at rest; **never returned**.
    pub value: String,
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

    // Assemble a full WorkflowDefinition from the request body.
    let def = workflow::WorkflowDefinition {
        id: body.id.unwrap_or_else(Uuid::new_v4),
        version: body.version.unwrap_or(0),
        name: body.name,
        nodes: body.nodes,
        edges: body.edges,
        inputs: body.inputs,
        budget: body.budget,
        allowed_hosts: body.allowed_hosts,
        // WF-3: forward editor metadata (canvas positions) through to the stored
        // definition. Body.metadata defaults to Null when the editor omits it.
        metadata: body.metadata,
    };

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
        store::insert_node_run(
            pool,
            run_id,
            &entry.node_id,
            &entry.status,
            entry.output,
            entry.cost_usd,
            entry.model_used.as_deref(),
            entry.error.as_deref(),
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
/// [`store::upsert_quality_verdict`] (the cloud mint reads it to sign `wfr:v2`).
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

    // --- Load + defense-in-depth validate -----------------------------------
    let (def, version) = store::get_definition(pool, org, id)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("no workflow with id {id}")))?;

    {
        let registry = state.registry.clone();
        validate::validate(&def, &|m| registry.resolve(m).is_some())
            .map_err(|errors| ApiError::InvalidRequest(errors.join("; ")))?;
    }

    // --- Insert initial run record (status = "running") ----------------------
    let run_id = Uuid::new_v4();
    let run_max_cost = def.budget.max_cost_usd.or(body.max_cost_usd);
    let inputs = body.inputs; // extract for ownership — moved/borrowed per branch below
    store::insert_run(
        pool,
        &WorkflowRunRecord {
            id: run_id,
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
        },
    )
    .await;

    // --- Load org secrets once (both sync + streaming paths) -----------------
    // Empty map when TT_MASTER_KEY is absent — Http nodes without secrets work,
    // {{secrets.*}} refs just resolve to "".
    let secrets = match master_key_from_env() {
        Some(master) => load_secrets(pool, org, &master).await,
        None => std::collections::HashMap::new(),
    };

    // Capture the bearer before the executor (below) moves it; the flow
    // quality-gate spawn uses it to resolve the judge model's credentials.
    let raw_bearer_for_gate = raw_bearer.clone();

    if !body.stream {
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
// GET /v1/workflows/:id/runs + GET /v1/workflows/runs/:run_id (WF-6)
// ---------------------------------------------------------------------------
// The run rows are persisted by `create_run`/`persist_run_results`
// (`workflow_runs` + `workflow_node_runs`); this exposes them over HTTP so a run
// that spent real dollars doesn't vanish on navigation. Org-scoped: a run is
// only ever returned to its owning org (`require_org` + the SQL's org_id filter).

/// A single workflow run row as returned to API clients. Mirrors
/// [`store::WorkflowRunRecord`] minus the org_id (never echoed) + with the
/// timestamps as RFC 3339 (JSON-friendly).
#[derive(Debug, Serialize)]
pub struct WorkflowRunView {
    pub id: Uuid,
    pub workflow_id: Uuid,
    pub version: i32,
    /// `"running"` / `"completed"` / `"failed"`.
    pub status: String,
    pub inputs: Option<serde_json::Value>,
    pub cost_usd: f64,
    pub max_cost_usd: Option<f64>,
    pub baseline_cost_usd: f64,
    pub saved_usd: f64,
    pub error: Option<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl WorkflowRunView {
    fn from_record(r: store::WorkflowRunRecord) -> Self {
        Self {
            id: r.id,
            workflow_id: r.workflow_id,
            version: r.version,
            status: r.status,
            inputs: r.inputs,
            cost_usd: r.cost_usd,
            max_cost_usd: r.max_cost_usd,
            baseline_cost_usd: r.baseline_cost_usd,
            saved_usd: r.saved_usd,
            error: r.error,
            started_at: r.started_at,
            finished_at: r.finished_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ListWorkflowRunsResponse {
    pub object: &'static str,
    pub data: Vec<WorkflowRunView>,
}

/// `GET /v1/workflows/:id/runs` — list recent runs of a workflow, scoped to the
/// caller's org. The workflow id is validated as org-owned the same way
/// `create_run` does (store::get_definition returns None for a foreign-org id).
pub async fn list_workflow_runs(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<ListWorkflowRunsResponse>> {
    let org = require_org(ctx)?;
    let pool = db_pool(&state)?;
    // Defense-in-depth: confirm the workflow exists + is org-scoped before
    // listing its runs (a foreign org id → 404, not an empty list that might
    // imply the workflow exists).
    if store::get_definition(pool, org, id).await.is_none() {
        return Err(ApiError::NotFound(format!("no workflow with id {id}")));
    }
    let runs = store::list_runs(pool, org, 50).await;
    let data = runs.into_iter().map(WorkflowRunView::from_record).collect();
    Ok(Json(ListWorkflowRunsResponse {
        object: "list",
        data,
    }))
}

/// `GET /v1/workflows/runs/:run_id` — fetch a single run by id (org-scoped via
/// the SQL's `WHERE id AND org_id`, so a foreign-org run id → 404).
pub async fn get_workflow_run(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(run_id): Path<Uuid>,
) -> ApiResult<Json<WorkflowRunView>> {
    let org = require_org(ctx)?;
    let pool = db_pool(&state)?;
    let run = store::get_run(pool, run_id, org)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("no workflow run with id {run_id}")))?;
    Ok(Json(WorkflowRunView::from_record(run)))
}

/// `POST /v1/workflows/secrets` — encrypt and upsert a named secret for the
/// caller's org.
///
/// * `name` must match `^[A-Z0-9_]{1,64}$` (the charset used by
///   `{{secrets.NAME}}` template references in Http nodes) → 400 otherwise.
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ProviderRegistry;
    use crate::workflow::types::{BudgetPolicy, Edge, Node, NodeKind};
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
