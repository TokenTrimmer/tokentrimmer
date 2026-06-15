//! `GET /ready` — readiness probe. Distinct from `/health` (liveness): it
//! reports whether the gateway can actually serve, by checking the hard
//! dependencies that were *configured at boot*.
//!
//! Semantics (deliberate, given the intentional DB-less degraded mode):
//!
//! * **Postgres = HARD.** When a DB pool was wired at boot
//!   ([`AppState::with_db_pool`](crate::AppState::with_db_pool)) the probe runs
//!   `SELECT 1`; a failure answers **503** so an orchestrator (Fly) pulls the
//!   process out of rotation rather than routing traffic to a gateway that
//!   can't read routes/auth/telemetry. When no DB was configured (degraded
//!   mode) Postgres is reported `not_configured` and never gates the result.
//!
//! * **Redis/L1 = SOFT.** The L1 cache is optional and `degrades cleanly`
//!   throughout the codebase: auth, routing, budget and dispatch all work
//!   without it. A gateway whose *cache* is down can still serve, so a dead
//!   L1 is reported (`down`) for observability but does NOT by itself force a
//!   503 — removing a serving process from rotation because its cache blinked
//!   would be wrong. Flip `redis` into the gating set here if that tradeoff
//!   ever changes.
//!
//! `/health` stays liveness-only (is the process up?); `/ready` answers "should
//! traffic come here?".

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;

use crate::AppState;

/// Status of one dependency the readiness probe inspects.
#[derive(Serialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum DepStatus {
    /// The dependency was configured and responded to the probe.
    Ok,
    /// The dependency was configured but the probe failed.
    Down,
    /// The dependency was not wired at boot — not a dependency for this
    /// process (e.g. the intentional DB-less degraded mode).
    NotConfigured,
}

#[derive(Serialize)]
pub struct ReadyResponse {
    /// `"ready"` when every configured HARD dependency is up, else
    /// `"not_ready"`.
    pub status: &'static str,
    /// Per-dependency probe outcomes.
    pub checks: ReadyChecks,
}

#[derive(Serialize)]
pub struct ReadyChecks {
    /// Postgres `SELECT 1` outcome. HARD when configured.
    pub postgres: DepStatus,
    /// L1/Redis sentinel-read outcome. SOFT — reported, never gates the 503.
    pub redis: DepStatus,
}

/// Probe Postgres (hard) and Redis/L1 (soft). Returns 503 iff a HARD
/// dependency that was configured at boot is down, else 200.
pub async fn handler(State(state): State<AppState>) -> (StatusCode, Json<ReadyResponse>) {
    // Postgres (HARD when configured): a cheap `SELECT 1` round-trips the pool.
    let postgres = match state.db_pool.as_ref() {
        Some(pool) => match sqlx::query("SELECT 1").execute(pool).await {
            Ok(_) => DepStatus::Ok,
            Err(err) => {
                tracing::warn!(error = %err, "readiness: postgres SELECT 1 failed");
                DepStatus::Down
            }
        },
        None => DepStatus::NotConfigured,
    };

    // Redis/L1 (SOFT): a sentinel read confirms reachability. We never insert,
    // so the probe key need not exist — `Ok(None)` is a healthy miss.
    let redis = match state.l1.as_ref() {
        Some(l1) => match l1.cache.get("tt:ready:probe").await {
            Ok(_) => DepStatus::Ok,
            Err(err) => {
                tracing::warn!(error = %err, "readiness: redis/L1 probe failed (soft)");
                DepStatus::Down
            }
        },
        None => DepStatus::NotConfigured,
    };

    // Only a configured-and-down HARD dependency gates readiness.
    let hard_down = postgres == DepStatus::Down;
    let code = if hard_down {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    let status = if hard_down { "not_ready" } else { "ready" };

    (
        code,
        Json(ReadyResponse {
            status,
            checks: ReadyChecks { postgres, redis },
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ProviderRegistry;

    /// Degraded mode: no DB and no L1 wired → both `not_configured`, 200 ready.
    #[tokio::test]
    async fn ready_when_no_hard_deps_configured() {
        let state = AppState::new(ProviderRegistry::new());
        let (code, Json(body)) = handler(State(state)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body.status, "ready");
        assert_eq!(body.checks.postgres, DepStatus::NotConfigured);
        assert_eq!(body.checks.redis, DepStatus::NotConfigured);
    }

    /// A configured-but-unreachable pool makes the HARD Postgres check fail →
    /// 503 not_ready. Uses `connect_lazy` to an unroutable address with a tiny
    /// acquire timeout so `SELECT 1` fails fast without needing a live DB.
    #[tokio::test]
    async fn not_ready_when_configured_db_is_down() {
        // TEST-NET-1 (RFC 5737) address: routable nowhere, connection refused/timeout.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(200))
            .connect_lazy("postgres://user:pass@192.0.2.1:5432/db")
            .expect("connect_lazy builds without connecting");
        let state = AppState::new(ProviderRegistry::new()).with_db_pool(pool);

        let (code, Json(body)) = handler(State(state)).await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.status, "not_ready");
        assert_eq!(body.checks.postgres, DepStatus::Down);
        // Redis stays not_configured and never affects the verdict.
        assert_eq!(body.checks.redis, DepStatus::NotConfigured);
    }
}
