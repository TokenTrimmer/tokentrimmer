//! OpenAI-compatible Batch Lane endpoints (slice 2: submit / status, durable
//! `batch_jobs` store). NO async polling — the slice-3 worker fills realized
//! savings + drives long-running status. These handlers only proxy the slice-1
//! [`OpenAiProvider`](tt_provider_openai::OpenAiProvider) batch client and
//! persist/read an org-scoped row.
//!
//! Endpoints (all behind the gateway's normal API-key auth, org-scoped):
//!
//! * `POST /v1/files` — multipart upload (`purpose=batch` JSONL) →
//!   `upload_batch_input` → a complete OpenAI `FileObject`
//!   (`{id, object:"file", bytes, created_at, filename, purpose, status}`).
//! * `POST /v1/batches` — `create_batch` → persist a `batch_jobs` row
//!   (org-scoped) → return the `Batch`.
//! * `GET /v1/batches/{id}` — read the org's row (best-effort on-demand provider
//!   refresh) → return the `Batch`.
//! * `POST /v1/batches/{id}/cancel` — `cancel_batch` + update the row.
//! * `GET /v1/files/{id}/content` — authorize against the org's durable batch
//!   rows, then stream the provider-held result/error JSONL bytes.
//!
//! Org-scoping: the org is taken from the authenticated `tt_live_*` key's
//! [`ApiKeyContext`] (never caller-supplied). Every row write + read filters on
//! that org, so another org's batch id 404s. Anonymous / sandbox / dogfood
//! callers get 401.
//!
//! Provider resolution: OpenAI-only for now. The per-org OpenAI credential is
//! resolved EXACTLY as chat/embeddings do (the shared
//! [`resolve_credentials`](crate::routes::chat::resolve_credentials)), so a
//! verified org with no stored OpenAI key gets the same actionable
//! `missing_provider_credential` 400 — its `tt_live_*` bearer is never forwarded
//! upstream.

use axum::{
    body::Body,
    extract::{Multipart, Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tt_auth::ApiKeyContext;
use tt_provider_openai::OpenAiProvider;
use tt_shared::context::ProviderCredentials;
use tt_shared::{ProviderError, RequestContext};
use uuid::Uuid;

use crate::batch_store::{BatchJob, BatchStore};
use crate::budget_reservation::{
    derive_budget_dispatch, dispatch_state_for_idempotency, BudgetDispatchKind,
    BudgetReservationError, BudgetReservationRequest, ReservationAdmission,
};
use crate::error::{ApiError, ApiResult};
use crate::routes::chat::resolve_credentials;
use crate::{AppState, DOGFOOD_ORG_ID};

/// The provider the Batch Lane serves today. Structured so a later abstraction
/// can pick a provider per-request; slice 2 is OpenAI-only.
const BATCH_PROVIDER: &str = "openai";

/// Resolve the caller's real org, or 401. Anonymous (`None`) and the dogfood
/// identity are rejected — batch jobs are billable, org-scoped resources that
/// require a real `tt_live_*` key (mirrors `routes_api::require_org`).
fn require_org(ctx: Option<Extension<ApiKeyContext>>) -> Result<(Uuid, Uuid), ApiError> {
    match ctx {
        Some(Extension(c)) if c.org_id != DOGFOOD_ORG_ID => Ok((c.org_id, c.key_id)),
        _ => Err(ApiError::Unauthorized),
    }
}

/// The durable Batch store, or 503 when the gateway has none wired.
fn store(state: &AppState) -> ApiResult<&Arc<dyn BatchStore>> {
    state.batch_store.as_ref().ok_or_else(|| {
        ApiError::ServiceUnavailable("batch processing is not configured on this gateway".into())
    })
}

/// The concrete OpenAI adapter, or 503 when OpenAI is not registered. The Batch
/// Lane calls inherent slice-1 methods that the type-erased dispatch handle
/// cannot reach.
fn openai(state: &AppState) -> ApiResult<Arc<OpenAiProvider>> {
    state.registry.openai().ok_or_else(|| {
        ApiError::ServiceUnavailable("OpenAI provider is not configured on this gateway".into())
    })
}

/// The raw bearer token (sans `Bearer ` prefix), used by `resolve_credentials`
/// for the legacy anonymous BYO passthrough. For a verified org the store hit
/// wins and this is an inert placeholder.
fn raw_bearer(headers: &HeaderMap) -> String {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.strip_prefix("Bearer ")
                .or_else(|| s.strip_prefix("bearer "))
        })
        .unwrap_or("")
        .to_string()
}

