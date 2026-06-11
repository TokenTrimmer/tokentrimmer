//! Cache-aware request split — the TYPE-LEVEL invariant that a [`RequestPass`]
//! cannot mutate the cache-stable prefix of a request.
//!
//! [`SplitRequest::compute`] divides a request's messages into:
//!
//! - a **stable prefix** — everything the provider's prompt cache keys on
//!   (what the Anthropic adapter marks with `cache_control`, or the prefix
//!   OpenAI auto-caches). Exposed to passes as a read-only [`StablePrefix`].
//! - a **volatile tail** — the only region passes may mutate, exposed as a
//!   [`VolatileTail`] with tail-relative slice/remove operations only.
//!
//! Both fields of [`SplitRequest`] are private and there is no public path to
//! a `&mut ChatCompletionRequest` except [`SplitRequest::mutate_whole_request`]
//! — the deliberate escape hatch, which consumes the split and returns a
//! `#[must_use]` [`CacheBustEstimate`] that the caller MUST book as a negative
//! savings entry. See the [`crate::passes`] module docs for the full invariant
//! and its rationale.
//!
//! # The split rule
//!
//! `min` = the model's `prompt_cache_min_tokens` (see [`cache_min_tokens`]):
//!
//! - **No `min`** (model/provider not cache-capable — every test/local
//!   provider) → the whole request is volatile ([`StableReason::NotCacheCapable`]);
//!   passes behave exactly as they did before the split existed.
//! - **Multi-turn** (any assistant message present) and the estimated tokens of
//!   the WHOLE message list ≥ `min` → the ENTIRE message list is stable
//!   ([`StableReason::MultiTurnHistory`]). This is deliberately WIDER than
//!   "all-but-recent history": this turn's fresh tail becomes next turn's
//!   cached prefix (Anthropic's rolling breakpoint per #150 marks the *last*
//!   message), so mutating the tail forfeits the incremental cache write — the
//!   client re-sends the raw tail next turn while the provider cached the
//!   transformed bytes, re-billing it at 1.0x + the 1.25x write premium. A
//!   whitespace trim can never buy that back (the −145% per-turn re-selection
//!   trap in miniature).
//! - **Single-shot** (no assistant message) → the stable region is the system
//!   prefix `messages[..=last_system_idx]` when its estimated tokens ≥ `min`
//!   ([`StableReason::SystemPrefix`]); the user/tool tail stays mutable because
//!   no warm per-conversation prefix exists yet, and deterministic trims
//!   preserve cross-request OpenAI prefix sharing. (Covering up to the *last*
//!   system message also protects any out-of-order system block the Anthropic
//!   adapter would hoist into the cached system prefix.)
//! - Otherwise → nothing qualifies for caching, the whole request is volatile
//!   ([`StableReason::BelowMinTokens`]).
//!
//! Token estimation uses [`tt_tokenize::estimate_tokens`] with the route's
//! FINAL served provider id — the same estimator routing, preview, and savings
//! attribution use — never characters.
//!
//! [`RequestPass`]: crate::passes::RequestPass

use tt_shared::messages::Message;
use tt_shared::pricing::ModelPricing;
use tt_shared::ChatCompletionRequest;

use super::messages_text;

/// Everything a pass may consult but never mutate: the served provider id (the
/// tokenizer key), the served model, and its catalog pricing.
pub struct PassContext<'a> {
    /// The FINAL served provider's id — the tokenizer key for every token
    /// count, so estimates match what the upstream will bill.
    pub provider_id: &'a str,
    /// The served model id (post-routing/pin).
    pub model: &'a str,
    /// The served model's catalog pricing. `None` for unpriced models; test
    /// and local providers carry no `prompt_cache_min_tokens`, which keeps
    /// them on the all-volatile path.
    pub pricing: Option<&'a ModelPricing>,
}

