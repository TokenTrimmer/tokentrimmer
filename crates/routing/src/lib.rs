//! Routing engine — match incoming requests against per-org rules to pick a
//! target model (or pass through unchanged).
//!
//! Mirrors the shape used by `tt-plan-core`'s replay-time matcher so a Plan
//! projection and the live Gateway agree on which route would fire for a
//! given request. Differences from plan-core:
//!
//! - Input is the canonical [`ChatCompletionRequest`] + [`RequestContext`]
//!   (live runtime), not a historical `RequestLog`.
//! - Token-count conditions use `input_tokens` estimated from the request
//!   (caller supplies; the engine never tokenizes itself — that's a hot-path
//!   responsibility owned by the caller's tokenizer cache).
//!
//! Rules are stored sorted descending by priority. First match wins.

pub mod cache;
pub mod store;

pub use cache::CachingRoutingStore;
#[cfg(feature = "postgres")]
pub use store::PostgresRoutingStore;
pub use store::{InMemoryRoutingStore, RoutingStore, RoutingStoreError};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use tt_shared::{ChatCompletionRequest, RequestContext};

/// A single routing rule. When [`Route::when`] matches the request, the
/// caller rewrites `request.model` to [`Route::then::target_model`] (and may
/// observe the [`Route::id`] for telemetry attribution).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    /// Stable id — used in `request_logs.matched_route_id` for attribution.
    pub id: Uuid,
    /// Human-readable name, surfaced in dashboards.
    pub name: String,
    /// Higher value wins on tie-breaker; engine evaluates descending.
    pub priority: u32,
    /// Disabled routes never match.
    pub enabled: bool,
    /// AND-ed match conditions. Empty / `None` fields match anything.
    pub when: RouteConditions,
    /// What to do when matched.
    pub then: RouteAction,
}

/// Match conditions for a [`Route`]. v1 supports four predicates — extend
/// alongside `tt_plan_core::types::RouteConditions` so Plan and Gateway stay
/// in lockstep.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RouteConditions {
    /// Match only if `req.model` is in this list. Empty list matches any model.
    #[serde(default)]
    pub model_in: Vec<String>,
    /// Match only if estimated `input_tokens < this`.
    #[serde(default)]
    pub input_tokens_lt: Option<u32>,
    /// Match only if estimated `input_tokens > this`.
    #[serde(default)]
    pub input_tokens_gt: Option<u32>,
    /// Match only if `ctx.tag == Some(this)`.
    #[serde(default)]
    pub tag_equals: Option<String>,
}

/// What a matching [`Route`] does to the request before dispatch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteAction {
    /// Rewrite to this model on the same provider as the request (v1 is
    /// same-provider only — see ADR-007 / Plan design for the cross-provider
    /// constraint).
    pub target_model: String,
}

/// Rule engine. Hold routes sorted by descending priority; iterate to find
/// the first match.
#[derive(Debug, Clone, Default)]
pub struct RoutingEngine {
    routes: Vec<Route>,
}

impl RoutingEngine {
    /// Construct an empty engine. Use [`RoutingEngine::with_routes`] for the
    /// common case of building from a stored config.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct from a collection of routes. Internally sorted descending by
    /// priority — the order the caller passes them in does not matter.
    pub fn with_routes(routes: impl IntoIterator<Item = Route>) -> Self {
        let mut v: Vec<Route> = routes.into_iter().collect();
        v.sort_by_key(|r| std::cmp::Reverse(r.priority));
        Self { routes: v }
    }

    /// Add a route in-place and re-sort. Hot-path callers should prefer
    /// [`RoutingEngine::with_routes`] to amortize the sort.
    pub fn add(&mut self, route: Route) {
        self.routes.push(route);
        self.routes.sort_by_key(|r| std::cmp::Reverse(r.priority));
    }

    /// All routes, descending priority order.
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }

    /// Find the first matching route for `(req, ctx)`. Returns `None` when no
    /// enabled route matches — caller dispatches the request unchanged.
    ///
    /// `input_tokens_estimate` is supplied by the caller — typically a cheap
    /// length-over-4 heuristic, or the result of a tokenizer call cached at
    /// the request boundary. The engine never tokenizes itself.
    pub fn evaluate(
        &self,
        req: &ChatCompletionRequest,
        ctx: &RequestContext,
        input_tokens_estimate: u32,
    ) -> Option<&Route> {
        self.routes
            .iter()
            .find(|r| r.enabled && matches(r, req, ctx, input_tokens_estimate))
    }
}

