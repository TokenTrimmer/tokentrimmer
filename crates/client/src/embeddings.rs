//! Embeddings: `Client::embed` posts to `/v1/embeddings` and returns the typed
//! response plus the gateway's cost/savings headers.

use serde_json::json;

use crate::{parse_cost, Client, CostInfo, EmbeddingInput, EmbeddingsResponse, Error, Result};

/// A completed embeddings call: the typed response plus parsed cost/savings.
#[derive(Debug, Clone)]
pub struct EmbedOutcome {
    pub response: EmbeddingsResponse,
    pub cost: CostInfo,
}

impl EmbedOutcome {
    /// The embedding rows, in returned order.
    pub fn vectors(&self) -> impl Iterator<Item = &[f32]> {
        self.response.data.iter().map(|d| d.embedding.as_slice())
    }
}

impl Client {
    /// Embed `input` with `model`. Returns the vectors + cost.
    ///
    /// # Errors
    /// [`Error::MissingModel`] if `model` is empty, [`Error::Request`] on
    /// transport failure, [`Error::Status`] on a non-2xx response (carrying the
    /// cost/trace telemetry), [`Error::Decode`] if the body isn't a valid
    /// embeddings response.
    pub async fn embed(
        &self,
        model: impl Into<String>,
        input: EmbeddingInput,
    ) -> Result<EmbedOutcome> {
        let model = model.into();
        if model.trim().is_empty() {
            return Err(Error::MissingModel);
        }
        let body = json!({ "model": model, "input": input });
        let resp = self
            .http
            .post(format!("{}/v1/embeddings", self.base))
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await
            .map_err(Error::Request)?;
        let cost = parse_cost(resp.headers());
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Status {
                status: status.as_u16(),
                body,
                cost: Box::new(cost),
            });
        }
        let response = resp
            .json::<EmbeddingsResponse>()
            .await
            .map_err(Error::Decode)?;
        Ok(EmbedOutcome { response, cost })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Client;
    use httpmock::prelude::*;

    fn embeddings_body() -> serde_json::Value {
        json!({
            "object": "list",
            "data": [
                { "object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3] },
                { "object": "embedding", "index": 1, "embedding": [0.4, 0.5, 0.6] }
            ],
            "model": "text-embedding-3-small",
            "usage": { "prompt_tokens": 8, "completion_tokens": 0, "total_tokens": 8 }
        })
    }

    #[tokio::test]
    async fn embed_returns_vectors_and_cost() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .body_contains("text-embedding-3-small");
            then.status(200)
                .header("content-type", "application/json")
                .header("x-tokentrimmer-cost-usd", "0.0002")
                .header("x-tokentrimmer-model-used", "text-embedding-3-small")
                .json_body(embeddings_body());
        });

        let client = Client::new(server.base_url(), "k");
        let out = client
            .embed(
                "text-embedding-3-small",
                EmbeddingInput::Batch(vec!["a".into(), "b".into()]),
            )
            .await
            .unwrap();

        let rows: Vec<&[f32]> = out.vectors().collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], &[0.1, 0.2, 0.3]);
        assert_eq!(out.cost.cost_usd, Some(0.0002));
        assert_eq!(
            out.cost.model_used.as_deref(),
            Some("text-embedding-3-small")
        );
    }

    #[tokio::test]
    async fn embed_surfaces_status_error() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(501).body("not implemented");
        });
        let client = Client::new(server.base_url(), "k");
        let result = client.embed("m", EmbeddingInput::Single("hi".into())).await;
        assert!(matches!(result, Err(Error::Status { status: 501, .. })));
    }

    #[tokio::test]
    async fn embed_without_model_errors_before_any_request() {
        // dead base — no network is touched because the model is empty.
        let client = Client::new("http://127.0.0.1:1", "k");
        let result = client.embed("  ", EmbeddingInput::Single("hi".into())).await;
        assert!(matches!(result, Err(Error::MissingModel)));
    }
}