/// Resolve the org's OpenAI upstream credential exactly as chat/embeddings do,
/// then build a minimal [`RequestContext`] for the batch client call. A verified
/// org with no stored OpenAI credential gets `missing_provider_credential` (its
/// `tt_live_*` bearer is never forwarded upstream).
async fn batch_ctx(
    state: &AppState,
    org_id: Uuid,
    api_key_id: Uuid,
    headers: &HeaderMap,
) -> ApiResult<RequestContext> {
    let bearer = raw_bearer(headers);
    // `resolve_credentials` returns `None` exactly when a VERIFIED org has no
    // stored OpenAI credential (BYO-only, P0 #9) — surface the actionable
    // missing-credential error rather than forward the org's `tt_live_*` bearer
    // upstream. An anonymous caller (which never reaches here — `require_org`
    // 401s first) or a store-less dev gateway gets the raw-bearer passthrough.
    let credentials: ProviderCredentials =
        resolve_credentials(state, org_id, BATCH_PROVIDER, &bearer)
            .await
            .ok_or_else(|| ApiError::MissingProviderCredential {
                provider: BATCH_PROVIDER.to_string(),
            })?;
    Ok(RequestContext {
        budget_dispatch: tt_shared::context::BudgetDispatchState::default(),
        trace_id: Uuid::now_v7(),
        org_id,
        api_key_id,
        credentials,
        tag: None,
        deadline: None,
        run_id: None,
        node_id: None,
    })
}

// ── POST /v1/files ──────────────────────────────────────────────────────────

