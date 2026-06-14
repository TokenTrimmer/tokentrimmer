//! Sub-lever 1 — **lossless cache-prefix breakpoint annotation**.
//!
//! The highest-leverage, lowest-risk agentic-budget lever (spec §1.5, §4.3):
//! make the provider's prompt cache fire on the static + history prefix the
//! caller forgot to mark. It is NOT a [`RequestPass`](crate::passes::RequestPass):
//! a pass may mutate only the volatile tail, but this lever's whole job is to
//! confirm and annotate the breakpoint the provider keys the STABLE prefix on
//! — so it runs through the planner BEFORE the pass pipeline and touches the
//! prefix's *framing*, never its content.
//!
//! # Why this never busts the cache (structural defense against forbidden F2)
//!
//! The forbidden mode (spec §4.5 / passes module docs) is rewriting the
//! cache-stable prefix with non-deterministic output every turn, destroying the
//! provider cache and re-billing at up to ~6.4×, then claiming it as savings.
//! Sub-lever 1 cannot do this: it does not mutate prefix CONTENT at all. It
//! reuses [`compute_stable_region`] to locate the boundary the adapters'
//! injection gates already cache on, and emits a [`CachePrefixPlan`] describing
//! WHERE the breakpoint belongs. Because no content changes,
//! [`SplitRequest::mutate_whole_request`] is never engaged and the operation
//! reports [`CacheBustEstimate::NONE`].
//!
//! # Per-provider behavior
//!
//! - **`anthropic`** (explicit-breakpoint style): mark the largest stable
//!   boundary the adapter's `maybe_inject_cache_control` /
//!   `maybe_inject_message_cache_control` would honor — the system prefix on a
//!   single-shot ([`StableReason::SystemPrefix`]), or the rolling last-message
//!   breakpoint over the whole history on a cache-qualified multi-turn chat
//!   ([`StableReason::MultiTurnHistory`]). The adapter performs the actual
//!   `cache_control` injection (#126/#150, adapter-owned); the plan records the
//!   boundary so the planner can net Sub-lever 1 against the other levers.
//! - **positional-auto-cache providers (OpenAI)**: there is no breakpoint to
//!   insert — the provider auto-caches the ≥`min`-token prefix positionally. The
//!   plan asserts the stable prefix is byte-identical across turns (a no-op when
//!   it already is) so auto-caching keeps firing.
//!
//! # Accounting (spec §4.3)
//!
//! Sub-lever 1 books **NO TT headline savings and NO cache bust**. The
//! provider's own prompt-cache discount is reported on the SEPARATE
//! `provider_cache_saved_usd` axis (derived downstream from the provider's
//! realized cache-read usage), never folded into the TT headline — so the
//! ledger survives a CFO reconciling provider invoices. This is pure
//! provider-cache *enablement*.

use tt_shared::ChatCompletionRequest;

use crate::passes::split::{compute_stable_region, CacheBustEstimate, PassContext, StableReason};

/// Where Sub-lever 1 would have the provider key its prompt cache, and on what
/// basis — a lossless **annotation**, not a content mutation.
///
/// `boundary_len == 0` means "no qualifying prefix" (below the model's
/// prompt-cache minimum, or not cache-capable): the plan is a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CachePrefixPlan {
    /// Number of leading messages (`messages[..boundary_len]`) the provider's
    /// cache keys on — the largest stable boundary. `0` = no annotation.
    pub boundary_len: usize,
    /// Estimated tokens of the cache-keyed prefix (the SYSTEM text for a
    /// single-shot Anthropic [`StableReason::SystemPrefix`], the whole prefix
    /// otherwise) — the basis for the SEPARATE `provider_cache_saved_usd`
    /// estimate, never the TT headline.
    pub prefix_tokens: u32,
    /// Why this boundary is (or is not) the cache key — mirrors the split.
    pub reason: StableReason,
    /// Whether an explicit `cache_control` breakpoint is warranted here. `true`
    /// for the Anthropic explicit-breakpoint styles ([`StableReason::SystemPrefix`]
    /// / [`StableReason::MultiTurnHistory`]); `false` for positional-auto-cache
    /// providers (the prefix is cached positionally — no breakpoint to insert,
    /// the plan only asserts byte-stability) and for any non-qualifying region.
    pub explicit_breakpoint: bool,
}

impl CachePrefixPlan {
    /// The no-op plan: nothing qualified for caching, so nothing is annotated.
    pub const NONE: CachePrefixPlan = CachePrefixPlan {
        boundary_len: 0,
        prefix_tokens: 0,
        reason: StableReason::BelowMinTokens,
        explicit_breakpoint: false,
    };