/// Why a region is protected (or not) — for logs and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StableReason {
    /// The model/provider has no prompt-cache minimum — nothing can cache, the
    /// stable prefix is empty.
    NotCacheCapable,
    /// A minimum exists but the candidate prefix is below it — the provider
    /// would silently refuse to cache it, the stable prefix is empty.
    BelowMinTokens,
    /// Single-shot request with a cache-qualified system prefix: the system
    /// messages are stable, the rest of the request is the volatile tail.
    SystemPrefix,
    /// Cache-qualified multi-turn conversation: the entire message list is
    /// stable (this turn's tail is next turn's cached prefix).
    MultiTurnHistory,
}

/// Read-only view of the cache-stable region. Passes can inspect it (e.g. to
/// classify), but there is no mutable access — by type, not by discipline.
pub struct StablePrefix<'a> {
    /// The protected messages (`messages[..stable_len]`).
    pub messages: &'a [Message],
    /// Estimated tokens of the protected region, counted with the served
    /// provider's tokenizer at split time (pre-mutation bytes).
    pub est_tokens: u32,
    /// Why this region is (or is not) protected.
    pub reason: StableReason,
}

/// The ONLY thing passes can mutate: the messages after the stable boundary.
///
/// Internally the tail is detached from the request for the duration of a pass
/// (see [`SplitRequest::run_pass`]), so this wraps a `Vec` the pass may edit
/// in place or shrink — it exposes no way to reach the stable prefix, the
/// model, `tools`, or any non-message field.
pub struct VolatileTail<'a> {
    msgs: &'a mut Vec<Message>,
}

impl VolatileTail<'_> {
    /// Number of messages in the volatile tail.
    #[must_use]
    pub fn len(&self) -> usize {
        self.msgs.len()
    }

    /// True when the tail has no messages (e.g. a cache-qualified multi-turn
    /// request, where the whole message list is stable).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.msgs.is_empty()
    }

    /// Read-only view of the tail messages (tail-relative indexing).
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        self.msgs
    }

    /// Mutable view of the tail messages for content-in-place mutation
    /// (tail-relative indexing).
    pub fn messages_mut(&mut self) -> &mut [Message] {
        self.msgs
    }

    /// Remove and return the tail message at `tail_idx` (tail-relative) — used
    /// by de-duplication.
    pub fn remove(&mut self, tail_idx: usize) -> Message {
        self.msgs.remove(tail_idx)
    }
}

/// The model's prompt-cache minimum prefix length, in tokens, or `None` when
/// the model cannot cache.
///
/// - `anthropic` → the shared catalog lookup (exact id, then longest-prefix
///   for dated wire ids) with the same conservative 4096-token fallback the
///   adapter's `cache_control` injection gate uses
///   (`tt-provider-anthropic::translate::cache_min_tokens_for_model`) — both
///   delegate to [`tt_shared::pricing::PricingCatalog::prompt_cache_min_tokens`]
///   so they can never disagree. Always `Some` for Anthropic: every current
///   Anthropic model caches.
/// - everything else → the catalog row's `prompt_cache_min_tokens` (OpenAI
///   gpt-5.x = 1024; test/local providers = `None`, which keeps every hermetic
///   test on the unchanged all-volatile path).
pub(crate) fn cache_min_tokens(cx: &PassContext<'_>) -> Option<u32> {
    /// Mirror of `tt-provider-anthropic`'s conservative fallback: the largest
    /// documented Anthropic minimum, so an unknown model is never assumed to
    /// cache a prefix shorter than its real (unknown) minimum.
    const ANTHROPIC_FALLBACK_CACHE_MIN_TOKENS: u32 = 4096;

    if cx.provider_id == "anthropic" {
        Some(
            tt_shared::pricing::catalog()
                .prompt_cache_min_tokens("anthropic", cx.model)
                .unwrap_or(ANTHROPIC_FALLBACK_CACHE_MIN_TOKENS),
        )
    } else {
        cx.pricing.and_then(|p| p.prompt_cache_min_tokens)
    }
}

/// A request split into a read-only stable prefix and a mutable volatile tail.
///
/// Fields are private: there is no public `&mut ChatCompletionRequest` path
/// except [`Self::mutate_whole_request`].
pub struct SplitRequest<'r> {
    req: &'r mut ChatCompletionRequest,
    stable_len: usize,
    stable_tokens: u32,
    reason: StableReason,
}

