//! `POST /v1/embeddings` — OpenAI-compatible embeddings.
//!
//! Skeleton handler — full provider dispatch lands when the OpenAI adapter
//! ships `embeddings()` (Week 8-9, when L2 semantic cache needs them).

use axum::{extract::State, Json};
use tt_shared::{EmbeddingsRequest, EmbeddingsResponse};

use crate::{ApiError, ApiResult, AppState};

pub async fn handler(
    State(_state): State<AppState>,
    Json(req): Json<EmbeddingsRequest>,
) -> ApiResult<Json<EmbeddingsResponse>> {
    Err(ApiError::ModelNotFound { model: req.model })
}
