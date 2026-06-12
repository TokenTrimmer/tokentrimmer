//! Auto-pause evaluator for regressing down-routes (research Phase 2.3 — the
//! "auto-pause is a deliberate follow-up" the #155 sampler promised).
//!
//! [`AutoPauseJudgeSink`] is a [`JudgeSink`] appended to the fanout AFTER the
//! persistent sink ([`crate::quality_persist::PostgresJudgeSink`]) —
//! [`crate::quality_sample::FanoutJudgeSink`] awaits sinks sequentially in
//! order, so the just-recorded verdict is already in the DB window when this
//! evaluator consults it. When a route's recent paired pass-rate (over the
//! most recent [`PAUSE_WINDOW_MAX_VERDICTS`] classified verdicts) drops
//! STRICTLY below its floor with at least `pause_min_verdicts` classified
//! verdicts, the evaluator sticky-pauses the route via
//! [`tt_routing::RoutingStore::pause_route`].
//!
//! # Invariants
//!
//! * **OFF BY DEFAULT, per-route opt-in.** Only a route with
//!   `then.auto_pause == true` can be auto-paused; the evaluator itself only
//!   exists when [`crate::AppState::with_route_auto_pause`] is called.
//! * **Fail-safe in the EXPENSIVE direction.** Paused = rewrite suppressed =
//!   requests flow to the originally-requested model (quality-safe). A pause
//!   can never cheapen traffic.
//! * **Sticky.** The pause persists (`route_pauses` row) until an explicit
//!   `POST /v1/routes/:id/resume`; a paused route stops rewriting, so it
//!   stops being judged, so its window freezes — it can never un-pause
//!   itself.
//! * **Resume restarts the evidence.** A resume RETAINS the `route_pauses`
//!   row with `resumed_at` stamped, and [`RECENT_CLASSIFIED_SQL`] bounds the
//!   window to verdicts AFTER that watermark — the frozen, mostly-degraded
//!   pre-pause window can never instantly re-pause a just-resumed route. The
//!   evaluator starts over: it needs `pause_min_verdicts` fresh post-resume
//!   classified verdicts before the floor can trigger again.
//! * **Best-effort, never user-facing.** Every error is warn-logged and
//!   swallowed; the sink runs inside the detached judge task and can never
//!   touch a user request.
//! * **No `rand` anywhere** (hard rule 3) — the only nondeterminism is the
//!   DB window itself.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tt_plan_core::JudgeVerdict;
use uuid::Uuid;

use crate::quality_sample::{JudgeOutcome, JudgeSink};

/// Default pass-rate floor: pause when fewer than 90% of the windowed
/// classified verdicts are `acceptable`. Route-overridable via
/// `RouteAction::pause_floor_pass_rate`.
pub const DEFAULT_PAUSE_FLOOR_PASS_RATE: f64 = 0.90;

/// Default minimum classified verdicts (acceptable + degraded) in-window
/// before the floor can trigger. Route-overridable via
/// `RouteAction::pause_min_verdicts`.
pub const DEFAULT_PAUSE_MIN_VERDICTS: u32 = 20;

/// Most recent classified verdicts considered (the rolling window).
pub const PAUSE_WINDOW_MAX_VERDICTS: u32 = 100;

#[derive(Debug, thiserror::Error)]
pub enum WindowError {
    #[error("backend error: {0}")]
    Backend(String),
}

/// Read-side of the verdict window the pause decision is computed over.
/// Production wires [`PgVerdictWindow`] (multi-replica-correct: every replica
/// sees the same durable `quality_verdicts` rows); tests use
/// [`InMemoryVerdictWindow`].
#[async_trait]
pub trait VerdictWindowSource: Send + Sync {
    /// `(acceptable, degraded)` over the MOST RECENT `limit` classified
    /// verdicts for `(org, route)`. Unclear/unjudged rows are excluded —
    /// consistent with `VerdictTally` / quality-preserved summaries.
    async fn recent_classified_counts(
        &self,
        org_id: Uuid,
        route_id: Uuid,
        limit: u32,
    ) -> Result<(u64, u64), WindowError>;
}

/// SQL behind [`PgVerdictWindow`]: classified-only, recency-ordered, LIMITed
/// — served by the migration-0019 `quality_verdicts_route_idx` — and bounded
/// below by the route's resume watermark (`route_pauses.resumed_at`): after a
/// resume, only POST-resume verdicts count, so the frozen pre-pause window
/// can never instantly re-pause a just-resumed route. No `route_pauses` row
/// (never paused) or an active pause (`resumed_at IS NULL`) leaves the window
/// unbounded.
pub const RECENT_CLASSIFIED_SQL: &str = r#"SELECT COUNT(*) FILTER (WHERE verdict = 'acceptable'),
       COUNT(*) FILTER (WHERE verdict = 'degraded')
