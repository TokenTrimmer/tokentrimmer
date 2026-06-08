//! Embeddings: `Client::embed` posts to `/v1/embeddings` and returns the typed
//! response plus the gateway's cost/savings headers.

use crate::{
    parse_cost, Client, CostInfo, EmbeddingInput, EmbeddingsRequest, EmbeddingsResponse, Error,
    Result,
};

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
    /// Start building an embeddings request:
    /// `client.embeddings().model(m).input(i).dimensions(256).send()`.
    #[must_use]
    pub fn embeddings(&self) -> EmbedBuilder<'_> {
        EmbedBuilder {
            client: self,
            model: String::new(),
            input: None,
            dimensions: None,
            encoding_format: None,
            cost_limit: None,
        }
    }

    /// Embed `input` with `model` — a convenience for
    /// `embeddings().model(model).input(input).send()`.
    ///
    /// # Errors
    /// See [`EmbedBuilder::send`].
    pub async fn embed(
        &self,
        model: impl Into<String>,
        input: EmbeddingInput,
    ) -> Result<EmbedOutcome> {
        self.embeddings().model(model).input(input).send().await
    }
}

/// Fluent builder for an embeddings request. See [`Client::embeddings`].
pub struct EmbedBuilder<'a> {
    client: &'a Client,
    model: String,
    input: Option<EmbeddingInput>,
    dimensions: Option<u32>,
    encoding_format: Option<String>,
    cost_limit: Option<f64>,
}

impl EmbedBuilder<'_> {
    #[must_use]
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }
    #[must_use]
    pub fn input(mut self, input: EmbeddingInput) -> Self {
        self.input = Some(input);
        self
    }
    /// Reduce the embedding to `n` dimensions (Matryoshka models).
    #[must_use]
    pub fn dimensions(mut self, n: u32) -> Self {
        self.dimensions = Some(n);
        self
    }
    /// Wire encoding format (e.g. `"float"` or `"base64"`).
    #[must_use]
    pub fn encoding_format(mut self, format: impl Into<String>) -> Self {
        self.encoding_format = Some(format.into());
        self
    }
    /// `X-TokenTrimmer-Cost-Limit-Usd` — the gateway rejects with `402` if the
    /// estimated cost exceeds `usd`.
    #[must_use]
    pub fn cost_limit(mut self, usd: f64) -> Self {
        self.cost_limit = Some(usd);
        self
    }

    /// Send the request and return the vectors + cost.
    ///
    /// # Errors
    /// [`Error::MissingModel`] / [`Error::MissingInput`] (pre-flight),
    /// [`Error::Request`] on transport failure, [`Error::Status`] on a non-2xx
    /// response (carrying cost/trace), [`Error::Decode`] on an invalid body.
    pub async fn send(self) -> Result<EmbedOutcome> {
        if self.model.trim().is_empty() {
            return Err(Error::MissingModel);
        }
        let Some(input) = self.input else {
            return Err(Error::MissingInput);
        };
        let req = EmbeddingsRequest {
            model: self.model,
            input,
            dimensions: self.dimensions,
            encoding_format: self.encoding_format,
        };
        let http_req = self
            .client
            .http
            .post(format!("{}/v1/embeddings", self.client.base))
            .bearer_auth(&self.client.key)
            .json(&req);
        let http_req = crate::apply_tt_headers(http_req, None, self.cost_limit)?;
        let resp = http_req.send().await.map_err(Error::Request)?;
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
    use serde_json::json;

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
    async fn embeddings_builder_sends_dimensions_and_encoding_format() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .body_contains("\"dimensions\":256")
                .body_contains("\"encoding_format\":\"float\"");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(embeddings_body());
        });
        let client = Client::new(server.base_url(), "k");
        let out = client
            .embeddings()
            .model("text-embedding-3-small")
            .input(EmbeddingInput::Single("hi".into()))
            .dimensions(256)
            .encoding_format("float")
            .send()
            .await
            .unwrap();
        assert_eq!(out.vectors().count(), 2);
    }

    #[tokio::test]
    async fn embed_builder_sends_cost_limit_header() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/embeddings")
                .header("x-tokentrimmer-cost-limit-usd", "0.01");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(embeddings_body());
        });
        let client = Client::new(server.base_url(), "k");
        // The mock requires the header, so `.send()` only succeeds if it was sent.
        let out = client
            .embeddings()
            .model("text-embedding-3-small")
            .input(EmbeddingInput::Single("hi".into()))
            .cost_limit(0.01)
            .send()
            .await
            .unwrap();
        assert_eq!(out.vectors().count(), 2);
    }

    #[tokio::test]
    async fn embed_builder_missing_input_errors() {
        // dead base — no network because input is unset.
        let client = Client::new("http://127.0.0.1:1", "k");
        let result = client.embeddings().model("m").send().await;
        assert!(matches!(result, Err(Error::MissingInput)));
    }

    #[tokio::test]
    async fn embed_builder_missing_model_errors() {
        let client = Client::new("http://127.0.0.1:1", "k");
        let result = client
            .embeddings()
            .input(EmbeddingInput::Single("hi".into()))
            .send()
            .await;
        assert!(matches!(result, Err(Error::MissingModel)));
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
        let result = client
            .embed("  ", EmbeddingInput::Single("hi".into()))
            .await;
        assert!(matches!(result, Err(Error::MissingModel)));
    }
}
