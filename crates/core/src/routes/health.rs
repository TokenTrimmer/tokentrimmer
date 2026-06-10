//! `GET /health` — liveness probe. Returns 200 if the process is running.
//! Real readiness (provider reachability, DB connectivity) goes to `/ready` later.

use axum::Json;
use serde::Serialize;

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
    /// Git SHA the binary was built from, injected at compile time via the
    /// `TT_GIT_SHA` env var (Docker `--build-arg TT_GIT_SHA=...`). Reads
    /// "unknown" when it was absent or empty at compile time. The deployed-SHA
    /// drift check reads this exact field name; do not rename.
    pub git_sha: &'static str,
}

pub async fn handler() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        git_sha: option_env!("TT_GIT_SHA")
            .filter(|sha| !sha.is_empty())
            .unwrap_or("unknown"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn health_includes_git_sha() {
        let Json(body) = handler().await;
        let value = serde_json::to_value(&body).expect("serializes to JSON");
        let git_sha = value
            .get("git_sha")
            .and_then(|v| v.as_str())
            .expect("git_sha field present");
        // The SHA is baked in at compile time via `TT_GIT_SHA`; when it was
        // absent (or empty) at compile time the field must read "unknown".
        match option_env!("TT_GIT_SHA") {
            Some(sha) if !sha.is_empty() => assert_eq!(git_sha, sha),
            _ => assert_eq!(git_sha, "unknown"),
        }
    }
}