FROM (SELECT verdict FROM quality_verdicts
      WHERE org_id = $1 AND route_id = $2 AND verdict IN ('acceptable','degraded')
        AND ts > COALESCE((SELECT p.resumed_at FROM route_pauses p
                           WHERE p.org_id = $1 AND p.route_id = $2),
                          '-infinity'::timestamptz)
      ORDER BY ts DESC LIMIT $3) recent"#;

/// Production window: reads the durable `quality_verdicts` table.
pub struct PgVerdictWindow {
    pool: sqlx::PgPool,
}

impl PgVerdictWindow {
    #[must_use]
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

impl std::fmt::Debug for PgVerdictWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgVerdictWindow")
            .field("pool", &"PgPool { .. }")
            .finish()
    }
}

#[async_trait]
impl VerdictWindowSource for PgVerdictWindow {
    async fn recent_classified_counts(
        &self,
        org_id: Uuid,
        route_id: Uuid,
        limit: u32,
    ) -> Result<(u64, u64), WindowError> {
        let (acceptable, degraded): (i64, i64) = sqlx::query_as(RECENT_CLASSIFIED_SQL)
            .bind(org_id)
            .bind(route_id)
            .bind(i64::from(limit))
            .fetch_one(&self.pool)
            .await
            .map_err(|e| WindowError::Backend(e.to_string()))?;
        Ok((acceptable.max(0) as u64, degraded.max(0) as u64))
    }
}

/// Test / dev window: a per-`(org, route)` deque of classified verdicts
/// (`true` = acceptable, `false` = degraded), most recent last. Also a
/// [`JudgeSink`] so an in-memory fanout can feed it exactly where the
/// production fanout feeds Postgres (route-attributed classified verdicts
/// only; unclear / route-less outcomes are ignored, mirroring
/// [`RECENT_CLASSIFIED_SQL`]). Does NOT model the resume watermark — that
/// bound lives in the production SQL (`route_pauses.resumed_at`); tests that
/// need a post-resume window use a fresh instance.
#[derive(Debug, Default)]
pub struct InMemoryVerdictWindow {
    inner: Mutex<HashMap<(Uuid, Uuid), VecDeque<bool>>>,
}

impl InMemoryVerdictWindow {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one classified verdict (`acceptable == true`) for `(org, route)`.
    pub fn push(&self, org_id: Uuid, route_id: Uuid, acceptable: bool) {
        let mut g = self.inner.lock().expect("verdict window poisoned");
        g.entry((org_id, route_id))
            .or_default()
            .push_back(acceptable);
    }
}

#[async_trait]
impl VerdictWindowSource for InMemoryVerdictWindow {
    async fn recent_classified_counts(
        &self,
        org_id: Uuid,
        route_id: Uuid,
        limit: u32,
    ) -> Result<(u64, u64), WindowError> {
        let g = self.inner.lock().expect("verdict window poisoned");
        let Some(window) = g.get(&(org_id, route_id)) else {
            return Ok((0, 0));
        };
        let recent = window.iter().rev().take(limit as usize);
        let mut acceptable = 0u64;
        let mut degraded = 0u64;
        for &is_acceptable in recent {
            if is_acceptable {
                acceptable += 1;
            } else {
                degraded += 1;
            }
        }
        Ok((acceptable, degraded))
    }
}

#[async_trait]
impl JudgeSink for InMemoryVerdictWindow {
    async fn record(&self, outcome: JudgeOutcome) {
        let Some(route_id) = outcome.route_id else {
            return;
        };
        match outcome.score.verdict {
            JudgeVerdict::Acceptable => self.push(outcome.org_id, route_id, true),
            JudgeVerdict::Degraded => self.push(outcome.org_id, route_id, false),
            JudgeVerdict::Unclear => {} // unclassified — excluded from the window
        }
    }
}

/// Pure pause decision: `Some(pass_rate)` when the windowed classified counts
/// say "pause" — at least `min_verdicts` classified verdicts AND a pass rate
/// STRICTLY below `floor`. `None` otherwise (insufficient n, or at/above the
/// floor). Exactly-at-floor does NOT trigger.
#[must_use]
pub fn should_pause(acceptable: u64, degraded: u64, min_verdicts: u32, floor: f64) -> Option<f64> {
    let n = acceptable + degraded;
    if n < u64::from(min_verdicts) || n == 0 {
        return None;
    }
    let pass_rate = acceptable as f64 / n as f64;
    if pass_rate < floor {
        Some(pass_rate)
    } else {
        None
    }
}

