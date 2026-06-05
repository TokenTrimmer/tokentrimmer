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
pub mod validate;

pub use cache::CachingRoutingStore;
#[cfg(feature = "postgres")]
pub use store::PostgresRoutingStore;
pub use store::{InMemoryRoutingStore, NewRoute, RoutingStore, RoutingStoreError};
pub use validate::{validate_capability, ValidationError};

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
    /// Match only if the request carries at least one image input part
    /// (`ContentPart::ImageUrl`). `Some(false)` requires no image; `None` ignores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_images: Option<bool>,
    /// Match only if the request carries at least one audio input part
    /// (`ContentPart::InputAudio`). `Some(false)` requires no audio; `None` ignores.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_audio: Option<bool>,
    /// Match if the request's user+system text contains ANY of these keywords
    /// (case-insensitive substring). Empty = ignore.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompt_contains_any_of: Vec<String>,
    /// Match only if the request's estimated cost (USD) is greater than this.
    /// Unknown cost (caller passed `None`) never matches a cost condition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_gt: Option<f64>,
    /// Match only if the request's estimated cost (USD) is less than this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_lt: Option<f64>,
}

/// What a matching [`Route`] does to the request before dispatch.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RouteAction {
    /// Rewrite to this model. May target a different provider than the request
    /// (V3d-1 cross-provider routing); the target is capability-checked and
    /// dispatch/savings use the target's own provider.
    pub target_model: String,
    /// Ordered fallback model ids, tried in order when the primary dispatch
    /// fails with a fallback-eligible error (provider down / 5xx / timeout).
    /// Empty = no failover. The gateway resolves each via the registry, so a
    /// fallback may cross providers. Populated by the cloud routes schema;
    /// `#[serde(default)]` keeps older rows / payloads compatible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<String>,
    /// Override the projected cache layer carried from a Plan apply. The
    /// gateway does not yet honor this at runtime (follow-up: wire
    /// force_cache_layer into the dispatch path); the field is present so a
    /// `tt_plan_core::RouteAction` round-trips losslessly to a
    /// `tt_routing::RouteAction` without dropping the value on apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_cache_layer: Option<String>,
    /// When true, a request this route matches skips L1+L2 entirely (no lookup,
    /// no insert) — for privacy/sensitive traffic that must not persist in the
    /// shared cache. Default false; omitted from JSON when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_cache: bool,
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
        // No cost signal — cost conditions never fire (see `evaluate_with_cost`).
        self.evaluate_with_cost(req, ctx, input_tokens_estimate, None)
    }

    /// Like [`RoutingEngine::evaluate`] but with a pre-flight cost estimate (USD)
    /// for `estimated_cost_gt` / `estimated_cost_lt` conditions. `None` means the
    /// cost is unknown (e.g. the requested model has no pricing) — cost
    /// conditions then never match, mirroring the engine's other "unknown data →
    /// don't match" stances.
    pub fn evaluate_with_cost(
        &self,
        req: &ChatCompletionRequest,
        ctx: &RequestContext,
        input_tokens_estimate: u32,
        estimated_cost_usd: Option<f64>,
    ) -> Option<&Route> {
        self.routes
            .iter()
            .find(|r| r.enabled && matches(r, req, ctx, input_tokens_estimate, estimated_cost_usd))
    }
}

