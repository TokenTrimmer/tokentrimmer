//! `GET /v1/models` — list all models from all registered providers.
//!
//! OpenAI-compatible response shape, augmented with a `tokentrimmer` block per
//! the gateway API reference (`docs/04-gateway-api-reference.md`).

use axum::{extract::State, Json};
use serde::Serialize;
use tt_shared::{ModelInfo, ModelPricing};

use crate::AppState;

#[derive(Serialize)]
pub struct ModelsResponse {
    pub object: &'static str,
    pub data: Vec<ModelEntry>,
}

#[derive(Serialize)]
pub struct ModelEntry {
    pub id: String,
    pub object: &'static str,
    pub owned_by: String,
    pub tokentrimmer: TokenTrimmerMeta,
}

#[derive(Serialize)]
pub struct TokenTrimmerMeta {
    pub provider: String,
    pub pricing: Option<ModelPricing>,
    pub capabilities: Vec<String>,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
}

pub async fn handler(State(state): State<AppState>) -> Json<ModelsResponse> {
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
    Json(ModelsResponse {
        object: "list",
        data,
    })
}

fn model_entry(info: &ModelInfo, provider_id: &str, pricing: Option<ModelPricing>) -> ModelEntry {
    ModelEntry {
        id: info.id.clone(),
        object: "model",
        owned_by: provider_id.to_string(),
        tokentrimmer: TokenTrimmerMeta {
            provider: provider_id.to_string(),
            pricing,
            capabilities: info
                .capabilities
                .iter()
                .map(|c| format!("{c:?}").to_lowercase())
                .collect(),
            max_input_tokens: info.max_input_tokens,
            max_output_tokens: info.max_output_tokens,
        },
    }
}