/// `POST /v1/files` — accept the multipart upload (`purpose=batch` JSONL) and
/// proxy it via `upload_batch_input` to the org's OpenAI provider. Returns a
/// complete OpenAI-compatible `FileObject` (`id`, `object`, `bytes`,
/// `created_at`, `filename`, `purpose`, `status`) so the OpenAI SDK's
/// `files.create` parses it.
///
/// We read the `file` part's bytes off the multipart body in the gateway and
/// re-emit a fresh multipart to the upstream (the slice-1 client owns the
/// upstream multipart framing). The `purpose` field is accepted but pinned to
/// `batch` upstream — this endpoint exists solely to seed a batch.
pub async fn upload_file(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> ApiResult<Response> {
    let (org_id, api_key_id) = require_org(ctx)?;
    let provider = openai(&state)?;
    // Validate the store/credential wiring up front (consistent 503/400 before
    // we read the body).
    let _ = store(&state)?;
    let request_ctx = batch_ctx(&state, org_id, api_key_id, &headers).await?;

    let mut file_bytes: Option<Vec<u8>> = None;
    // The multipart filename of the `file` part, surfaced in the FileObject
    // response so the OpenAI SDK's `FileObject.filename` (a required field)
    // round-trips. Defaults to `batch.jsonl` when the client omits one.
    let mut file_name: Option<String> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::InvalidRequest(format!("malformed multipart body: {e}")))?
    {
        match field.name() {
            Some("file") => {
                // Capture the filename BEFORE consuming the field's bytes (the
                // `bytes()` call below moves `field`).
                file_name = field.file_name().map(str::to_string);
                let data = field
                    .bytes()
                    .await
                    .map_err(|e| ApiError::InvalidRequest(format!("reading file part: {e}")))?;
                file_bytes = Some(data.to_vec());
            }
            // `purpose` (and any other field) is read + ignored: the upstream
            // upload is pinned to purpose=batch by the slice-1 client.
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let bytes = file_bytes.ok_or_else(|| {
        ApiError::InvalidRequest("multipart upload is missing the 'file' part".into())
    })?;

    // Capture the byte length BEFORE `bytes` is moved into the upload call so we
    // can report it in the FileObject `bytes` field.
    let byte_len = bytes.len();
    let filename = file_name.unwrap_or_else(|| "batch.jsonl".to_string());

    let file_id = provider
        .upload_batch_input(bytes, &request_ctx)
        .await
        .map_err(ApiError::from)?;

    // Return a complete OpenAI `FileObject` so the OpenAI SDK's `files.create`
    // can parse it (it requires id, bytes, created_at, filename, object,
    // purpose, status). `status: "processed"` is the terminal status for a
    // successfully stored batch input.
    let body = serde_json::json!({
        "id": file_id,
        "object": "file",
        "bytes": byte_len as u64,
        "created_at": chrono::Utc::now().timestamp(),
        "filename": filename,
        "purpose": "batch",
        "status": "processed",
    });
    Ok((StatusCode::OK, Json(body)).into_response())
}

// ── POST /v1/batches ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize)]
pub struct CreateBatchRequest {
    pub input_file_id: String,
    pub endpoint: String,
    pub completion_window: String,
}

async fn enforce_batch_budget(
    state: &AppState,
    org_id: Uuid,
    api_key_id: Uuid,
    headers: &HeaderMap,
    req: &CreateBatchRequest,
) -> ApiResult<()> {
    let idempotency_key =
        super::idempotency_key_from_headers(headers)?.unwrap_or_else(|| Uuid::now_v7().to_string());
    let Some(store) = state.budget_reservation_store.as_ref() else {
        return Ok(());
    };
    let dispatch_state = dispatch_state_for_idempotency(org_id, api_key_id, &idempotency_key);
    let canonical =
        serde_json::to_string(req).map_err(|error| ApiError::InvalidRequest(error.to_string()))?;
    let dispatch = derive_budget_dispatch(
        &dispatch_state,
        BATCH_PROVIDER,
        BudgetDispatchKind::Batch,
        "batch",
        &canonical,
    );
    match store
        .reserve(BudgetReservationRequest {
            org_id,
            api_key_id,
            trace_id: Uuid::now_v7(),
            dispatch,
            model: "batch input file",
            estimated_usd: None,
            now: chrono::Utc::now(),
        })
        .await
    {
        Ok(ReservationAdmission::NotCapped) => Ok(()),
        Ok(ReservationAdmission::Reserved(_)) => Err(ApiError::ServiceUnavailable(
            "batch budget admission produced an unpriced reservation".into(),
        )),
        Err(BudgetReservationError::Exceeded {
            estimated_usd,
            remaining_usd,
        }) => Err(ApiError::Provider(ProviderError::BudgetExceeded {
            estimated_usd,
            remaining_usd,
        })),
        Err(BudgetReservationError::PriceUnknown { model }) => {
            Err(ApiError::Provider(ProviderError::BudgetPriceUnknown {
                model,
            }))
        }
        Err(BudgetReservationError::Unavailable(message)) => Err(ApiError::Provider(
            ProviderError::BudgetUnavailable(message),
        )),
    }
}

/// `POST /v1/batches` — create the batch upstream via `create_batch`, persist an
/// org-scoped `batch_jobs` row, and return the OpenAI-compatible `Batch`.
pub async fn create_batch(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    headers: HeaderMap,
    Json(req): Json<CreateBatchRequest>,
) -> ApiResult<Response> {
    let (org_id, api_key_id) = require_org(ctx)?;
    let provider = openai(&state)?;
    let store = store(&state)?.clone();
    enforce_batch_budget(&state, org_id, api_key_id, &headers, &req).await?;
    let request_ctx = batch_ctx(&state, org_id, api_key_id, &headers).await?;

    let batch = provider
        .create_batch(
            &req.input_file_id,
            &req.endpoint,
            &req.completion_window,
            &request_ctx,
        )
        .await
        .map_err(ApiError::from)?;

    // Persist BEFORE returning so a subsequent GET (and the slice-3 worker) can
    // find the row. A store error fails the request — the batch exists upstream
    // but we must not return a handle we can't track.
    let job = BatchJob::from_batch(org_id, Some(api_key_id), BATCH_PROVIDER, &batch);
    store
        .insert(job.clone())
        .await
        .map_err(|e| ApiError::Internal(format!("persisting batch job: {e}")))?;

    Ok((StatusCode::OK, Json(job.to_openai_json())).into_response())
}

// ── GET /v1/batches/{id} ──────────────────────────────────────────────────────

/// `GET /v1/batches/{id}` — read the org's `batch_jobs` row and return the
/// `Batch`. A best-effort on-demand provider refresh (`get_batch`) keeps the
/// stored status fresh for an interactive caller; the slice-3 worker is the
/// durable poller. A refresh failure is non-fatal — the stored row is still
/// returned (the worker reconciles later). Another org's id 404s (the row is
/// fetched org-scoped, so it simply isn't found).
pub async fn get_batch(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let (org_id, api_key_id) = require_org(ctx)?;
    let store = store(&state)?.clone();

    let mut job = store
        .get(org_id, &id)
        .await
        .map_err(|e| ApiError::Internal(format!("reading batch job: {e}")))?
        .ok_or_else(|| ApiError::NotFound(format!("no batch with id {id}")))?;

    // Best-effort on-demand refresh (slice-3 worker is the durable poller). Only
    // refresh while the batch is still in a non-terminal state — a settled batch
    // needs no upstream round-trip. A refresh error is swallowed: the stored row
    // is authoritative until the worker reconciles.
    if !is_terminal(&job.status) {
        if let Ok(provider) = openai(&state) {
            if let Ok(request_ctx) = batch_ctx(&state, org_id, api_key_id, &headers).await {
                let provider_id = job
                    .provider_batch_id
                    .clone()
                    .unwrap_or_else(|| job.id.clone());
                if let Ok(fresh) = provider.get_batch(&provider_id, &request_ctx).await {
                    job.apply_provider_update(&fresh);
                    // Persist the refreshed view; a write error is non-fatal.
                    if let Err(e) = store.update(job.clone()).await {
                        tracing::warn!(error = %e, batch_id = %id, "batch status refresh persist failed");
                    }
                }
            }
        }
    }

    Ok((StatusCode::OK, Json(job.to_openai_json())).into_response())
}

/// Terminal batch states need no further provider refresh.
fn is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "expired" | "cancelled")
}