fn matches(
    r: &Route,
    req: &ChatCompletionRequest,
    ctx: &RequestContext,
    input_tokens: u32,
    estimated_cost_usd: Option<f64>,
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
    if let Some(t) = c.estimated_cost_gt {
        // Unknown cost never matches a cost condition.
        if !matches!(estimated_cost_usd, Some(cost) if cost > t) {
            return false;
        }
    }
    if let Some(t) = c.estimated_cost_lt {
        if !matches!(estimated_cost_usd, Some(cost) if cost < t) {
            return false;
        }
    }
    if let Some(tag) = &c.tag_equals {
        if ctx.tag.as_deref() != Some(tag.as_str()) {
            return false;
        }
    }
    if let Some(want) = c.has_images {
        if tt_shared::capability_check::request_has_images(req) != want {
            return false;
        }
    }
    if let Some(want) = c.has_audio {
        if tt_shared::capability_check::request_has_audio(req) != want {
            return false;
        }
    }
    if !c.prompt_contains_any_of.is_empty() {
        let text = tt_shared::capability_check::request_input_text(req).to_lowercase();
        if !c
            .prompt_contains_any_of
            .iter()
            .any(|kw| text.contains(&kw.to_lowercase()))
        {
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
        messages::{ContentPart, ImageUrl, InputAudio},
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
                fallbacks: Vec::new(),
                force_cache_layer: None,
                disable_cache: false,
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

    fn make_req_with_part(model: &str, part: ContentPart) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.into(),
            messages: vec![Message::User {
                content: MessageContent::Parts(vec![part]),
                name: None,
            }],
            ..serde_json::from_str(r#"{"model":"placeholder","messages":[]}"#).unwrap()
        }
    }

    fn image_part() -> ContentPart {
        ContentPart::ImageUrl {
            image_url: ImageUrl {
                url: "data:image/png;base64,abc".into(),
                detail: None,
            },
        }
    }

    fn audio_part() -> ContentPart {
        ContentPart::InputAudio {
            input_audio: InputAudio {
                data: "abc".into(),
                format: "wav".into(),
            },
        }
    }

    #[test]
    fn has_images_true_matches_only_image_requests() {
        let route = Route {
            when: RouteConditions {
                has_images: Some(true),
                ..Default::default()
            },
            ..make_route("vision", 10, vec![], "vision-mini")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(
                &make_req_with_part("gpt-4o", image_part()),
                &make_ctx(None),
                100
            )
            .is_some());
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .is_none());
    }

    #[test]
    fn has_images_false_matches_only_non_image_requests() {
        let route = Route {
            when: RouteConditions {
                has_images: Some(false),
                ..Default::default()
            },
            ..make_route("text", 10, vec![], "cheap")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .is_some());
        assert!(eng
            .evaluate(
                &make_req_with_part("gpt-4o", image_part()),
                &make_ctx(None),
                100
            )
            .is_none());
    }

    #[test]
    fn has_audio_true_matches_only_audio_requests() {
        let route = Route {
            when: RouteConditions {
                has_audio: Some(true),
                ..Default::default()
            },
            ..make_route("audio", 10, vec![], "audio-model")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(
                &make_req_with_part("gpt-4o", audio_part()),
                &make_ctx(None),
                100
            )
            .is_some());
        assert!(eng
            .evaluate(
                &make_req_with_part("gpt-4o", image_part()),
                &make_ctx(None),
                100
            )
            .is_none());
    }

    #[test]
    fn modality_anded_with_model_in() {
        let route = Route {
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                has_images: Some(true),
                ..Default::default()
            },
            ..make_route("both", 10, vec!["gpt-4o"], "vision-mini")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(
                &make_req_with_part("gpt-4o", image_part()),
                &make_ctx(None),
                100
            )
            .is_some());
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
            .is_none());
        assert!(eng
            .evaluate(
                &make_req_with_part("other", image_part()),
                &make_ctx(None),
                100
            )
            .is_none());
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

    // --- rv-routeaction-shared-type: field-parity serde tests ---

    /// (c) Serializing a `RouteAction` with empty fallbacks and no
    /// force_cache_layer must produce the same JSON as before — just
    /// `{"target_model":"x"}` — confirming skip_serializing_if is wired.
    #[test]
    fn route_action_minimal_serializes_without_new_fields() {
        let a = RouteAction {
            target_model: "x".into(),
            fallbacks: Vec::new(),
            force_cache_layer: None,
            disable_cache: false,
        };
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(
            json, r#"{"target_model":"x"}"#,
            "empty fallbacks and None force_cache_layer must be omitted from JSON"
        );
    }

    /// (b) Old JSON that has only `target_model` still deserializes — serde
    /// default fills in empty fallbacks and None force_cache_layer.
    #[test]
    fn route_action_backward_compat_deserialize() {
        let json = r#"{"target_model":"gpt-4o-mini"}"#;
        let a: RouteAction = serde_json::from_str(json).unwrap();
        assert_eq!(a.target_model, "gpt-4o-mini");
        assert!(a.fallbacks.is_empty(), "fallbacks must default to empty");
        assert!(
            a.force_cache_layer.is_none(),
            "force_cache_layer must default to None"
        );
    }

    /// (a) Full round-trip: a `RouteAction` with both new fields serializes to
    /// JSON that carries both fields, and deserializes back with all values
    /// preserved — no field dropped.
    #[test]
    fn route_action_full_round_trip() {
        let original = RouteAction {
            target_model: "claude-haiku-4-5".into(),
            fallbacks: vec!["gpt-4o-mini".into(), "gemini-flash".into()],
            force_cache_layer: Some("l1".into()),
            disable_cache: false,
        };
        let json = serde_json::to_string(&original).unwrap();
        // Both new fields must appear in the serialized JSON.
        assert!(
            json.contains("\"fallbacks\""),
            "fallbacks must be present: {json}"
        );
        assert!(
            json.contains("\"force_cache_layer\""),
            "force_cache_layer must be present: {json}"
        );
        let roundtripped: RouteAction = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped.target_model, original.target_model);
        assert_eq!(roundtripped.fallbacks, original.fallbacks);
        assert_eq!(roundtripped.force_cache_layer, original.force_cache_layer);
    }

    #[test]
    fn route_action_disable_cache_defaults_false_and_omits() {
        // Omitted from JSON when false (back-compat: existing rows unchanged).
        let a = RouteAction {
            target_model: "x".into(),
            fallbacks: Vec::new(),
            force_cache_layer: None,
            disable_cache: false,
        };
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            r#"{"target_model":"x"}"#
        );
        // Defaults false when absent.
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert!(!parsed.disable_cache);
        // Present when true.
        let b = RouteAction {
            disable_cache: true,
            ..a
        };
        assert!(serde_json::to_string(&b)
            .unwrap()
            .contains("\"disable_cache\":true"));
    }

    /// Cross-crate lossless round-trip: JSON produced by `tt_routing::RouteAction`
    /// (with both fields) deserializes into a structurally identical representation.
    /// Because both types are now field-identical (target_model, fallbacks,
    /// force_cache_layer), the JSON is the shared wire format — a plan apply
    /// can serialize a `tt_plan_core::RouteAction` and the gateway reads it as
    /// a `tt_routing::RouteAction` without dropping any field.
    #[test]
    fn route_action_cross_type_wire_compat() {
        // Simulate the JSON a tt_plan_core::RouteAction with all fields would
        // produce (field names and serde attributes are now identical).
        let plan_side_json = r#"{"target_model":"claude-3-5-haiku","fallbacks":["gpt-4o-mini"],"force_cache_layer":"l1"}"#;
        let gateway_action: RouteAction = serde_json::from_str(plan_side_json).unwrap();
        assert_eq!(gateway_action.target_model, "claude-3-5-haiku");
        assert_eq!(gateway_action.fallbacks, vec!["gpt-4o-mini"]);
        assert_eq!(gateway_action.force_cache_layer.as_deref(), Some("l1"));
    }

    fn make_req_text(model: &str, text: &str) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: model.into(),
            messages: vec![Message::User {
                content: MessageContent::Text(text.into()),
                name: None,
            }],
            ..serde_json::from_str(r#"{"model":"placeholder","messages":[]}"#).unwrap()
        }
    }

    #[test]
    fn prompt_contains_matches_case_insensitive_any() {
        let route = Route {
            when: RouteConditions {
                prompt_contains_any_of: vec!["confidential".into(), "salary".into()],
                ..Default::default()
            },
            ..make_route("topic", 10, vec![], "local")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(
                &make_req_text("gpt-4o", "This is a Confidential memo"),
                &make_ctx(None),
                100
            )
            .is_some());
        assert!(eng
            .evaluate(
                &make_req_text("gpt-4o", "my SALARY is"),
                &make_ctx(None),
                100
            )
            .is_some());
        assert!(eng
            .evaluate(
                &make_req_text("gpt-4o", "the weather today"),
                &make_ctx(None),
                100
            )
            .is_none());
    }

    #[test]
    fn prompt_contains_anded_with_model_in() {
        let route = Route {
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                prompt_contains_any_of: vec!["confidential".into()],
                ..Default::default()
            },
            ..make_route("both", 10, vec!["gpt-4o"], "local")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate(
                &make_req_text("gpt-4o", "confidential"),
                &make_ctx(None),
                100
            )
            .is_some());
        assert!(eng
            .evaluate(&make_req_text("gpt-4o", "hello"), &make_ctx(None), 100)
            .is_none());
    }

    #[test]
    fn cost_gt_matches_above_threshold_only() {
        let route = Route {
            when: RouteConditions {
                estimated_cost_gt: Some(0.02),
                ..Default::default()
            },
            ..make_route("expensive", 10, vec![], "cheaper")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        // est 0.03 > 0.02 → match; 0.01 !> 0.02 → no match; unknown cost → no match.
        assert!(eng
            .evaluate_with_cost(&make_req("gpt-4o"), &make_ctx(None), 100, Some(0.03))
            .is_some());
        assert!(eng
            .evaluate_with_cost(&make_req("gpt-4o"), &make_ctx(None), 100, Some(0.01))
            .is_none());
        assert!(eng
            .evaluate_with_cost(&make_req("gpt-4o"), &make_ctx(None), 100, None)
            .is_none());
    }

    #[test]
    fn cost_lt_anded_with_model_in() {
        let route = Route {
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                estimated_cost_lt: Some(0.05),
                ..Default::default()
            },
            ..make_route("cheap-small", 10, vec![], "target")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        assert!(eng
            .evaluate_with_cost(&make_req("gpt-4o"), &make_ctx(None), 100, Some(0.01))
            .is_some());
        // cost not below threshold → no match
        assert!(eng
            .evaluate_with_cost(&make_req("gpt-4o"), &make_ctx(None), 100, Some(0.09))
            .is_none());
        // wrong model → no match
        assert!(eng
            .evaluate_with_cost(&make_req("claude-x"), &make_ctx(None), 100, Some(0.01))
            .is_none());
    }
}