impl<'r> SplitRequest<'r> {
    /// Compute the split for `req` under `cx` — see the module docs for the
    /// rule. The boundary and the prefix token estimate are derived from the
    /// CURRENT (pre-mutation) bytes: that is what the provider's cache keyed
    /// on, so that is what a later bust re-prices.
    pub fn compute(req: &'r mut ChatCompletionRequest, cx: &PassContext<'_>) -> Self {
        let (stable_len, stable_tokens, reason) = match cache_min_tokens(cx) {
            None => (0, 0, StableReason::NotCacheCapable),
            Some(min) => {
                let multi_turn = req
                    .messages
                    .iter()
                    .any(|m| matches!(m, Message::Assistant { .. }));
                if multi_turn {
                    let est =
                        tt_tokenize::estimate_tokens(cx.provider_id, &messages_text(&req.messages));
                    if est >= min {
                        (req.messages.len(), est, StableReason::MultiTurnHistory)
                    } else {
                        (0, 0, StableReason::BelowMinTokens)
                    }
                } else {
                    let stable_len = req
                        .messages
                        .iter()
                        .rposition(|m| matches!(m, Message::System { .. }))
                        .map_or(0, |i| i + 1);
                    let est = tt_tokenize::estimate_tokens(
                        cx.provider_id,
                        &messages_text(&req.messages[..stable_len]),
                    );
                    if stable_len > 0 && est >= min {
                        (stable_len, est, StableReason::SystemPrefix)
                    } else {
                        (0, 0, StableReason::BelowMinTokens)
                    }
                }
            }
        };
        Self {
            req,
            stable_len,
            stable_tokens,
            reason,
        }
    }

    /// Read-only view of the stable prefix.
    #[must_use]
    pub fn stable(&self) -> StablePrefix<'_> {
        StablePrefix {
            messages: &self.req.messages[..self.stable_len],
            est_tokens: self.stable_tokens,
            reason: self.reason,
        }
    }

    /// Run `f` with disjoint borrows of the stable prefix (read-only) and the
    /// volatile tail (mutable) — the pipeline-internal way a pass is applied.
    ///
    /// Implementation note: the tail is `split_off` the request for the
    /// duration of `f` and re-appended after, which is what lets the borrow
    /// checker prove the pass cannot reach the prefix through the tail handle.
    /// (If `f` panics the request is left truncated, but a panicking pass
    /// already aborts the request — nothing is dispatched from that state.)
    pub(crate) fn run_pass<R>(
        &mut self,
        f: impl FnOnce(&StablePrefix<'_>, &mut VolatileTail<'_>) -> R,
    ) -> R {
        let mut tail_vec = self.req.messages.split_off(self.stable_len);
        let stable = StablePrefix {
            messages: &self.req.messages,
            est_tokens: self.stable_tokens,
            reason: self.reason,
        };
        let mut tail = VolatileTail {
            msgs: &mut tail_vec,
        };
        let r = f(&stable, &mut tail);
        self.req.messages.append(&mut tail_vec);
        r
    }

    /// The concatenated text of the volatile tail — what the token-true gate
    /// counts before/after each pass (the stable prefix is immutable, so its
    /// count cancels out of the delta).
    pub(crate) fn tail_text(&self) -> String {
        messages_text(&self.req.messages[self.stable_len..])
    }

    /// Clone the volatile tail so the gate can restore it byte-identical when
    /// a pass inflates the request.
    pub(crate) fn tail_snapshot(&self) -> Vec<Message> {
        self.req.messages[self.stable_len..].to_vec()
    }

    /// Restore the volatile tail from a [`Self::tail_snapshot`] — the gate's
    /// fail-open path (discard the transform, keep the original bytes).
    pub(crate) fn restore_tail(&mut self, snap: Vec<Message>) {
        self.req.messages.truncate(self.stable_len);
        self.req.messages.extend(snap);
    }

    /// **The escape hatch** — the ONLY way to reach the whole request
    /// mutably, stable prefix included.
    ///
    /// Consumes the split and returns the raw `&mut ChatCompletionRequest`
    /// plus a [`CacheBustEstimate`] for the WHOLE pre-mutation stable prefix
    /// (conservative: any mutation inside the prefix busts the exact-prefix
    /// match from byte 0 of the change onward). The estimate is `#[must_use]`:
    /// the caller MUST either book it into the savings-attribution channel
    /// (when the mutation actually touched the prefix) or explicitly
    /// [`CacheBustEstimate::discard_unused`] it (when it did not). Never hide
    /// a cache bust.
    ///
    /// `source` is the stable identifier booked with the negative entry
    /// (e.g. `"redaction-1"`).
    pub fn mutate_whole_request(
        self,
        source: &'static str,
    ) -> (&'r mut ChatCompletionRequest, CacheBustEstimate) {
        (
            self.req,
            CacheBustEstimate {
                source,
                busted_prefix_tokens: self.stable_tokens,
            },
        )
    }
}