// ── GET /v1/batches ────────────────────────────────────────────────────────────

/// `GET /v1/batches` — list the org's `batch_jobs` rows (newest-first) as an
/// OpenAI-compatible list envelope `{"object":"list","data":[<batch>...]}`. Each
/// item is the SAME shape `GET /v1/batches/{id}` returns (`to_openai_json`), so
/// list and single-get agree byte-for-byte. Strictly org-scoped — never another
/// org's batches. No per-row provider refresh: the slice-3 worker is the durable
/// poller and a list of N batches must not fan out N upstream round-trips.
pub async fn list_batches(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
) -> ApiResult<Response> {
    let (org_id, _api_key_id) = require_org(ctx)?;
    let store = store(&state)?.clone();

    let jobs = store
        .list(org_id)
        .await
        .map_err(|e| ApiError::Internal(format!("listing batch jobs: {e}")))?;

    let data: Vec<serde_json::Value> = jobs.iter().map(BatchJob::to_openai_json).collect();
    let body = serde_json::json!({ "object": "list", "data": data });
    Ok((StatusCode::OK, Json(body)).into_response())
}

// ── POST /v1/batches/{id}/cancel ──────────────────────────────────────────────

/// `POST /v1/batches/{id}/cancel` — cancel the batch upstream via `cancel_batch`
/// and update the org's row. Another org's id 404s (the row is fetched
/// org-scoped first, so cancel never reaches a foreign batch).
pub async fn cancel_batch(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let (org_id, api_key_id) = require_org(ctx)?;
    let provider = openai(&state)?;
    let store = store(&state)?.clone();

    // Org-scoped fetch first: a foreign batch id is unknown to this org → 404,
    // and we never issue an upstream cancel for a batch this org doesn't own.
    let mut job = store
        .get(org_id, &id)
        .await
        .map_err(|e| ApiError::Internal(format!("reading batch job: {e}")))?
        .ok_or_else(|| ApiError::NotFound(format!("no batch with id {id}")))?;

    let request_ctx = batch_ctx(&state, org_id, api_key_id, &headers).await?;
    let provider_id = job
        .provider_batch_id
        .clone()
        .unwrap_or_else(|| job.id.clone());
    let cancelled = provider
        .cancel_batch(&provider_id, &request_ctx)
        .await
        .map_err(ApiError::from)?;

    job.apply_provider_update(&cancelled);
    store
        .update(job.clone())
        .await
        .map_err(|e| ApiError::Internal(format!("updating batch job: {e}")))?;

    Ok((StatusCode::OK, Json(job.to_openai_json())).into_response())
}

// ── GET /v1/files/{id}/content ────────────────────────────────────────────────

/// `GET /v1/files/{id}/content` — proxy `stream_file_content` without
/// accumulating the successful provider body, and stream the raw result/error
/// JSONL bytes back to the caller. The exact file id must first appear on one of
/// the authenticated organization's durable batch rows; a provider credential
/// alone is not an ownership boundary because organizations can share an
/// upstream account. Returns the bytes as `application/jsonl`.
pub async fn download_file_content(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let (org_id, api_key_id) = require_org(ctx)?;
    let store = store(&state)?.clone();
    let owns_file = store
        .owns_file(org_id, &id)
        .await
        .map_err(|e| ApiError::Internal(format!("checking batch file ownership: {e}")))?;
    if !owns_file {
        return Err(ApiError::NotFound("batch file not found".to_string()));
    }
    let provider = openai(&state)?;
    let request_ctx = batch_ctx(&state, org_id, api_key_id, &headers).await?;

    let stream = provider
        .stream_file_content(&id, &request_ctx)
        .await
        .map_err(ApiError::from)?;

    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/jsonl")],
        Body::from_stream(stream),
    )
        .into_response())
}
