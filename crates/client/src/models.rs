//! Bounded typed read for the responder-scoped `GET /v1/models` contract.

use std::collections::HashSet;
use std::time::Duration;

use reqwest::header::{ACCEPT, CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use sha2::{Digest, Sha256};
use tt_shared::{
    ModelEntry, ModelPricing, ModelsResponse, MODELS_FLEET_CONSISTENCY,
    MODELS_PROVIDER_CREDENTIALS, MODELS_PROVIDER_HEALTH, MODELS_REQUEST_ACCEPTANCE,
    MODELS_SCHEMA_VERSION, MODELS_SNAPSHOT_SCOPE, MODELS_SOURCE,
};

use crate::{Client, CostInfo, Error, Result};

/// The public model document is control metadata, not a bulk-data surface.
pub const MAX_MODELS_RESPONSE_BYTES: usize = 256 * 1024;
/// One deadline covers response headers and the complete bounded body.
pub const MODELS_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

impl Client {
    fn models_request(&self) -> Result<reqwest::RequestBuilder> {
        Ok(self
            .control_http
            .as_ref()
            .ok_or(Error::ControlMetadataClientUnavailable)?
            .get(format!("{}/v1/models", self.base))
            // `/v1/models` is deliberately anonymous. Do not send a configured
            // bearer to a read that neither needs nor authenticates it.
            .header(ACCEPT, "application/json")
            .timeout(MODELS_REQUEST_TIMEOUT))
    }

    /// Read and validate one exact responding gateway process's model catalog.
    ///
    /// The document supplies metadata only. A returned model does not prove a
    /// credential, provider health, request acceptance, tokenizer result, or
    /// fleet consistency; those limitations are required in the v1 envelope.
    ///
    /// # Errors
    /// Fails on transport/timeout, redirects, non-success status, a body above
    /// 256 KiB, non-JSON data, missing no-store/nosniff headers, a future or
    /// malformed contract, duplicate provider/model rows, or a snapshot digest
    /// mismatch. Remote error text is never surfaced.
    pub async fn models(&self) -> Result<ModelsResponse> {
        let endpoint = format!("{}/v1/models", self.base);
        let response = self
            .models_request()?
            .send()
            .await
            .map_err(Error::Request)?;
        if response.url().as_str() != endpoint {
            // Defense in depth for an unexpectedly replaced metadata client.
            let _ = read_bounded(response).await?;
            return Err(Error::UnexpectedModelCatalogRedirect);
        }
        let status = response.status();
        if status.is_redirection() {
            let _ = read_bounded(response).await?;
            return Err(Error::UnexpectedModelCatalogRedirect);
        }
        if !status.is_success() {
            // Drain only under the same control-metadata cap, then keep remote
            // prose out of the public SDK error.
            let _ = read_bounded(response).await?;
            return Err(Error::Status {
                status: status.as_u16(),
                body: "model catalog request failed".into(),
                cost: Box::<CostInfo>::default(),
            });
        }
        // Retain any header error while still consuming the success body under
        // the same cap before returning it.
        let header_result = validate_response_headers(&response);
        let body = read_bounded(response).await?;
        header_result?;
        let document =
            serde_json::from_slice::<ModelsResponse>(&body).map_err(Error::InvalidResponse)?;
        validate_document(&document)?;
        Ok(document)
    }
}

fn validate_response_headers(response: &reqwest::Response) -> Result<()> {
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !content_type
        .split(';')
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
    {
        return Err(Error::InvalidModelCatalog("content_type"));
    }
    let cache_control = response
        .headers()
        .get(CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !cache_control
        .split(',')
        .any(|directive| directive.trim().eq_ignore_ascii_case("no-store"))
    {
        return Err(Error::InvalidModelCatalog("cache_control"));
    }
    if response
        .headers()
        .get(X_CONTENT_TYPE_OPTIONS)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|value| !value.eq_ignore_ascii_case("nosniff"))
    {
        return Err(Error::InvalidModelCatalog("content_type_options"));
    }
    Ok(())
}

async fn read_bounded(mut response: reqwest::Response) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MODELS_RESPONSE_BYTES as u64)
    {
        return Err(Error::ResponseTooLarge {
            limit: MAX_MODELS_RESPONSE_BYTES,
        });
    }

    let mut body = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(MAX_MODELS_RESPONSE_BYTES as u64) as usize,
    );
    while let Some(chunk) = response.chunk().await.map_err(Error::Request)? {
        if chunk.len() > MAX_MODELS_RESPONSE_BYTES.saturating_sub(body.len()) {
            return Err(Error::ResponseTooLarge {
                limit: MAX_MODELS_RESPONSE_BYTES,
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn validate_document(document: &ModelsResponse) -> Result<()> {
    let meta = &document.tokentrimmer;
    if document.object != "list"
        || meta.schema_version != MODELS_SCHEMA_VERSION
        || meta.snapshot_scope != MODELS_SNAPSHOT_SCOPE
        || meta.source != MODELS_SOURCE
        || meta.limitations.provider_credentials != MODELS_PROVIDER_CREDENTIALS
        || meta.limitations.provider_health != MODELS_PROVIDER_HEALTH
        || meta.limitations.request_acceptance != MODELS_REQUEST_ACCEPTANCE
        || meta.limitations.fleet_consistency != MODELS_FLEET_CONSISTENCY
    {
        return Err(Error::InvalidModelCatalog("metadata"));
    }
    if meta.snapshot_sha256.len() != 64
        || !meta
            .snapshot_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::InvalidModelCatalog("snapshot_sha256"));
    }

    let mut seen = HashSet::new();
    for entry in &document.data {
        validate_entry(entry)?;
        if !seen.insert((entry.owned_by.as_str(), entry.id.as_str())) {
            return Err(Error::InvalidModelCatalog("duplicate_model"));
        }
    }

    let snapshot = serde_json::to_vec(&document.data).map_err(Error::InvalidResponse)?;
    if hex::encode(Sha256::digest(snapshot)) != meta.snapshot_sha256 {
        return Err(Error::InvalidModelCatalog("snapshot_mismatch"));
    }
    Ok(())
}

fn validate_entry(entry: &ModelEntry) -> Result<()> {
    if entry.object != "model"
        || entry.id.trim().is_empty()
        || entry.owned_by.trim().is_empty()
        || entry.tokentrimmer.provider != entry.owned_by
        || entry.tokentrimmer.max_input_tokens == 0
        || entry.tokentrimmer.capabilities.is_empty()
    {
        return Err(Error::InvalidModelCatalog("model_entry"));
    }
    if let Some(pricing) = &entry.tokentrimmer.pricing {
        validate_pricing(pricing)?;
    }
    Ok(())
}

fn validate_pricing(pricing: &ModelPricing) -> Result<()> {
    let required = [pricing.input_per_million, pricing.output_per_million];
    let optional = [
        pricing.cached_input_per_million,
        pricing.cache_write_per_million,
        pricing.batch_input_per_million,
        pricing.batch_output_per_million,
        pricing.flex_input_per_million,
        pricing.flex_output_per_million,
    ];
    if required
        .into_iter()
        .any(|value| !value.is_finite() || value < 0.0)
        || optional
            .into_iter()
            .flatten()
            .any(|value| !value.is_finite() || value < 0.0)
    {
        return Err(Error::InvalidModelCatalog("pricing"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use httpmock::prelude::*;
    use httpmock::Then;
    use tt_shared::{
        pricing::Capability, ModelCatalogLimitations, ModelTokenTrimmerMeta, ModelsDocumentMeta,
    };

    use super::*;

    fn model_data() -> Vec<ModelEntry> {
        vec![ModelEntry {
            id: "gpt-4o-mini".into(),
            object: "model".into(),
            owned_by: "openai".into(),
            tokentrimmer: ModelTokenTrimmerMeta {
                provider: "openai".into(),
                pricing: None,
                capabilities: vec![
                    Capability::Text,
                    Capability::Tools,
                    Capability::JsonMode,
                    Capability::Streaming,
                ],
                max_input_tokens: 128_000,
                max_output_tokens: 16_384,
            },
        }]
    }

    fn document(data: Vec<ModelEntry>) -> ModelsResponse {
        let snapshot = serde_json::to_vec(&data).expect("serialize fixture data");
        ModelsResponse {
            object: "list".into(),
            data,
            tokentrimmer: ModelsDocumentMeta {
                schema_version: 1,
                snapshot_scope: "responding_process".into(),
                source: "registered_provider_catalog".into(),
                snapshot_sha256: hex::encode(Sha256::digest(snapshot)),
                limitations: ModelCatalogLimitations {
                    provider_credentials: "not_inspected".into(),
                    provider_health: "not_probed".into(),
                    request_acceptance: "not_negotiated".into(),
                    fleet_consistency: "not_attested".into(),
                },
            },
        }
    }

    fn catalog_headers(then: Then) -> Then {
        then.header("content-type", "application/json")
            .header("cache-control", "private, no-store")
            .header("x-content-type-options", "nosniff")
    }

    #[tokio::test]
    async fn models_reads_exact_bounded_anonymous_contract() {
        let server = MockServer::start_async().await;
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/v1/models")
                .header("accept", "application/json");
            let then = catalog_headers(then);
            then.status(200).json_body_obj(&document(model_data()));
        });
        let client = Client::new(server.base_url(), "must-not-be-sent");
        let request = client
            .models_request()
            .expect("build metadata request")
            .build()
            .expect("build models request");
        assert!(request.headers().get("authorization").is_none());

        let response = client.models().await.expect("valid catalog");
        assert_eq!(response.data.len(), 1);
        assert_eq!(response.data[0].tokentrimmer.max_output_tokens, 16_384);
        mock.assert();
    }

    #[tokio::test]
    async fn models_rejects_snapshot_mismatch_and_remote_error_text() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(GET).path("/v1/models");
            let mut value = document(model_data());
            value.tokentrimmer.snapshot_sha256 = "0".repeat(64);
            let then = catalog_headers(then);
            then.status(200).json_body_obj(&value);
        });
        let client = Client::new(server.base_url(), "k");
        assert!(matches!(
            client.models().await,
            Err(Error::InvalidModelCatalog("snapshot_mismatch"))
        ));

        let failed = MockServer::start_async().await;
        failed.mock(|when, then| {
            when.method(GET).path("/v1/models");
            then.status(503).body("private provider diagnostic");
        });
        let error = Client::new(failed.base_url(), "k")
            .models()
            .await
            .expect_err("503 must fail");
        match error {
            Error::Status { body, .. } => {
                assert_eq!(body, "model catalog request failed");
                assert!(!body.contains("provider"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn models_rejects_declared_oversize_before_json_decode() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(GET).path("/v1/models");
            let then = catalog_headers(then);
            then.status(200)
                .body("x".repeat(MAX_MODELS_RESPONSE_BYTES + 1));
        });
        assert!(matches!(
            Client::new(server.base_url(), "k").models().await,
            Err(Error::ResponseTooLarge {
                limit: MAX_MODELS_RESPONSE_BYTES
            })
        ));
    }

    #[tokio::test]
    async fn models_rejects_redirect_without_contacting_target() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(GET).path("/v1/models");
            then.status(302).header("location", "/elsewhere");
        });
        let target = server.mock(|when, then| {
            when.method(GET).path("/elsewhere");
            then.status(200).body("{}");
        });

        assert!(matches!(
            Client::new(server.base_url(), "k").models().await,
            Err(Error::UnexpectedModelCatalogRedirect)
        ));
        assert_eq!(target.calls(), 0);
    }
}
