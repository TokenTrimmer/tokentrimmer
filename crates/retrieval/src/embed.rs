//! OpenAI text-embedding-3-small client. 1536-dim output.
//!
//! For tests, the `base_url` is overridable.

use serde::Deserialize;

use crate::error::RetrievalError;

#[derive(Debug, Clone)]
pub struct EmbeddingClient {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub http: reqwest::Client,
}

impl EmbeddingClient {
    pub fn openai(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: "https://api.openai.com".into(),
            model: "text-embedding-3-small".into(),
            http: reqwest::Client::new(),
        }
    }

    pub async fn embed(&self, text: &str) -> Result<Vec<f32>, RetrievalError> {
        #[derive(serde::Serialize)]
        struct Req<'a> {
            input: &'a str,
            model: &'a str,
        }
        let body = Req {
            input: text,
            model: &self.model,
        };
        let resp = self
            .http
            .post(format!("{}/v1/embeddings", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| RetrievalError::Embedding(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(RetrievalError::Embedding(format!(
                "HTTP {}",
                resp.status()
            )));
        }
        #[derive(Deserialize)]
        struct R {
            data: Vec<E>,
        }
        #[derive(Deserialize)]
        struct E {
            embedding: Vec<f32>,
        }
        let parsed: R = resp
            .json()
            .await
            .map_err(|e| RetrievalError::Embedding(e.to_string()))?;
        parsed
            .data
            .into_iter()
            .next()
            .map(|e| e.embedding)
            .ok_or_else(|| RetrievalError::Embedding("empty data".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    #[tokio::test]
    async fn embed_round_trip() {
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/v1/embeddings");
                then.status(200).json_body(serde_json::json!({
                    "data": [{ "embedding": [0.1, 0.2, 0.3] }]
                }));
            })
            .await;
        let c = EmbeddingClient {
            api_key: "k".into(),
            base_url: server.base_url(),
            model: "text-embedding-3-small".into(),
            http: reqwest::Client::new(),
        };
        let v = c.embed("hi").await.unwrap();
        assert_eq!(v, vec![0.1, 0.2, 0.3]);
    }

    #[tokio::test]
    async fn embed_5xx_errors() {
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/v1/embeddings");
                then.status(500).body("boom");
            })
            .await;
        let c = EmbeddingClient {
            api_key: "k".into(),
            base_url: server.base_url(),
            model: "text-embedding-3-small".into(),
            http: reqwest::Client::new(),
        };
        let err = c.embed("hi").await.unwrap_err();
        assert!(matches!(err, RetrievalError::Embedding(_)));
    }
}