/// A negative savings entry for a deliberate stable-prefix mutation: the
/// estimated cost of re-billing a cache-warm prefix at the full input rate
/// (~0.1x cache reads repriced to 1.0x) because its bytes changed.
///
/// This is an ESTIMATE of induced future cost — it reduces the TT-attributed
/// savings headline (pre-clamp) but is never folded into `cost_usd` /
/// `baseline_cost_usd`, which must reconcile against the realized provider
/// invoice.
#[must_use = "a stable-prefix mutation MUST be booked into the savings channel \
              (or explicitly discarded via discard_unused) — never hide a cache bust"]
#[derive(Debug, Clone, Copy)]
pub struct CacheBustEstimate {
    /// Stable identifier of the mutator (e.g. `"redaction-1"`) — carried into
    /// the `cache_bust:<source>` warning token and metrics label.
    pub source: &'static str,
    /// Estimated tokens of the busted (pre-mutation) stable prefix.
    pub busted_prefix_tokens: u32,
}

impl CacheBustEstimate {
    /// The zero entry — no stable prefix was mutated.
    pub const NONE: CacheBustEstimate = CacheBustEstimate {
        source: "none",
        busted_prefix_tokens: 0,
    };

    /// The estimated USD penalty: the warm prefix repriced from the cache-read
    /// rate back to the full input rate —
    /// `tokens × (input_per_million − cached_input_per_million) / 1e6`,
    /// floored at 0. Returns 0 when pricing or the cached rate is absent:
    /// with no documented cache-read discount nothing was cacheable, so there
    /// is no bust to book.
    #[must_use]
    pub fn penalty_usd(&self, pricing: Option<&ModelPricing>) -> f64 {
        let Some(p) = pricing else { return 0.0 };
        let Some(cached) = p.cached_input_per_million else {
            return 0.0;
        };
        ((self.busted_prefix_tokens as f64) * (p.input_per_million - cached) / 1_000_000.0).max(0.0)
    }

