//! Typed client controls for short-lived agent transcript export and erasure.

use crate::{Client, Error, Result, Run};

impl Client {
    /// Export a still-retained paused/resumed agent transcript. The gateway
    /// returns `404` once the one-hour record expires or is deleted; durable
    /// run metadata remains available through the ordinary run history API.
    pub async fn export_run_transcript(&self, run_id: &str) -> Result<Run> {
        let url = format!("{}/v1/agent/runs/{run_id}/transcript", self.base);
        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.key)
            .send()
            .await
            .map_err(Error::Request)?;
        let cost = crate::parse_cost(resp.headers());
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Status {
                status: status.as_u16(),
                body,
                cost: Box::new(cost),
            });
        }
        resp.json::<Run>().await.map_err(Error::Decode)
    }

    /// Idempotently erase a still-retained paused/resumed transcript. Durable
    /// billing/audit run metadata is intentionally unaffected.
    pub async fn delete_run_transcript(&self, run_id: &str) -> Result<()> {
        let url = format!("{}/v1/agent/runs/{run_id}/transcript", self.base);
        let resp = self
            .http
            .delete(url)
            .bearer_auth(&self.key)
            .send()
            .await
            .map_err(Error::Request)?;
        let cost = crate::parse_cost(resp.headers());
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Status {
                status: status.as_u16(),
                body,
                cost: Box::new(cost),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use httpmock::prelude::*;
    use serde_json::json;

    use super::*;
    use crate::RunStatus;

    #[tokio::test]
    async fn export_and_delete_use_exact_scoped_routes() {
        let server = MockServer::start_async().await;
        let run_id = "00000000-0000-0000-0000-000000000001";
        let export = server.mock(|when, then| {
            when.method(GET)
                .path(format!("/v1/agent/runs/{run_id}/transcript"))
                .header("authorization", "Bearer k");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(json!({
                    "id": run_id,
                    "status": "requires_action",
                    "messages": [],
                    "turns": 1,
                    "usage": {"prompt_tokens": 2, "completion_tokens": 3}
                }));
        });
        let delete = server.mock(|when, then| {
            when.method(DELETE)
                .path(format!("/v1/agent/runs/{run_id}/transcript"))
                .header("authorization", "Bearer k");
            then.status(204);
        });

        let client = Client::new(server.base_url(), "k");
        let run = client.export_run_transcript(run_id).await.unwrap();
        assert_eq!(run.id, run_id);
        assert_eq!(run.status, RunStatus::RequiresAction);
        client.delete_run_transcript(run_id).await.unwrap();
        export.assert();
        delete.assert();
    }

    #[tokio::test]
    async fn export_surfaces_expiry_as_status_error() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(GET).path("/v1/agent/runs/missing/transcript");
            then.status(404).body("not retained");
        });
        let client = Client::new(server.base_url(), "k");
        let result = client.export_run_transcript("missing").await;
        assert!(matches!(result, Err(Error::Status { status: 404, .. })));
    }
}