fn matches(
    r: &Route,
    req: &ChatCompletionRequest,
    ctx: &RequestContext,
    input_tokens: u32,
) -> bool {
    let c = &r.when;
    if !c.model_in.is_empty() && !c.model_in.iter().any(|m| m == &req.model) {
        return false;
    }
    if let Some(t) = c.input_tokens_lt {
        if input_tokens >= t {
            return false;
        }
    }
    if let Some(t) = c.input_tokens_gt {
        if input_tokens <= t {
            return false;
        }
    }
    if let Some(tag) = &c.tag_equals {
        if ctx.tag.as_deref() != Some(tag.as_str()) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_shared::{
        context::{ProviderCredentials, SecretString},
        ChatCompletionRequest, Message, MessageContent,
    };

    fn make_route(name: &str, priority: u32, model_in: Vec<&str>, target: &str) -> Route {
        Route {
            id: Uuid::now_v7(),
            name: name.into(),
            priority,
            enabled: true,
            when: RouteConditions {
                model_in: model_in.into_iter().map(String::from).collect(),
                ..Default::default()
            },
            then: RouteAction {
                target_model: target.into(),
            },
        }
    }

    fn make_req(model: &str) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.into(),
            messages: vec![Message::User {
                content: MessageContent::Text("hi".into()),
                name: None,
            }],
            ..serde_json::from_str(r#"{"model":"placeholder","messages":[]}"#).unwrap()
        }
    }

    fn make_ctx(tag: Option<&str>) -> RequestContext {
        RequestContext {
            trace_id: Uuid::now_v7(),
            org_id: Uuid::nil(),
            api_key_id: Uuid::nil(),
            credentials: ProviderCredentials {
                api_key: SecretString::new(""),
                base_url: None,
                extra_headers: Vec::new(),
            },
            tag: tag.map(String::from),
            deadline: None,
        }
    }

    #[test]
    fn empty_engine_matches_nothing() {
        let eng = RoutingEngine::new();
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .is_none());
    }

    #[test]
    fn model_in_matches() {
        let eng = RoutingEngine::with_routes(vec![make_route(
            "to-mini",
            10,
            vec!["gpt-4o"],
            "gpt-4o-mini",
        )]);
        let m = eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .expect("should match");
        assert_eq!(m.then.target_model, "gpt-4o-mini");
    }

    #[test]
    fn priority_descending_first_match_wins() {
        let eng = RoutingEngine::with_routes(vec![
            make_route("low", 1, vec!["gpt-4o"], "low-target"),
            make_route("high", 100, vec!["gpt-4o"], "high-target"),
            make_route("mid", 50, vec!["gpt-4o"], "mid-target"),
        ]);
        let m = eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .unwrap();
        assert_eq!(m.then.target_model, "high-target");
    }

    #[test]
    fn disabled_route_skipped() {
        let mut route = make_route("disabled", 100, vec!["gpt-4o"], "never");
        route.enabled = false;
        let eng = RoutingEngine::with_routes(vec![
            route,
            make_route("enabled", 10, vec!["gpt-4o"], "winner"),
        ]);
        let m = eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .unwrap();
        assert_eq!(m.then.target_model, "winner");
    }

    #[test]
    fn token_lt_filters() {
        let route = Route {
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                input_tokens_lt: Some(500),
                ..Default::default()
            },
            ..make_route("short-only", 10, vec!["gpt-4o"], "gpt-4o-mini")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .is_some());
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 600)
            .is_none());
    }

    #[test]
    fn token_gt_filters() {
        let route = Route {
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                input_tokens_gt: Some(1000),
                ..Default::default()
            },
            ..make_route("long-only", 10, vec!["gpt-4o"], "claude-opus-4-7")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 500)
            .is_none());
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 1500)
            .is_some());
    }

    #[test]
    fn tag_equals_filters() {
        let route = Route {
            when: RouteConditions {
                tag_equals: Some("background".into()),
                ..Default::default()
            },
            ..make_route("bg-only", 10, vec![], "cheap-model")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .is_none());
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(Some("background")), 100)
            .is_some());
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(Some("foreground")), 100)
            .is_none());
    }

    #[test]
    fn empty_model_in_matches_any_model() {
        let route = make_route("any", 10, vec![], "target");
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(&make_req("claude-sonnet-4-6"), &make_ctx(None), 100)
            .is_some());
    }
}
