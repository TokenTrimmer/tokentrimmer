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
pub mod latency;
pub mod store;
pub mod validate;

pub use cache::CachingRoutingStore;
pub use latency::{LatencyTracker, MIN_SAMPLES as LATENCY_MIN_SAMPLES};
#[cfg(feature = "postgres")]
pub use store::PostgresRoutingStore;
pub use store::{InMemoryRoutingStore, NewRoute, RoutingStore, RoutingStoreError};
pub use validate::{validate_capability, validate_shadow_model, ValidationError};

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
    /// Match only when the *live, gateway-observed* p95 upstream latency (in
    /// milliseconds) for the originally-requested `(provider, model)` exceeds
    /// this threshold — letting a route shift traffic off a primary that is
    /// currently slow.
    ///
    /// # In-process-window + cold-start behavior (READ THIS)
    ///
    /// The signal is the gateway's OWN bounded rolling latency window
    /// ([`crate::LatencyTracker`]), NOT any cloud/`request_logs` data — those are
    /// unavailable at decision time. The window is per-instance and reflects only
    /// recent upstream behavior this replica observed. Consequently:
    ///
    /// - The condition is TRUE only when there are **enough samples**
    ///   (`>= LATENCY_MIN_SAMPLES`) AND the observed p95 strictly exceeds the
    ///   threshold — i.e. a genuinely slow primary triggers the alternate route.
    /// - The condition is **FALSE on cold start / insufficient data**: with no
    ///   tracker supplied, or fewer than the minimum samples for the key, an
    ///   unknown primary does NOT match. This is deliberate — the feature must
    ///   never gate on a fabricated signal. See `matches` / `LatencyTracker::p95`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_latency_ms_p95_gt: Option<u32>,
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
    /// When true, a request this route matches skips L1+L2 entirely (no lookup,
    /// no insert) — for privacy/sensitive traffic that must not persist in the
    /// shared cache. Default false; omitted from JSON when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_cache: bool,
    /// Hard per-request ceiling (USD). After this route's rewrite, if the
    /// rerouted model's estimated cost still exceeds this, the gateway rejects
    /// the request (402) instead of dispatching. `None` = no ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
    /// Opt the matched request into OpenAI's **Flex** service tier
    /// (`service_tier: "flex"`) — a synchronous-but-slower processing tier
    /// billed at ~50% of standard. The gateway sets `service_tier="flex"` on the
    /// upstream request only when the served model is flex-eligible (carries a
    /// Flex rate in the catalog); an ineligible model is left untouched and a
    /// `flex_not_applied:<model>` warning is surfaced. Savings are attributed as
    /// a distinct `flex` source (standard baseline − flex cost). Default false;
    /// omitted from JSON when false (back-compat with existing rows).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub flex: bool,
    /// Opt the matched request into the **conservative compression pass**
    /// (request-pass pipeline, compression pass #1): a content-lossless trim of
    /// *non-prose* blocks (collapse redundant whitespace/blank lines in
    /// system/tool-result payloads, de-duplicate exactly-repeated adjacent
    /// tool-result blocks, canonicalize `tool_calls` arguments JSON). User prose
    /// and the actual instruction content are never altered. The pass runs only
    /// when this is true; the removed input tokens lower the prompt-token bill
    /// and are attributed as a distinct `compression` savings source (standard
    /// baseline − compressed cost). **Off by default** — no behavior change
    /// unless a route enables it; omitted from JSON when false (back-compat).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub compress: bool,
    /// Opt the matched request into the **request-redaction guardrail**
    /// (request-pass pipeline): a conservative SAFETY transform that replaces
    /// PII/secrets in the OUTBOUND request (user prose, system blocks,
    /// tool-result content) with a `[REDACTED]` placeholder BEFORE the request
    /// is dispatched upstream. Reuses the Tier-1 secret patterns plus email/SSN
    /// matchers and prefers over-redacting to leaking. This is **not** a savings
    /// feature: no cost/savings is attributed to redaction; the
    /// `x-tokentrimmer-warnings` header (`redacted:<class>`) is the signal that
    /// redaction fired. It redacts the upstream request, not the gateway's logs.
    /// **Off by default** — no behavior change unless a route enables it;
    /// omitted from JSON when false (back-compat).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redact: bool,
    /// Canary traffic split (0-100). When `Some(pct)`, only a deterministic
    /// `pct`% of the matched requests are routed to `target_model` (the canary
    /// arm); the remaining `(100 - pct)%` pass through on their
    /// originally-requested model (the control arm). The arm is chosen by the
    /// replica-independent [`sticky_traffic_split`] hash so the same logical
    /// request always lands in the same arm across replicas and retries.
    ///
    /// `None` (the default) = 100% of matched requests take the route
    /// (unconditional rewrite, today's behavior). `Some(0)` = 0% canary (the
    /// route never rewrites, only its `shadow_model`/side-effects could fire).
    /// Values above 100 are clamped to 100 by the split function. Omitted from
    /// JSON when `None` (back-compat with existing rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traffic_pct: Option<u32>,
    /// Shadow-mode candidate model. When `Some(model)`, the gateway ALSO
    /// dispatches the matched request to `model` as a **shadow** (non-streaming,
    /// single candidate, NO failover), DISCARDS the shadow response, and records
    /// the shadow's cost/usage SEPARATELY (never folded into the primary cost).
    /// Only the primary (control/canary) response is ever returned to the client.
    ///
    /// Shadow mode DOUBLES upstream spend for matched requests, so it is opt-in
    /// (default `None`) and the shadow cost is logged in its own column /
    /// span attribute for reconciliation. A `shadow_model` with no `traffic_pct`
    /// means 100% of matched requests are shadowed (the primary still serves
    /// normally). The configured `shadow_model` MUST resolve to a registered
    /// provider — config validation rejects an unresolvable shadow at route
    /// creation time (fail at config time, not silently at dispatch). Omitted
    /// from JSON when `None` (back-compat with existing rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow_model: Option<String>,
}