    /// True when this plan annotates a real cache boundary.
    #[must_use]
    pub fn is_some(&self) -> bool {
        self.boundary_len > 0
    }
}

/// Annotate the request's cache-prefix breakpoint **losslessly**.
///
/// Reuses [`compute_stable_region`] to find the largest stable boundary the
/// adapters already cache on, returning a [`CachePrefixPlan`] describing it. It
/// does **not** mutate the request's message content — the dispatched message
/// TEXT is byte-identical to the input — so it can never bust the provider
/// cache. The returned [`CacheBustEstimate`] is therefore always
/// [`CacheBustEstimate::NONE`]: the structural defense against forbidden mode
/// F2 (the [`SplitRequest::mutate_whole_request`](crate::passes::SplitRequest::mutate_whole_request)
/// escape hatch is never engaged, so no negative entry can book).
///
/// Returns `(plan, CacheBustEstimate::NONE)`. The TT headline books nothing
/// here; the provider's discount lands on the separate `provider_cache_saved_usd`
/// axis (spec §4.3). The returned [`CacheBustEstimate`] is already `#[must_use]`
/// (the booking contract), so the tuple cannot be silently dropped.
pub fn annotate_cache_prefix(
    req: &ChatCompletionRequest,
    cx: &PassContext<'_>,
) -> (CachePrefixPlan, CacheBustEstimate) {
    let (boundary_len, prefix_tokens, reason) = compute_stable_region(&req.messages, cx);

    if boundary_len == 0 {
        // Below the model's prompt-cache minimum, or not cache-capable: there
        // is nothing to annotate. No-op, no bust.
        return (CachePrefixPlan::NONE, CacheBustEstimate::NONE);
    }

    // Anthropic uses EXPLICIT breakpoints (the adapter injects `cache_control`
    // on the largest stable boundary); positional-auto-cache providers cache
    // the prefix without a breakpoint, so the plan only asserts byte-stability.
    let explicit_breakpoint = matches!(
        reason,
        StableReason::SystemPrefix | StableReason::MultiTurnHistory
    ) && cx.provider_id == "anthropic";

    let plan = CachePrefixPlan {
        boundary_len,
        prefix_tokens,
        reason,
        explicit_breakpoint,
    };

    // Lossless annotation only — prefix CONTENT is untouched, so the provider
    // cache keeps hitting and no bust books. NEVER engage `mutate_whole_request`.
    (plan, CacheBustEstimate::NONE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tt_shared::messages::{Message, MessageContent};
    use tt_shared::pricing::ModelPricing;
    use tt_shared::ChatCompletionRequest;

    fn sys(text: &str) -> Message {
        Message::System {
            content: MessageContent::Text(text.into()),
        }
    }
    fn user(text: &str) -> Message {
        Message::User {
            content: MessageContent::Text(text.into()),
            name: None,
        }
    }
    fn assistant(text: &str) -> Message {
        Message::Assistant {
            content: Some(MessageContent::Text(text.into())),
            tool_calls: vec![],
            name: None,
        }
    }
    fn req_with(messages: Vec<Message>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "claude-sonnet-4-6".into(),
            messages,
            ..Default::default()
        }
    }
    fn pricing(min: Option<u32>, input: f64, cached: Option<f64>) -> ModelPricing {
        ModelPricing {
            input_per_million: input,
            output_per_million: 2.0 * input,
            cached_input_per_million: cached,
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: min,
            effective_at: Utc::now(),
        }
    }

    /// Project the per-message TEXT a pass could touch — the bytes that must be
    /// byte-identical before/after annotation (annotation marks framing, never
    /// rewrites content).
    fn message_text_projection(req: &ChatCompletionRequest) -> String {
        crate::passes::messages_text(&req.messages)
    }

    /// Sub-lever 1 on a cache-qualified multi-turn Anthropic request marks the
    /// largest stable history boundary for `cache_control`, and the dispatched
    /// message TEXT is BYTE-IDENTICAL to the input — annotation only, never a
    /// content rewrite. Only the cache-control plan differs.
    #[test]
    fn cache_prefix_annotates_breakpoint_lossless() {
        let cx = PassContext {
            provider_id: "anthropic",
            model: "claude-sonnet-4-6",
            pricing: None,
        };
        // ~3000 tokens of conversation history — over Sonnet's 2048 catalog
        // minimum, with an assistant turn so the request is multi-turn.
        let long_history = "word ".repeat(3000);
        let req = req_with(vec![
            sys("you are a helpful assistant"),
            user(&long_history),
            assistant("here is my answer"),
            user("a follow-up question"),
        ]);

        // The TEXT projection and the serialized message bytes BEFORE
        // annotation — captured so we can prove the annotation mutates nothing.
        let text_before = message_text_projection(&req);
        let json_before = serde_json::to_string(&req.messages).unwrap();

        let (plan, bust) = annotate_cache_prefix(&req, &cx);

        // The largest stable boundary is the WHOLE history (the rolling
        // last-message breakpoint), and Anthropic warrants an explicit marker.
        assert_eq!(plan.reason, StableReason::MultiTurnHistory);
        assert_eq!(
            plan.boundary_len,
            req.messages.len(),
            "the whole history is the cache-keyed prefix on a multi-turn chat"
        );
        assert!(plan.is_some());
        assert!(
            plan.explicit_breakpoint,
            "Anthropic uses an explicit cache_control breakpoint on the largest stable boundary"
        );
        assert!(plan.prefix_tokens >= 2048);

        // LOSSLESS: the request's message text is unchanged — annotation marks
        // framing, never content. The only thing that "differs" is the returned
        // plan, which lives outside the request bytes. (`annotate_cache_prefix`
        // takes `&req`, so the bytes cannot have moved — we re-project to prove
        // the assertion holds against the same value.)
        assert_eq!(
            message_text_projection(&req),
            text_before,
            "annotation must not rewrite message content"
        );
        assert_eq!(
            serde_json::to_string(&req.messages).unwrap(),
            json_before,
            "the serialized message bytes must be byte-identical after annotation"
        );

        // And it books NO cache bust (see the dedicated test for the contract).
        assert_eq!(bust.busted_prefix_tokens, 0);
    }

    /// The operation returns [`CacheBustEstimate::NONE`]: it annotates
    /// breakpoints, does not mutate prefix CONTENT, so the escape hatch never
    /// engages and no negative entry books. This is the structural defense
    /// against forbidden mode F2.
    #[test]
    fn cache_prefix_does_not_bust_cache() {
        let cx = PassContext {
            provider_id: "anthropic",
            model: "claude-sonnet-4-6",
            pricing: None,
        };
        let long_history = "word ".repeat(3000);
        let req = req_with(vec![
            sys("you are a helpful assistant"),
            user(&long_history),
            assistant("answer"),
            user("follow-up"),
        ]);

        let (_plan, bust) = annotate_cache_prefix(&req, &cx);

        // NONE by identity: source + zero busted tokens + zero priced penalty.
        assert_eq!(bust.source, CacheBustEstimate::NONE.source);
        assert_eq!(bust.busted_prefix_tokens, 0);
        let p = pricing(Some(2048), 3.0, Some(0.3));
        assert_eq!(
            bust.penalty_usd(Some(&p)),
            0.0,
            "annotation prices a zero cache-bust penalty by construction"
        );
    }

    /// A request whose stable region is `BelowMinTokens` (per
    /// [`compute_stable_region`]) gets NO annotation — the plan is a no-op.
    #[test]
    fn cache_prefix_noop_below_min_tokens() {
        let p = pricing(Some(1024), 2.5, Some(1.25));
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-x",
            pricing: Some(&p),
        };
        // A short single-shot prompt — well below the 1024 minimum.
        let mut req = req_with(vec![sys("be brief"), user("question?")]);
        req.model = "gpt-x".into();

        let (plan, bust) = annotate_cache_prefix(&req, &cx);

        assert_eq!(plan.reason, StableReason::BelowMinTokens);
        assert_eq!(plan.boundary_len, 0);
        assert!(!plan.is_some(), "no qualifying prefix → no annotation");
        assert!(!plan.explicit_breakpoint);
        assert_eq!(plan.prefix_tokens, 0);
        // And still no bust.
        assert_eq!(bust.busted_prefix_tokens, 0);
    }

    /// Positional-auto-cache providers (OpenAI): a cache-qualified prefix is
    /// annotated, but with NO explicit breakpoint — the provider auto-caches
    /// the prefix positionally, the plan only asserts byte-stability.
    #[test]
    fn cache_prefix_positional_provider_no_explicit_breakpoint() {
        let p = pricing(Some(1024), 2.5, Some(1.25));
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-x",
            pricing: Some(&p),
        };
        let long_system = "word ".repeat(1500);
        let mut req = req_with(vec![sys(&long_system), user("question?")]);
        req.model = "gpt-x".into();

        let (plan, bust) = annotate_cache_prefix(&req, &cx);

        assert_eq!(plan.reason, StableReason::AutoCachedPrefix);
        assert!(plan.is_some());
        assert!(
            !plan.explicit_breakpoint,
            "positional auto-cache providers insert no breakpoint"
        );
        assert_eq!(bust.busted_prefix_tokens, 0);
    }
}