/// JudgeSink that AUTO-PAUSES a regressing down-route. Appended to the
/// fanout AFTER the persistent sink (FanoutJudgeSink awaits in order) so the
/// just-recorded verdict is included in the DB window. Best-effort: every
/// error is warn-logged; it can never touch a user request (it runs inside
/// the detached judge task).
pub struct AutoPauseJudgeSink {
    routing_store: Arc<tt_routing::CachingRoutingStore>,
    window: Arc<dyn VerdictWindowSource>,
}

impl AutoPauseJudgeSink {
    #[must_use]
    pub fn new(
        routing_store: Arc<tt_routing::CachingRoutingStore>,
        window: Arc<dyn VerdictWindowSource>,
    ) -> Self {
        Self {
            routing_store,
            window,
        }
    }
}

#[async_trait]
impl JudgeSink for AutoPauseJudgeSink {
    async fn record(&self, outcome: JudgeOutcome) {
        use tt_routing::{NewRoutePause, PausedBy, RoutingStore};

        // (1) L2-hit verdicts don't attribute to a route — nothing to pause.
        let Some(route_id) = outcome.route_id else {
            return;
        };
        // (2) Unclear (incl. unjudged spend rows) leaves the classified counts
        // unchanged — the decision cannot have moved.
        if outcome.score.verdict == JudgeVerdict::Unclear {
            return;
        }
        // (3) Per-route opt-in (OFF BY DEFAULT); an absent or already-paused
        // route short-circuits.
        let route = match self.routing_store.get_route(outcome.org_id, route_id).await {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(error = %e, route_id = %route_id, "auto-pause: route lookup failed (best-effort, skipping)");
                return;
            }
        };
        if route.paused || !route.then.auto_pause {
            return;
        }
        // (4)–(5) Windowed decision.
        let (acceptable, degraded) = match self
            .window
            .recent_classified_counts(outcome.org_id, route_id, PAUSE_WINDOW_MAX_VERDICTS)
            .await
        {
            Ok(counts) => counts,
            Err(e) => {
                tracing::warn!(error = %e, route_id = %route_id, "auto-pause: verdict window query failed (best-effort, skipping)");
                return;
            }
        };
        let min_verdicts = route
            .then
            .pause_min_verdicts
            .unwrap_or(DEFAULT_PAUSE_MIN_VERDICTS);
        let floor = route
            .then
            .pause_floor_pass_rate
            .unwrap_or(DEFAULT_PAUSE_FLOOR_PASS_RATE);
        let Some(pass_rate) = should_pause(acceptable, degraded, min_verdicts, floor) else {
            return;
        };
        // (6) Sticky pause. Ok(false) = another replica/verdict won the race —
        // the route is paused either way, nothing more to do.
        let n = acceptable + degraded;
        let pause = NewRoutePause {
            paused_by: PausedBy::Auto,
            reason: format!(
                "auto: paired pass-rate {:.1}% < floor {:.1}% (n={n})",
                pass_rate * 100.0,
                floor * 100.0
            ),
            pass_rate: Some(pass_rate),
            verdicts_in_window: Some(i32::try_from(n).unwrap_or(i32::MAX)),
        };
        match self
            .routing_store
            .pause_route(outcome.org_id, route_id, pause)
            .await
        {
            Ok(true) => {
                tracing::warn!(
                    org_id = %outcome.org_id,
                    route_id = %route_id,
                    route = %route.name,
                    pass_rate = pass_rate,
                    floor = floor,
                    n = n,
                    "route_auto_paused: paired pass-rate below floor — rewrite suppressed (requests flow to the original model) until POST /v1/routes/:id/resume"
                );
                crate::metrics::record_route_auto_paused(&route.name);
                // CachingRoutingStore already invalidated the org's engine.
            }
            Ok(false) => {} // already paused (raced) — sticky either way
            Err(e) => {
                tracing::warn!(error = %e, route_id = %route_id, "auto-pause: pause_route failed (best-effort; will retry on the next verdict)");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_plan_core::{RiskBand, SampleScore};
    use tt_routing::{
        CachingRoutingStore, InMemoryRoutingStore, NewRoutePause, PausedBy, Route, RouteAction,
        RouteConditions, RoutingStore, RoutingStoreError,
    };

    /// (a=15, d=5, min=20, floor=0.90) → Some(0.75); n below min → None;
    /// pass rate exactly AT the floor → None (strictly-below triggers).
    #[test]
    fn should_pause_math() {
        assert_eq!(should_pause(15, 5, 20, 0.90), Some(0.75));
        assert_eq!(should_pause(15, 4, 20, 0.90), None, "n=19 < min=20");
        // Exactly at the floor: 18/20 = 0.90 — NOT strictly below.
        assert_eq!(should_pause(18, 2, 20, 0.90), None);
        // Just below the floor triggers.
        assert!(should_pause(17, 3, 20, 0.90).is_some());
        // No classified verdicts → never (even with min 0).
        assert_eq!(should_pause(0, 0, 0, 0.90), None);
    }

    /// The in-memory window honors recency + limit and excludes unclear (the
    /// JudgeSink impl never records Unclear).
    #[tokio::test]
    async fn in_memory_window_recency_and_limit() {
        let w = InMemoryVerdictWindow::new();
        let org = Uuid::now_v7();
        let route = Uuid::now_v7();
        // 120 old degraded, then 100 recent acceptable.
        for _ in 0..120 {
            w.push(org, route, false);
        }
        for _ in 0..100 {
            w.push(org, route, true);
        }
        assert_eq!(
            w.recent_classified_counts(org, route, 100).await.unwrap(),
            (100, 0),
            "limit must take the MOST RECENT verdicts"
        );
        assert_eq!(
            w.recent_classified_counts(org, route, 150).await.unwrap(),
            (100, 50)
        );
        // Unknown (org, route) → empty, not an error.
        assert_eq!(
            w.recent_classified_counts(Uuid::now_v7(), route, 100)
                .await
                .unwrap(),
            (0, 0)
        );
        // The sink impl ignores Unclear and route-less outcomes.
        w.record(outcome(org, Some(route), JudgeVerdict::Unclear))
            .await;
        w.record(outcome(org, None, JudgeVerdict::Degraded)).await;
        assert_eq!(
            w.recent_classified_counts(org, route, 250).await.unwrap(),
            (100, 120),
            "unclear/route-less outcomes must not enter the window"
        );
    }

    /// Recording wrapper store: delegates to an InMemoryRoutingStore and
    /// records every pause_route call.
    #[derive(Debug)]
    struct RecordingStore {
        inner: InMemoryRoutingStore,
        pause_calls: Mutex<Vec<(Uuid, Uuid, NewRoutePause)>>,
    }

    impl RecordingStore {
        fn new() -> Self {
            Self {
                inner: InMemoryRoutingStore::new(),
                pause_calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl RoutingStore for RecordingStore {
        async fn list_for_org(&self, org_id: Uuid) -> Result<Vec<Route>, RoutingStoreError> {
            self.inner.list_for_org(org_id).await
        }
        async fn list_all_for_org(&self, org_id: Uuid) -> Result<Vec<Route>, RoutingStoreError> {
            self.inner.list_all_for_org(org_id).await
        }
        async fn get_route(
            &self,
            org_id: Uuid,
            id: Uuid,
        ) -> Result<Option<Route>, RoutingStoreError> {
            self.inner.get_route(org_id, id).await
        }
        async fn pause_route(
            &self,
            org_id: Uuid,
            route_id: Uuid,
            pause: NewRoutePause,
        ) -> Result<bool, RoutingStoreError> {
            self.pause_calls
                .lock()
                .unwrap()
                .push((org_id, route_id, pause.clone()));
            self.inner.pause_route(org_id, route_id, pause).await
        }
        async fn resume_route(
            &self,
            org_id: Uuid,
            route_id: Uuid,
        ) -> Result<bool, RoutingStoreError> {
            self.inner.resume_route(org_id, route_id).await
        }
    }

    fn route(id: Uuid, auto_pause: bool, min_verdicts: Option<u32>) -> Route {
        Route {
            id,
            name: "downgrade".into(),
            priority: 10,
            enabled: true,
            when: RouteConditions::default(),
            then: RouteAction {
                target_model: "gpt-4o-mini".into(),
                auto_pause,
                pause_min_verdicts: min_verdicts,
                ..Default::default()
            },
            paused: false,
        }
    }

    fn outcome(org: Uuid, route_id: Option<Uuid>, verdict: JudgeVerdict) -> JudgeOutcome {
        JudgeOutcome {
            cache_entry_id: None,
            hit_similarity: None,
            org_id: org,
            route_id,
            requested_model: "gpt-4o".into(),
            served_model: "gpt-4o-mini".into(),
            score: SampleScore {
                request_id: Uuid::now_v7(),
                verdict,
                reason: "test".into(),
            },
            risk_band: RiskBand::Low,
            judge_model: "judge-cheap".into(),
            judge_cost_usd: Some(0.0001),
            baseline_cost_usd: Some(0.001),
            baseline_dispatched: true,
            optimized_position: crate::quality_sample::AbOrder::OptimizedA,
            orders_judged: 1,
            orders_agreed: None,
        }
    }

    struct Harness {
        store: Arc<RecordingStore>,
        window: Arc<InMemoryVerdictWindow>,
        sink: AutoPauseJudgeSink,
        org: Uuid,
    }

    fn harness(planted: Route) -> Harness {
        let store = Arc::new(RecordingStore::new());
        let org = Uuid::now_v7();
        store.inner.set_routes(org, vec![planted]);
        let window = Arc::new(InMemoryVerdictWindow::new());
        let caching = Arc::new(CachingRoutingStore::new(
            store.clone() as Arc<dyn RoutingStore>
        ));
        let sink = AutoPauseJudgeSink::new(caching, window.clone() as Arc<dyn VerdictWindowSource>);
        Harness {
            store,
            window,
            sink,
            org,
        }
    }

    /// auto_pause=false routes never call pause_route, even at 0% pass-rate.
    #[tokio::test]
    async fn sink_requires_opt_in() {
        let route_id = Uuid::now_v7();
        let h = harness(route(route_id, false, Some(1)));
        for _ in 0..30 {
            h.window.push(h.org, route_id, false); // 0% pass rate
        }
        h.sink
            .record(outcome(h.org, Some(route_id), JudgeVerdict::Degraded))
            .await;
        assert!(
            h.store.pause_calls.lock().unwrap().is_empty(),
            "auto_pause=false must NEVER pause (off-by-default opt-in)"
        );
    }

    /// Below-floor at n >= min pauses EXACTLY once, with Auto/evidence/reason;
    /// an already-paused route short-circuits (no second call).
    #[tokio::test]
    async fn sink_pauses_once_with_reason() {
        let route_id = Uuid::now_v7();
        let h = harness(route(route_id, true, Some(20)));
        // 15 acceptable + 5 degraded = 75% < 90% floor at n=20.
        for i in 0..20 {
            h.window.push(h.org, route_id, i < 15);
        }
        h.sink
            .record(outcome(h.org, Some(route_id), JudgeVerdict::Degraded))
            .await;
        {
            let calls = h.store.pause_calls.lock().unwrap();
            assert_eq!(calls.len(), 1, "exactly one pause call");
            let (org, id, pause) = &calls[0];
            assert_eq!(*org, h.org);
            assert_eq!(*id, route_id);
            assert_eq!(pause.paused_by, PausedBy::Auto);
            assert_eq!(pause.verdicts_in_window, Some(20));
            assert!((pause.pass_rate.unwrap() - 0.75).abs() < 1e-12);
            assert!(
                pause.reason.starts_with("auto:"),
                "reason must carry the auto: prefix — got {:?}",
                pause.reason
            );
        }
        // A further verdict on the now-paused route short-circuits (route.paused).
        h.sink
            .record(outcome(h.org, Some(route_id), JudgeVerdict::Degraded))
            .await;
        assert_eq!(
            h.store.pause_calls.lock().unwrap().len(),
            1,
            "already-paused route must not be re-paused"
        );
    }

    /// n below the route's min_verdicts → no pause even far below the floor.
    #[tokio::test]
    async fn sink_respects_min_verdicts() {
        let route_id = Uuid::now_v7();
        let h = harness(route(route_id, true, Some(20)));
        for _ in 0..19 {
            h.window.push(h.org, route_id, false); // n=19 < 20
        }
        h.sink
            .record(outcome(h.org, Some(route_id), JudgeVerdict::Degraded))
            .await;
        assert!(h.store.pause_calls.lock().unwrap().is_empty());
    }

    /// Unclear verdicts and route-less (L2-hit) outcomes return without
    /// touching the store or the window.
    #[tokio::test]
    async fn unclear_and_routeless_verdicts_ignored() {
        let route_id = Uuid::now_v7();
        let h = harness(route(route_id, true, Some(1)));
        for _ in 0..30 {
            h.window.push(h.org, route_id, false); // would pause if consulted
        }
        h.sink
            .record(outcome(h.org, Some(route_id), JudgeVerdict::Unclear))
            .await;
        h.sink
            .record(outcome(h.org, None, JudgeVerdict::Degraded))
            .await;
        assert!(
            h.store.pause_calls.lock().unwrap().is_empty(),
            "unclear / route-less outcomes must be ignored"
        );
    }
}