/// Deterministic, replica-independent canary arm selection.
///
/// Returns `true` when this request falls in the **canary** arm (route to the
/// new `target_model`) and `false` when it falls in the **control** arm (pass
/// through unchanged), such that — over many distinct idempotency keys — about
/// `traffic_pct`% land in the canary arm.
///
/// # Determinism + replica independence (READ THIS)
///
/// This is a PURE function of its three arguments. It uses the std-library
/// [`DefaultHasher`](std::collections::hash_map::DefaultHasher) — exactly the
/// deterministic pattern in `quality_sample::should_sample` — and contains NO
/// RNG, NO clock, NO per-instance/process state. Therefore every gateway
/// replica computes the SAME arm for the SAME `(org_id, idempotency_key,
/// traffic_pct)`, and a client retry with the same idempotency key is sticky to
/// the same arm. This is required so a canary split is consistent fleet-wide
/// rather than re-rolling the dice per replica or per retry.
///
/// # Hash input
///
/// The hash absorbs, in order: the 16 raw bytes of `org_id`, a `0xff` domain
/// separator byte (so an org id and an idempotency key that happen to share a
/// byte boundary cannot collide), and the UTF-8 bytes of `idempotency_key`. The
/// 64-bit hash is mapped to a bucket in `[0, 100)` via the top-of-range modulo;
/// the request is in the canary arm iff `bucket < traffic_pct`. Keying on
/// `org_id` as well as the idempotency key means two different orgs that reuse
/// the same idempotency-key string get independent (uncorrelated) splits.
///
/// # Edge cases
///
/// - `traffic_pct == 0` → always `false` (no request is canaried).
/// - `traffic_pct >= 100` → always `true` (every request is canaried); values
///   above 100 are clamped to 100.
/// - An EMPTY `idempotency_key` still hashes deterministically (it just means
///   every request with an empty key for a given org lands in the same arm) —
///   callers that cannot supply a stable key should pass a fresh uuid string so
///   the request is treated as a one-off (its arm is then effectively random
///   but still self-consistent for that single request).
#[must_use]
pub fn sticky_traffic_split(org_id: Uuid, idempotency_key: &str, traffic_pct: u32) -> bool {
    if traffic_pct == 0 {
        return false;
    }
    if traffic_pct >= 100 {
        return true;
    }
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    // Absorb the org id's raw bytes, a domain separator, then the key bytes.
    // Hashing the bytes directly (rather than `Uuid::hash`) keeps the input
    // wire-stable regardless of any future change to `Uuid`'s `Hash` impl.
    org_id.as_bytes().hash(&mut hasher);
    0xffu8.hash(&mut hasher);
    idempotency_key.as_bytes().hash(&mut hasher);
    let h = hasher.finish();
    // Map uniformly into [0, 100). `% 100` is uniform enough for a canary split
    // given a well-mixed 64-bit hash (the modulo bias against 100 across 2^64 is
    // ~6e-18, far below any traffic-shaping precision we need).
    let bucket = (h % 100) as u32;
    bucket < traffic_pct
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
        // No latency signal — latency conditions never fire (cold-start FALSE).
        self.evaluate_with_signals(req, ctx, input_tokens_estimate, estimated_cost_usd, None)
    }

    /// Like [`RoutingEngine::evaluate_with_cost`] but also threads the live,
    /// gateway-observed p95 upstream latency (milliseconds) for the
    /// originally-requested `(provider, model)`, for the
    /// `upstream_latency_ms_p95_gt` condition.
    ///
    /// `observed_p95_ms` is `None` when the gateway's
    /// [`crate::LatencyTracker`] has insufficient samples for that key (cold
    /// start) or no tracker is wired. A `None` latency makes the latency
    /// condition FALSE — never a fabricated match. The caller computes this once
    /// (all routes evaluate the same originally-requested model) and passes it
    /// in, mirroring `estimated_cost_usd`.
    pub fn evaluate_with_signals(
        &self,
        req: &ChatCompletionRequest,
        ctx: &RequestContext,
        input_tokens_estimate: u32,
        estimated_cost_usd: Option<f64>,
        observed_p95_ms: Option<u32>,
    ) -> Option<&Route> {
        self.routes.iter().find(|r| {
            r.enabled
                && matches(
                    r,
                    req,
                    ctx,
                    input_tokens_estimate,
                    estimated_cost_usd,
                    observed_p95_ms,
                )
        })
    }

    /// Find an enabled route by exact name (case-sensitive), bypassing condition
    /// evaluation — used to honor a forced-route request header.
    pub fn find_by_name(&self, name: &str) -> Option<&Route> {
        self.routes.iter().find(|r| r.enabled && r.name == name)
    }
}

