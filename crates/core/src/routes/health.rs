//! `GET /health` — liveness probe. Returns 200 if the process is running.
//! Real readiness (provider reachability, DB connectivity) goes to `/ready` later.

use axum::Json;
use serde::Serialize;

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
}

pub async fn handler() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}
