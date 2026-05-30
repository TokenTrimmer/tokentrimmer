//! `POST /v1/embeddings` — OpenAI-compatible embeddings.
//!
//! Skeleton handler — full provider dispatch lands when the OpenAI adapter
//! ships `embeddings()` (Week 8-9, when L2 semantic cache needs them).

use axum::{extract::State, Json};
use tt_shared::{EmbeddingsRequest, EmbeddingsResponse, ProviderError};

use crate::{ApiError, ApiResult, AppState};

pub async fn handler(
    State(_state): State<AppState>,
    Json(_req): Json<EmbeddingsRequest>,
) -> ApiResult<Json<EmbeddingsResponse>> {
    // Embeddings are currently used only internally (L2 semantic cache); the
    // public endpoint isn't dispatched to a provider yet. Return 501 Not
    // Implemented rather than a misleading 404 model-not-found, so a client
    // doing a one-line base-URL swap can tell the endpoint is unimplemented
    // (not that its model is unknown). (§4.15)
    Err(ApiError::Provider(ProviderError::Unsupported(
        "the /v1/embeddings endpoint is not implemented yet".into(),
    )))
}