    /// Explicitly drop an estimate that turned out not to apply (the mutation
    /// never touched the stable prefix, or never fired at all). Exists so the
    /// no-op path is a deliberate, greppable decision rather than a silently
    /// ignored `#[must_use]` value.
    pub fn discard_unused(self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tt_shared::messages::MessageContent;

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
            model: "gpt-4o".into(),
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

    /// No pricing (or no documented minimum) → the whole request is volatile.
    /// This pins the no-regression path every existing route/test relies on.
    #[test]
    fn split_not_cache_capable_is_all_volatile() {
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-4o",
            pricing: None,
        };
        let mut r = req_with(vec![sys("a"), user("b"), assistant("c")]);
        let split = SplitRequest::compute(&mut r, &cx);
        let stable = split.stable();
        assert!(stable.messages.is_empty());
        assert_eq!(stable.reason, StableReason::NotCacheCapable);

        // Same when pricing exists but carries no minimum (test providers).
        let p = pricing(None, 1.0, None);
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-4o",
            pricing: Some(&p),
        };
        let mut r = req_with(vec![sys("a"), user("b"), assistant("c")]);
        let split = SplitRequest::compute(&mut r, &cx);
        assert_eq!(split.stable().reason, StableReason::NotCacheCapable);
        assert!(split.stable().messages.is_empty());
    }

    /// Single-shot with a long system prefix (≥ min) → system messages stable,
    /// user tail volatile.
    #[test]
    fn split_single_shot_long_system_protects_system_only() {
        let p = pricing(Some(1024), 2.5, Some(1.25));
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-x",
            pricing: Some(&p),
        };
        // ~1500 cl100k tokens of system text — comfortably over the 1024 min.
        let long_system = "word ".repeat(1500);
        let mut r = req_with(vec![sys(&long_system), user("question?")]);
        let mut split = SplitRequest::compute(&mut r, &cx);
        {
            let stable = split.stable();
            assert_eq!(stable.reason, StableReason::SystemPrefix);
            assert_eq!(stable.messages.len(), 1);
            assert!(stable.est_tokens >= 1024);
        }
        split.run_pass(|stable, tail| {
            assert_eq!(stable.messages.len(), 1);
            assert_eq!(tail.len(), 1);
            assert!(matches!(tail.messages()[0], Message::User { .. }));
        });
    }

    /// Single-shot with a tiny system prefix (< min) → nothing would cache, the
    /// whole request stays volatile.
    #[test]
    fn split_single_shot_short_system_below_min_all_volatile() {
        let p = pricing(Some(1024), 2.5, Some(1.25));
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-x",
            pricing: Some(&p),
        };
        let mut r = req_with(vec![sys("be brief"), user("question?")]);
        let split = SplitRequest::compute(&mut r, &cx);
        assert_eq!(split.stable().reason, StableReason::BelowMinTokens);
        assert!(split.stable().messages.is_empty());
    }

    /// Cache-qualified multi-turn → the ENTIRE message list is stable and the
    /// tail is empty (this turn's tail is next turn's cached prefix).
    #[test]
    fn split_multi_turn_qualified_protects_everything() {
        let p = pricing(Some(8), 2.5, Some(1.25));
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-x",
            pricing: Some(&p),
        };
        let mut r = req_with(vec![
            sys("you are a helpful assistant, be concise"),
            user("first question about the weather"),
            assistant("the weather answer with some detail"),
            user("follow-up question"),
        ]);
        let mut split = SplitRequest::compute(&mut r, &cx);
        {
            let stable = split.stable();
            assert_eq!(stable.reason, StableReason::MultiTurnHistory);
            assert_eq!(stable.messages.len(), 4);
        }
        split.run_pass(|stable, tail| {
            assert_eq!(stable.messages.len(), 4);
            assert!(tail.is_empty(), "no message is mutable on this path");
        });
    }

    /// Anthropic provider with a model that is NOT in the catalog → the
    /// conservative 4096 fallback governs qualification, exactly like the
    /// adapter's injection gate.
    #[test]
    fn split_anthropic_unknown_model_uses_4096_fallback() {
        let cx = PassContext {
            provider_id: "anthropic",
            model: "claude-nonexistent-99",
            pricing: None,
        };
        // Short system: below the 4096 fallback → BelowMinTokens, NOT
        // NotCacheCapable (Anthropic is always cache-capable).
        let mut r = req_with(vec![sys("short"), user("q")]);
        let split = SplitRequest::compute(&mut r, &cx);
        assert_eq!(split.stable().reason, StableReason::BelowMinTokens);

        // ~5000 cl100k tokens of system text clears the 4096 fallback.
        let long_system = "word ".repeat(5000);
        let mut r = req_with(vec![sys(&long_system), user("q")]);
        let split = SplitRequest::compute(&mut r, &cx);
        assert_eq!(split.stable().reason, StableReason::SystemPrefix);
        assert_eq!(split.stable().messages.len(), 1);
    }

    /// A known Anthropic model resolves its catalog minimum (Sonnet 4.6 =
    /// 2048), including via a dated wire id.
    #[test]
    fn split_anthropic_known_model_uses_catalog_min() {
        let cx = PassContext {
            provider_id: "anthropic",
            model: "claude-sonnet-4-6-20260101",
            pricing: None,
        };
        // ~3000 tokens: above Sonnet's 2048 but below the 4096 fallback — so
        // this qualifying ONLY proves the catalog minimum was used.
        let long_system = "word ".repeat(3000);
        let mut r = req_with(vec![sys(&long_system), user("q")]);
        let split = SplitRequest::compute(&mut r, &cx);
        assert_eq!(split.stable().reason, StableReason::SystemPrefix);
    }

    /// The escape hatch returns the whole request mutably plus a bust estimate
    /// for the pre-mutation prefix; `penalty_usd` is the cache-read→full-input
    /// repricing and is 0 without a cached rate.
    #[test]
    fn escape_hatch_returns_must_use_bust_and_penalty_math() {
        let p = pricing(Some(8), 10.0, Some(1.0));
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-x",
            pricing: Some(&p),
        };
        let mut r = req_with(vec![
            sys("you are a helpful assistant, be concise"),
            user("question"),
            assistant("answer"),
        ]);
        let split = SplitRequest::compute(&mut r, &cx);
        let stable_tokens = split.stable().est_tokens;
        assert!(stable_tokens > 0);

        let (req_mut, bust) = split.mutate_whole_request("redaction-1");
        // Whole-request mutable access — including the stable prefix.
        req_mut.messages[0] = sys("MUTATED");
        assert_eq!(bust.source, "redaction-1");
        assert_eq!(bust.busted_prefix_tokens, stable_tokens);

        // penalty = tokens × (input − cached) / 1e6.
        let expected = (stable_tokens as f64) * (10.0 - 1.0) / 1_000_000.0;
        assert!((bust.penalty_usd(Some(&p)) - expected).abs() < 1e-12);

        // No pricing / no cached rate → nothing was cacheable → no bust.
        assert_eq!(bust.penalty_usd(None), 0.0);
        let no_cached = pricing(Some(8), 10.0, None);
        assert_eq!(bust.penalty_usd(Some(&no_cached)), 0.0);

        // A cached rate above the input rate (pathological catalog row) floors
        // at zero rather than booking a negative penalty.
        let inverted = pricing(Some(8), 1.0, Some(2.0));
        assert_eq!(bust.penalty_usd(Some(&inverted)), 0.0);
    }

    /// `NONE` books nothing under any pricing.
    #[test]
    fn none_estimate_is_zero() {
        let p = pricing(Some(8), 10.0, Some(1.0));
        assert_eq!(CacheBustEstimate::NONE.penalty_usd(Some(&p)), 0.0);
        CacheBustEstimate::NONE.discard_unused();
    }

    /// Snapshot/restore brings the tail back byte-identical (the gate's
    /// fail-open path) while the stable prefix is untouched throughout.
    #[test]
    fn tail_snapshot_restore_roundtrip() {
        let p = pricing(Some(4), 1.0, Some(0.1));
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-x",
            pricing: Some(&p),
        };
        let mut r = req_with(vec![
            sys("a long enough system prefix for the tiny minimum"),
            user("tail message"),
        ]);
        let before = serde_json::to_string(&r).unwrap();
        let mut split = SplitRequest::compute(&mut r, &cx);
        assert_eq!(split.stable().reason, StableReason::SystemPrefix);

        let snap = split.tail_snapshot();
        split.run_pass(|_stable, tail| {
            // Vandalize the tail…
            tail.messages_mut()[0] = Message::User {
                content: MessageContent::Text("VANDALIZED".into()),
                name: None,
            };
        });
        assert_ne!(serde_json::to_string(&r).unwrap(), before);

        let mut split = SplitRequest::compute(&mut r, &cx);
        split.restore_tail(snap);
        // …and the restore brings the request back byte-identical.
        assert_eq!(serde_json::to_string(&r).unwrap(), before);
    }
}