fn matches(
    r: &Route,
    req: &ChatCompletionRequest,
    ctx: &RequestContext,
    input_tokens: u32,
    estimated_cost_usd: Option<f64>,
    observed_p95_ms: Option<u32>,
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
    if let Some(t) = c.upstream_latency_ms_p95_gt {
        // Live, gateway-observed p95 (in-process window). `None` = insufficient
        // samples for this `(provider, model)` (cold start) or no tracker wired
        // — which must NOT match, so a fresh/unknown primary never gates on a
        // fabricated signal. Only an observed p95 STRICTLY above the threshold
        // fires the route, shifting traffic off a genuinely-slow primary.
        if !matches!(observed_p95_ms, Some(p95) if p95 > t) {
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
                disable_cache: false,
                max_cost_usd: None,
                flex: false,
                compress: false,
                redact: false,
                traffic_pct: None,
                shadow_model: None,
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
    fn find_by_name_matches_enabled_route_by_exact_name() {
        let enabled = make_route("alpha", 10, vec!["gpt-4o"], "gpt-4o-mini");
        let mut disabled = make_route("beta", 10, vec!["gpt-4o"], "gpt-4o-mini");
        disabled.enabled = false;
        let eng = RoutingEngine::with_routes(vec![enabled, disabled]);
        assert!(eng.find_by_name("alpha").is_some());
        assert_eq!(eng.find_by_name("alpha").unwrap().name, "alpha");
        assert!(
            eng.find_by_name("beta").is_none(),
            "disabled route not found"
        );
        assert!(eng.find_by_name("missing").is_none());
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

    /// (c) Serializing a `RouteAction` with empty fallbacks produces the
    /// minimal JSON `{"target_model":"x"}` — confirming skip_serializing_if.
    #[test]
    fn route_action_minimal_serializes_without_new_fields() {
        let a = RouteAction {
            target_model: "x".into(),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            compress: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
        };
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(
            json, r#"{"target_model":"x"}"#,
            "empty fallbacks must be omitted from JSON"
        );
    }

    /// (b) Old JSON that has only `target_model` still deserializes — serde
    /// default fills in empty fallbacks.
    #[test]
    fn route_action_backward_compat_deserialize() {
        let json = r#"{"target_model":"gpt-4o-mini"}"#;
        let a: RouteAction = serde_json::from_str(json).unwrap();
        assert_eq!(a.target_model, "gpt-4o-mini");
        assert!(a.fallbacks.is_empty(), "fallbacks must default to empty");
    }

    /// (a) Full round-trip: a `RouteAction` with fallbacks serializes to JSON
    /// carrying them, and deserializes back with all values preserved.
    #[test]
    fn route_action_full_round_trip() {
        let original = RouteAction {
            target_model: "claude-haiku-4-5".into(),
            fallbacks: vec!["gpt-4o-mini".into(), "gemini-flash".into()],
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            compress: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(
            json.contains("\"fallbacks\""),
            "fallbacks must be present: {json}"
        );
        let roundtripped: RouteAction = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtripped.target_model, original.target_model);
        assert_eq!(roundtripped.fallbacks, original.fallbacks);
    }

    #[test]
    fn route_action_disable_cache_defaults_false_and_omits() {
        // Omitted from JSON when false (back-compat: existing rows unchanged).
        let a = RouteAction {
            target_model: "x".into(),
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: None,
            flex: false,
            compress: false,
            redact: false,
            traffic_pct: None,
            shadow_model: None,
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
    /// (target_model + fallbacks) deserializes into a structurally identical
    /// representation. Because both types are field-identical, the JSON is the
    /// shared wire format — a plan apply can serialize a `tt_plan_core::RouteAction`
    /// and the gateway reads it as a `tt_routing::RouteAction` without loss.
    #[test]
    fn route_action_cross_type_wire_compat() {
        let plan_side_json = r#"{"target_model":"claude-3-5-haiku","fallbacks":["gpt-4o-mini"]}"#;
        let gateway_action: RouteAction = serde_json::from_str(plan_side_json).unwrap();
        assert_eq!(gateway_action.target_model, "claude-3-5-haiku");
        assert_eq!(gateway_action.fallbacks, vec!["gpt-4o-mini"]);
        let reemitted = serde_json::to_string(&gateway_action).unwrap();
        assert_eq!(reemitted, plan_side_json);
    }

    /// Cross-type wire-compat for the `redact` guardrail flag: JSON written by
    /// the plan side carrying `"redact":true` deserializes into a
    /// `tt_routing::RouteAction` with the flag set and re-emits the same wire
    /// form (same field order + skip_serializing_if gating). Locks `redact` into
    /// the shared wire format alongside `flex`/`compress`.
    #[test]
    fn route_action_redact_cross_type_wire_compat() {
        let plan_side_json = r#"{"target_model":"gpt-4o","redact":true}"#;
        let gateway_action: RouteAction = serde_json::from_str(plan_side_json).unwrap();
        assert_eq!(gateway_action.target_model, "gpt-4o");
        assert!(
            gateway_action.redact,
            "redact must round-trip from plan JSON"
        );
        let reemitted = serde_json::to_string(&gateway_action).unwrap();
        assert_eq!(reemitted, plan_side_json);
    }

    /// Back-compat: legacy JSON still carrying the removed `force_cache_layer`
    /// key deserializes fine (serde ignores the unknown field) and re-serializes
    /// without it. Guards persisted routes written before the removal.
    #[test]
    fn route_action_legacy_force_cache_layer_is_ignored() {
        let legacy =
            r#"{"target_model":"claude-3-5-haiku","fallbacks":["x"],"force_cache_layer":"l1"}"#;
        let a: RouteAction = serde_json::from_str(legacy).unwrap();
        assert_eq!(a.target_model, "claude-3-5-haiku");
        assert_eq!(a.fallbacks, vec!["x"]);
        let j = serde_json::to_string(&a).unwrap();
        assert!(
            !j.contains("force_cache_layer"),
            "obsolete key must not be re-emitted: {j}"
        );
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
    fn route_action_flex_defaults_false_omits_and_round_trips() {
        // Omitted from JSON when false (back-compat: existing rows unchanged).
        let mut a = make_route("x", 10, vec![], "gpt-5.4").then;
        assert!(
            !serde_json::to_string(&a).unwrap().contains("flex"),
            "flex must be omitted when false"
        );
        // Defaults false when absent from legacy JSON.
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert!(!parsed.flex);
        // Present + round-trips when true.
        a.flex = true;
        let j = serde_json::to_string(&a).unwrap();
        assert!(j.contains("\"flex\":true"), "flex=true must serialize: {j}");
        let back: RouteAction = serde_json::from_str(&j).unwrap();
        assert!(back.flex);
    }

    #[test]
    fn route_action_redact_defaults_false_omits_and_round_trips() {
        // Omitted from JSON when false (back-compat: existing rows unchanged).
        let mut a = make_route("x", 10, vec![], "gpt-4o-mini").then;
        assert!(
            !serde_json::to_string(&a).unwrap().contains("redact"),
            "redact must be omitted when false"
        );
        // Defaults false when absent from legacy JSON.
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert!(!parsed.redact, "redact must default false");
        // Present + round-trips when true.
        a.redact = true;
        let j = serde_json::to_string(&a).unwrap();
        assert!(
            j.contains("\"redact\":true"),
            "redact=true must serialize: {j}"
        );
        let back: RouteAction = serde_json::from_str(&j).unwrap();
        assert!(back.redact);
    }

    #[test]
    fn max_cost_usd_round_trips_and_omits_when_none() {
        let mut a = make_route("x", 10, vec![], "gpt-4o-mini").then;
        assert!(!serde_json::to_string(&a).unwrap().contains("max_cost_usd"));
        a.max_cost_usd = Some(0.1);
        let j = serde_json::to_string(&a).unwrap();
        assert!(j.contains("\"max_cost_usd\":0.1"));
        let back: RouteAction = serde_json::from_str(&j).unwrap();
        assert_eq!(back.max_cost_usd, Some(0.1));
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

    // --- upstream_latency_ms_p95_gt: live-signal condition tests ---

    fn latency_route(threshold_ms: u32) -> Route {
        Route {
            when: RouteConditions {
                upstream_latency_ms_p95_gt: Some(threshold_ms),
                ..Default::default()
            },
            ..make_route("slow-primary", 10, vec![], "faster-alt")
        }
    }

    #[test]
    fn p95_condition_false_without_tracker_signal() {
        // No latency signal (None) → cold start → never matches, even though the
        // route would otherwise match any model. This is the anti-masquerading
        // guarantee: with no real data, the latency route does NOT fire.
        let eng = RoutingEngine::with_routes(vec![latency_route(1000)]);
        assert!(
            eng.evaluate(&make_req("gpt-4o"), &make_ctx(None), 100)
                .is_none(),
            "no signal → no match"
        );
        assert!(eng
            .evaluate_with_signals(&make_req("gpt-4o"), &make_ctx(None), 100, None, None)
            .is_none());
    }

    #[test]
    fn p95_condition_false_when_p95_at_or_below_threshold() {
        let eng = RoutingEngine::with_routes(vec![latency_route(1000)]);
        // p95 == threshold → strictly-greater requirement not met → no match.
        assert!(eng
            .evaluate_with_signals(&make_req("gpt-4o"), &make_ctx(None), 100, None, Some(1000))
            .is_none());
        // p95 below threshold → no match.
        assert!(eng
            .evaluate_with_signals(&make_req("gpt-4o"), &make_ctx(None), 100, None, Some(800))
            .is_none());
    }

    #[test]
    fn p95_condition_true_when_p95_exceeds_threshold() {
        let eng = RoutingEngine::with_routes(vec![latency_route(1000)]);
        let m = eng
            .evaluate_with_signals(&make_req("gpt-4o"), &make_ctx(None), 100, None, Some(1500))
            .expect("slow primary → alternate route fires");
        assert_eq!(m.then.target_model, "faster-alt");
    }

    #[test]
    fn p95_condition_fed_by_tracker_min_samples_gate() {
        use crate::LatencyTracker;
        // End-to-end with the real tracker: below MIN_SAMPLES p95 is None →
        // condition FALSE; once enough slow samples exist, p95 > threshold → TRUE.
        let tracker = LatencyTracker::new();
        let eng = RoutingEngine::with_routes(vec![latency_route(1000)]);

        // Feed a handful of slow samples — not yet enough to report a p95.
        for _ in 0..(crate::LATENCY_MIN_SAMPLES - 1) {
            tracker.record("openai", "gpt-4o", 3000);
        }
        let p95_cold = tracker.p95("openai", "gpt-4o");
        assert!(p95_cold.is_none(), "still cold");
        assert!(
            eng.evaluate_with_signals(&make_req("gpt-4o"), &make_ctx(None), 100, None, p95_cold)
                .is_none(),
            "cold start (insufficient samples) → no match"
        );

        // One more slow sample crosses MIN_SAMPLES → p95 is reported and high.
        tracker.record("openai", "gpt-4o", 3000);
        let p95_warm = tracker.p95("openai", "gpt-4o");
        assert_eq!(p95_warm, Some(3000));
        assert!(
            eng.evaluate_with_signals(&make_req("gpt-4o"), &make_ctx(None), 100, None, p95_warm)
                .is_some(),
            "warm + slow → alternate route fires"
        );
    }

    #[test]
    fn p95_condition_serde_round_trip_and_omitted_when_none() {
        // Omitted from JSON when None (back-compat with existing route rows).
        let when = RouteConditions::default();
        let j = serde_json::to_string(&when).unwrap();
        assert!(
            !j.contains("upstream_latency_ms_p95_gt"),
            "must be omitted when None: {j}"
        );
        // Present + round-trips when set.
        let when2 = RouteConditions {
            upstream_latency_ms_p95_gt: Some(1500),
            ..Default::default()
        };
        let j2 = serde_json::to_string(&when2).unwrap();
        assert!(j2.contains("\"upstream_latency_ms_p95_gt\":1500"), "{j2}");
        let back: RouteConditions = serde_json::from_str(&j2).unwrap();
        assert_eq!(back.upstream_latency_ms_p95_gt, Some(1500));
        // Legacy JSON without the field still deserializes (serde default None).
        let legacy: RouteConditions = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.upstream_latency_ms_p95_gt, None);
    }

    #[test]
    fn p95_condition_anded_with_model_in() {
        let route = Route {
            when: RouteConditions {
                model_in: vec!["gpt-4o".into()],
                upstream_latency_ms_p95_gt: Some(1000),
                ..Default::default()
            },
            ..make_route("both", 10, vec!["gpt-4o"], "faster-alt")
        };
        let eng = RoutingEngine::with_routes(vec![route]);
        // Right model + slow p95 → match.
        assert!(eng
            .evaluate_with_signals(&make_req("gpt-4o"), &make_ctx(None), 100, None, Some(2000))
            .is_some());
        // Wrong model → no match even though p95 is high.
        assert!(eng
            .evaluate_with_signals(
                &make_req("claude-x"),
                &make_ctx(None),
                100,
                None,
                Some(2000)
            )
            .is_none());
        // Right model but fast → no match.
        assert!(eng
            .evaluate_with_signals(&make_req("gpt-4o"), &make_ctx(None), 100, None, Some(500))
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

    // --- canary: traffic_pct + shadow_model serde + sticky split tests ---

    /// `traffic_pct` / `shadow_model` default to `None` from legacy JSON and are
    /// omitted from the wire form when `None` (back-compat with existing rows).
    #[test]
    fn canary_fields_default_none_and_omit() {
        let a = make_route("x", 10, vec![], "gpt-4o-mini").then;
        let j = serde_json::to_string(&a).unwrap();
        assert!(
            !j.contains("traffic_pct"),
            "traffic_pct must be omitted when None: {j}"
        );
        assert!(
            !j.contains("shadow_model"),
            "shadow_model must be omitted when None: {j}"
        );
        // Legacy JSON without the fields still deserializes with None defaults.
        let parsed: RouteAction = serde_json::from_str(r#"{"target_model":"m"}"#).unwrap();
        assert_eq!(parsed.traffic_pct, None);
        assert_eq!(parsed.shadow_model, None);
    }

    /// `traffic_pct` + `shadow_model` round-trip and serialize when set.
    #[test]
    fn canary_fields_round_trip_when_set() {
        let mut a = make_route("x", 10, vec![], "gpt-4o-mini").then;
        a.traffic_pct = Some(25);
        a.shadow_model = Some("claude-haiku-4-5".into());
        let j = serde_json::to_string(&a).unwrap();
        assert!(j.contains("\"traffic_pct\":25"), "{j}");
        assert!(j.contains("\"shadow_model\":\"claude-haiku-4-5\""), "{j}");
        let back: RouteAction = serde_json::from_str(&j).unwrap();
        assert_eq!(back.traffic_pct, Some(25));
        assert_eq!(back.shadow_model.as_deref(), Some("claude-haiku-4-5"));
    }

    /// Cross-type wire-compat: JSON written by the plan side carrying the canary
    /// fields deserializes into a `tt_routing::RouteAction` with both set and
    /// re-emits the same wire form (same field order + skip_serializing_if
    /// gating). Locks `traffic_pct`/`shadow_model` into the shared wire format
    /// alongside `flex`/`compress`/`redact`. Mirrored by
    /// `tt_plan_core::types::tests::route_action_canary_cross_type_wire_compat`.
    #[test]
    fn route_action_canary_cross_type_wire_compat() {
        let plan_side_json =
            r#"{"target_model":"gpt-4o","traffic_pct":30,"shadow_model":"claude-haiku-4-5"}"#;
        let gateway_action: RouteAction = serde_json::from_str(plan_side_json).unwrap();
        assert_eq!(gateway_action.target_model, "gpt-4o");
        assert_eq!(gateway_action.traffic_pct, Some(30));
        assert_eq!(
            gateway_action.shadow_model.as_deref(),
            Some("claude-haiku-4-5")
        );
        let reemitted = serde_json::to_string(&gateway_action).unwrap();
        assert_eq!(reemitted, plan_side_json);
    }

    /// `sticky_traffic_split` is deterministic: the SAME `(org, key, pct)` always
    /// returns the SAME arm — the property a multi-replica fleet and client
    /// retries depend on. (Replica-independence is implied by purity: no RNG, no
    /// clock, no captured state — the same inputs always produce the same output
    /// in any process.)
    #[test]
    fn sticky_split_is_deterministic_for_same_key() {
        let org = Uuid::now_v7();
        for key in ["req-1", "req-2", "abc", ""] {
            let first = sticky_traffic_split(org, key, 50);
            for _ in 0..1000 {
                assert_eq!(
                    sticky_traffic_split(org, key, 50),
                    first,
                    "same (org,key,pct) must always pick the same arm — key={key:?}"
                );
            }
        }
    }

    /// `pct == 0` never canaries; `pct >= 100` always canaries (and values above
    /// 100 are clamped, not arithmetic-overflowed).
    #[test]
    fn sticky_split_boundary_pcts() {
        let org = Uuid::now_v7();
        for key in ["a", "b", "c", "really-long-idempotency-key-value", ""] {
            assert!(
                !sticky_traffic_split(org, key, 0),
                "pct=0 must never canary"
            );
            assert!(
                sticky_traffic_split(org, key, 100),
                "pct=100 must always canary"
            );
            assert!(
                sticky_traffic_split(org, key, 250),
                "pct>100 clamps to always-canary"
            );
        }
    }

    /// Over many distinct idempotency keys the canary fraction is close to
    /// `traffic_pct`% — i.e. the split actually shapes traffic by the configured
    /// percentage rather than being all-or-nothing.
    #[test]
    fn sticky_split_distribution_approximates_pct() {
        let org = Uuid::now_v7();
        const N: usize = 20_000;
        for pct in [10u32, 30, 50, 70, 90] {
            let canaried = (0..N)
                .filter(|i| sticky_traffic_split(org, &format!("key-{i}"), pct))
                .count();
            let observed = canaried as f64 / N as f64 * 100.0;
            // 20k samples → the std error on the proportion is well under 0.5pp;
            // a 3pp tolerance is comfortably loose yet still proves shaping.
            assert!(
                (observed - pct as f64).abs() < 3.0,
                "pct={pct}: observed canary share {observed:.2}% off target by >3pp"
            );
        }
    }

    /// Two different orgs reusing the SAME idempotency-key string get
    /// independent (uncorrelated) splits — keying on `org_id` prevents one org's
    /// key-space from dictating another's arms.
    #[test]
    fn sticky_split_is_org_scoped() {
        // Find a key that lands in different arms for two orgs (exists with high
        // probability since the splits are independent at pct=50).
        let org_a = Uuid::from_u128(1);
        let org_b = Uuid::from_u128(2);
        let differs = (0..1000).any(|i| {
            let k = format!("shared-{i}");
            sticky_traffic_split(org_a, &k, 50) != sticky_traffic_split(org_b, &k, 50)
        });
        assert!(
            differs,
            "two orgs must not be forced into identical arms for every shared key"
        );
    }
}
