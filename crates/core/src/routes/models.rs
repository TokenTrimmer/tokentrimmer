//! `GET /v1/models` — list all models from all registered providers.
//!
//! OpenAI-compatible response shape, augmented with a `tokentrimmer` block per
//! the gateway API reference (`docs/04-gateway-api-reference.md`).

use axum::{
    extract::State,
    http::{header, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use sha2::{Digest, Sha256};
pub use tt_shared::{
    ModelCatalogLimitations, ModelEntry, ModelsDocumentMeta, ModelsResponse, MODELS_SCHEMA_VERSION,
};
use tt_shared::{
    ModelInfo, ModelPricing, ModelTokenTrimmerMeta, MODELS_FLEET_CONSISTENCY,
    MODELS_PROVIDER_CREDENTIALS, MODELS_PROVIDER_HEALTH, MODELS_REQUEST_ACCEPTANCE,
    MODELS_SNAPSHOT_SCOPE, MODELS_SOURCE,
};

use crate::{ApiError, ApiResult, AppState};

pub async fn handler(State(state): State<AppState>) -> ApiResult<Response> {
    let mut data = Vec::new();
    for (_provider_id, provider) in state.registry.iter() {
        for info in provider.models() {
            let pricing = provider.pricing(&info.id);
            data.push(model_entry(&info, provider.id(), pricing));
        }
    }
    // Registry iteration order is unspecified (HashMap), which would make the
    // catalog response non-deterministic across restarts. Sort by
    // (provider, model id) so clients, snapshots, and diffs see a stable list.
    data.sort_by(|a, b| a.owned_by.cmp(&b.owned_by).then_with(|| a.id.cmp(&b.id)));

    let snapshot_bytes = serde_json::to_vec(&data)
        .map_err(|_| ApiError::Internal("failed to identify model catalog snapshot".into()))?;
    let document = ModelsResponse {
        object: "list".into(),
        data,
        tokentrimmer: ModelsDocumentMeta {
            schema_version: MODELS_SCHEMA_VERSION,
            snapshot_scope: MODELS_SNAPSHOT_SCOPE.into(),
            source: MODELS_SOURCE.into(),
            snapshot_sha256: hex::encode(Sha256::digest(snapshot_bytes)),
            limitations: ModelCatalogLimitations {
                provider_credentials: MODELS_PROVIDER_CREDENTIALS.into(),
                provider_health: MODELS_PROVIDER_HEALTH.into(),
                request_acceptance: MODELS_REQUEST_ACCEPTANCE.into(),
                fleet_consistency: MODELS_FLEET_CONSISTENCY.into(),
            },
        },
    };
    let mut response = Json(document).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

fn model_entry(info: &ModelInfo, provider_id: &str, pricing: Option<ModelPricing>) -> ModelEntry {
    ModelEntry {
        id: info.id.clone(),
        object: "model".into(),
        owned_by: provider_id.to_string(),
        tokentrimmer: ModelTokenTrimmerMeta {
            provider: provider_id.to_string(),
            pricing,
            capabilities: info.capabilities.clone(),
            max_input_tokens: info.max_input_tokens,
            max_output_tokens: info.max_output_tokens,
        },
    }
}
