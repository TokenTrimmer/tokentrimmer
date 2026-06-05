//! The TokenTrimmer model catalog as served by the gateway's `GET /v1/models`.
//! Consumed by `tt models` and by `tt chat` (for real context-window budgets).

use std::collections::HashMap;

use anyhow::Context as _;
use serde::Deserialize;

use crate::context::ResolvedContext;
use crate::ui;

/// One model from the gateway catalog (flattened from the `/v1/models` shape).
pub struct CatalogModel {
    pub id: String,
    pub provider: String,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
    pub capabilities: Vec<String>,
    pub input_per_million: Option<f64>,
    pub output_per_million: Option<f64>,
}

#[derive(Deserialize)]
struct RawResponse {
    data: Vec<RawEntry>,
}
#[derive(Deserialize)]
struct RawEntry {
    id: String,
    tokentrimmer: RawMeta,
}
#[derive(Deserialize)]
struct RawMeta {
    provider: String,
    #[serde(default)]
    capabilities: Vec<String>,
    max_input_tokens: u64,
    max_output_tokens: u64,
    #[serde(default)]
    pricing: Option<RawPricing>,
}
#[derive(Deserialize)]
struct RawPricing {
    input_per_million: f64,
    output_per_million: f64,
}

/// Parse a `/v1/models` response body into `CatalogModel`s.
pub fn parse_catalog(json: &str) -> anyhow::Result<Vec<CatalogModel>> {
    let resp: RawResponse = serde_json::from_str(json).context("parse /v1/models response")?;
    Ok(resp
        .data
        .into_iter()
        .map(|e| CatalogModel {
            id: e.id,
            provider: e.tokentrimmer.provider,
            max_input_tokens: e.tokentrimmer.max_input_tokens,
            max_output_tokens: e.tokentrimmer.max_output_tokens,
            capabilities: e.tokentrimmer.capabilities,
            input_per_million: e.tokentrimmer.pricing.as_ref().map(|p| p.input_per_million),
            output_per_million: e
                .tokentrimmer
                .pricing
                .as_ref()
                .map(|p| p.output_per_million),
        })
        .collect())
}

/// Map of model id → input context window (for `tt chat`'s budget).
#[must_use]
pub fn windows_map(models: &[CatalogModel]) -> HashMap<String, u32> {
    models
        .iter()
        .map(|m| {
            (
                m.id.clone(),
                u32::try_from(m.max_input_tokens).unwrap_or(u32::MAX),
            )
        })
        .collect()
}

/// Fetch the catalog from the gateway's `GET /v1/models`.
pub async fn fetch_catalog(
    http: &reqwest::Client,
    base: &str,
    key: &str,
) -> anyhow::Result<Vec<CatalogModel>> {
    let resp = http
        .get(format!("{base}/v1/models"))
        .bearer_auth(key)
        .send()
        .await
        .context("request /v1/models")?;
    if !resp.status().is_success() {
        anyhow::bail!("gateway returned {} for /v1/models", resp.status());
    }
    let body = resp.text().await.context("read /v1/models body")?;
    parse_catalog(&body)
}

/// Compact token-count display: `128000 → "128k"`, `1000000 → "1M"`.
#[must_use]
pub fn format_window(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{}M", tokens / 1_000_000)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

/// `tt models` — fetch and print the gateway model catalog as a table.
pub async fn run(flag_key: Option<String>, flag_base: Option<String>) -> anyhow::Result<()> {
    let ctx = ResolvedContext::load(flag_key, flag_base)?;
    let key = ctx
        .api_key_string()
        .context("no API key — run `tt login --token <KEY>` or set TT_API_KEY")?;
    let base = ctx.base_url.trim_end_matches('/').to_string();
    let http = reqwest::Client::new();
    let models = fetch_catalog(&http, &base, &key).await?;

    let mut table = ui::table(
        &["MODEL", "PROVIDER", "CONTEXT", "CAPS", "$IN/1M", "$OUT/1M"],
        console::colors_enabled(),
    );
    for m in &models {
        let price = |p: Option<f64>| p.map_or_else(|| "-".to_string(), |v| format!("{v:.2}"));
        table.add_row(vec![
            m.id.clone(),
            m.provider.clone(),
            format_window(m.max_input_tokens),
            m.capabilities.join(","),
            price(m.input_per_million),
            price(m.output_per_million),
        ]);
    }
    println!("{table}");
    ui::note(&format!("{} models", models.len()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "object": "list",
        "data": [
            { "id": "gpt-4o-mini", "object": "model", "owned_by": "openai",
              "tokentrimmer": { "provider": "openai",
                "pricing": { "input_per_million": 0.15, "output_per_million": 0.6, "effective_at": "2026-05-01T00:00:00Z" },
                "capabilities": ["text","tools"], "max_input_tokens": 128000, "max_output_tokens": 16000 } },
            { "id": "claude-haiku-4-5", "object": "model", "owned_by": "anthropic",
              "tokentrimmer": { "provider": "anthropic", "pricing": null,
                "capabilities": ["text","vision"], "max_input_tokens": 200000, "max_output_tokens": 8192 } }
        ]
    }"#;

    #[test]
    fn parse_catalog_extracts_models() {
        let m = parse_catalog(SAMPLE).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].id, "gpt-4o-mini");
        assert_eq!(m[0].provider, "openai");
        assert_eq!(m[0].max_input_tokens, 128_000);
        assert_eq!(m[0].capabilities, vec!["text", "tools"]);
        assert_eq!(m[0].input_per_million, Some(0.15));
        assert_eq!(m[1].input_per_million, None); // pricing: null
        assert!(parse_catalog("not json").is_err());
    }

    use httpmock::prelude::*;

    #[tokio::test]
    async fn fetch_catalog_parses_mock() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(GET).path("/v1/models");
            then.status(200)
                .header("content-type", "application/json")
                .body(SAMPLE);
        });
        let http = reqwest::Client::new();
        let models = fetch_catalog(&http, &server.base_url(), "k").await.unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[1].id, "claude-haiku-4-5");
    }

    #[tokio::test]
    async fn fetch_catalog_errors_on_5xx() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(GET).path("/v1/models");
            then.status(503).body("nope");
        });
        let http = reqwest::Client::new();
        assert!(fetch_catalog(&http, &server.base_url(), "k").await.is_err());
    }

    #[test]
    fn windows_map_and_format() {
        let m = parse_catalog(SAMPLE).unwrap();
        let w = windows_map(&m);
        assert_eq!(w.get("gpt-4o-mini").copied(), Some(128_000));
        assert_eq!(w.get("claude-haiku-4-5").copied(), Some(200_000));
        assert_eq!(format_window(128_000), "128k");
        assert_eq!(format_window(1_000_000), "1M");
        assert_eq!(format_window(2_000_000), "2M");
        assert_eq!(format_window(512), "512");
    }
}
