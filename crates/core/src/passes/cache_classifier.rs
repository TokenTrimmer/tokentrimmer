//! Cache classifier — an ACTIVE, lossless, diagnostics-only promotion of the
//! Tier-1 inspect lints `prompt-dynamic-prefix-breaks-cache`,
//! `prompt-volatile-in-system-prompt`, and
//! `cache-anthropic-prompt-cache-missing` into the request path.
//!
//! Classifies the request's would-be-stable region (the system prefix the
//! Anthropic adapter would mark with `cache_control`, or the prefix OpenAI
//! auto-caches) as CACHEABLE-STABLE vs VOLATILE: a timestamp / UUID / long hex
//! token rendered INTO the system prompt changes the cached bytes every call,
//! so the provider prompt cache busts on every request and the whole prefix
//! re-bills at full input price — a silent cost leak the static lints can only
//! see in source code, while this classifier sees it in live request bytes.
//!
//! The qualifying region is [`compute_stable_region`] — the SAME rule the
//! split and the adapters' injection gates use. In particular a SHORT system
//! prompt on a cache-qualified multi-turn conversation still fires: per #150
//! the Anthropic adapter injects the rolling last-message breakpoint when
//! system + history clears the minimum, so a per-request timestamp in a
//! 500-token system prompt of a 4000-token conversation busts the ENTIRE
//! conversation cache every call — the costliest real-world shape, and
//! exactly what the per-request waste figure quantifies (the FULL stable
//! prefix, not just the system tokens).
//!
//! # Conservative scope (deliberate)
//!
//! - **NEVER mutates.** The pass is read-only; it emits
//!   `cache_dynamic_prefix:<kind>` warning tokens (the `x-tokentrimmer-warnings`
//!   diagnostics channel), a `tracing::warn`, and Prometheus counters. No
//!   reordering — the research record shows per-turn content re-selection at
//!   **−145%**; moving "volatile" content is a semantic risk this pass does
//!   not take.
//! - **NEVER injects `cache_control`.** Auto-marking is owned by the Anthropic
//!   adapter (#126/#150, `tt-provider-anthropic::translate`); a second
//!   injection site would double-mark.
//! - **Does NOT suppress the adapter's injection either** (documented
//!   non-goal): skipping `cache_control` on a volatile-looking system prompt
//!   would save the 1.25x write premium, but a single-request heuristic cannot
//!   distinguish a per-call UUID from a STABLE one (a fixed tenant/config id
//!   in the prompt) — wrongly disabling caching there forfeits the ~0.1x read
//!   rate, which costs far more than the premium saves. Wiring suppression
//!   needs cross-request prefix-hash evidence (same breadcrumb in
//!   `translate.rs`).
//!
//! Only the LITERAL detectors port from the lints (ISO date-time, UUID,
//! ≥32-char hex token): the f-string/`${…}` interpolation regexes are
//! source-code constructs that never appear in rendered request bytes, and
//! bare dates are excluded for the same false-positive rationale as the lint
//! (a date in prose content is routinely stable).
//!
//! # Quantification
//!
//! Per request, the estimated waste of the busted prefix is
//! `stable_prefix_tokens × (input − cached_input) / 1e6` USD — the WHOLE
//! would-be-cached prefix (system + history on multi-turn), because an
//! exact-prefix match fails from the first changed byte — accumulated into
//! the `cache_dynamic_prefix_waste_microusd_total` counter (plus
//! `cache_dynamic_prefix_total{kind}`). The handler cannot know per-route
//! volume, so the *monthly* cost is a dashboard aggregation (30d rate over the
//! counter), not a per-request figure. NOTE: both series meter in the pass
//! stage, BEFORE the L1/L2 lookups — a request later served from
//! TokenTrimmer's own cache still counts (see `crate::metrics` docs).
//!
//! Default-on: invoked directly by the chat handler on EVERY request (mirrors
//! the `RedactionPass::redact` direct-call precedent) — allowed because it is
//! observability-only and changes no request/response semantics. Per-request
//! logging is at DEBUG (a volatile prefix is volatile on every call — WARN
//! would spam the hot path indefinitely); the persistent signals are the
//! metrics and the `cache_dynamic_prefix:<kind>` warning tokens.

use std::sync::OnceLock;

use regex::Regex;

use tt_shared::ChatCompletionRequest;

use super::split::{compute_stable_region, PassContext, StablePrefix, VolatileTail};
use super::{system_text, PassOutcome, RequestPass};

/// The diagnostics-only stable/volatile classifier pass.
#[derive(Debug, Clone, Copy, Default)]
pub struct CacheClassifierPass;

/// A literal ISO-8601 *date-time* (date plus a time component). The time part
/// is required so a bare date in prose does not fire — ported verbatim from
/// `prompt_volatile_in_system_prompt.rs`. `pub(crate)` so the L2
/// volatility-class TTL ([`crate::cache_volatility`]) reuses the exact same
/// detector instead of forking it.
pub(crate) fn literal_iso_timestamp_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}").expect("iso-timestamp regex is valid")
    })
}

/// A literal UUID (8-4-4-4-12 hex), case-insensitive — ported verbatim.
fn literal_uuid_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
            .expect("uuid regex is valid")
    })
}

/// A long random **hex** token (≥32 hex chars) — the shape of a generated
/// nonce / session id / request token. The 32-char floor keeps short hex ids
/// and colors out — ported verbatim (see the lint for why base64 is excluded).
fn literal_random_token_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b[0-9a-fA-F]{32,}\b").expect("random-token regex is valid"))
}

/// Every volatile-marker kind present in `text`, deduped by construction.
/// `pub(crate)` for reuse by [`crate::cache_volatility`].
pub(crate) fn volatile_kinds(text: &str) -> Vec<&'static str> {
    let mut kinds = Vec::new();
    if literal_iso_timestamp_regex().is_match(text) {
        kinds.push("timestamp");
    }
    if literal_uuid_regex().is_match(text) {
        kinds.push("uuid");
    }
    if literal_random_token_regex().is_match(text) {
        kinds.push("hex-token");
    }
    kinds
}

/// Classify a qualified stable prefix: scan its SYSTEM text for volatile
/// markers (the conservative scope — a marker in user/tool content is normal
/// payload, not a cached-prefix leak) and, when any fire, meter the estimated
/// per-request waste of busting the WHOLE `prefix_tokens`-sized prefix.
/// Shared by the direct-call API and the pipeline impl; callers must have
/// already established that the region qualifies for caching.
fn classify_qualified_prefix(
    sys_text: &str,
    prefix_tokens: u32,
    cx: &PassContext<'_>,
) -> Vec<String> {
    let kinds = volatile_kinds(sys_text);
    if kinds.is_empty() {
        return Vec::new();
    }

    // Estimated per-request waste: the whole would-be-cached prefix re-bills
    // at the full input rate instead of the cached rate, every call (the
    // exact-prefix match fails from the first changed byte). 0 when the model
    // documents no cache-read discount.
    let est_wasted_usd = match cx.pricing.and_then(|p| {
        p.cached_input_per_million
            .map(|cached| (p.input_per_million - cached).max(0.0))
    }) {
        Some(delta_rate) => (prefix_tokens as f64) * delta_rate / 1_000_000.0,
        None => 0.0,
    };

    for (i, kind) in kinds.iter().enumerate() {
        // DEBUG, not WARN: a volatile prefix fires on every call of that
        // route — the metrics + warning tokens carry the persistent signal.
        tracing::debug!(
            kind,
            prefix_tokens,
            est_wasted_usd_per_request = est_wasted_usd,
            "volatile marker inside would-be-stable prefix — provider prompt cache \
             busts every call"
        );
        // Book the per-request USD waste exactly once (with the first kind) so
        // the waste counter is never double-counted when several kinds fire on
        // one request; the per-kind counter still increments for each.
        crate::metrics::record_cache_dynamic_prefix(
            kind,
            if i == 0 { est_wasted_usd } else { 0.0 },
        );
    }

    kinds
        .into_iter()
        .map(|k| format!("cache_dynamic_prefix:{k}"))
        .collect()
}

impl CacheClassifierPass {
    /// Direct-call API (mirrors the `RedactionPass::redact` precedent) so the
    /// chat handler can run the classifier on EVERY request — diagnostics
    /// only, never mutates. Returns warning tokens for the
    /// `x-tokentrimmer-warnings` header, e.g. `cache_dynamic_prefix:uuid`.
    ///
    /// Gates on [`compute_stable_region`] — the same prefix rule the split
    /// and the adapters' injection gates use — so a short system prompt on a
    /// cache-qualified multi-turn conversation is correctly IN scope (the
    /// adapter's rolling breakpoint caches it; a marker there busts the whole
    /// conversation), while a request where nothing would cache stays silent.
    #[must_use]
    pub fn classify(req: &ChatCompletionRequest, cx: &PassContext<'_>) -> Vec<String> {
        let (stable_len, prefix_tokens, _reason) = compute_stable_region(&req.messages, cx);
        if stable_len == 0 {
            return Vec::new();
        }
        classify_qualified_prefix(&system_text(&req.messages[..stable_len]), prefix_tokens, cx)
    }
}

impl RequestPass for CacheClassifierPass {
    fn name(&self) -> &'static str {
        "cache-classifier-1"
    }

    /// Read-only: scans the SYSTEM text of the split's stable prefix (system
    /// messages in the volatile tail are, by definition, not in the cached
    /// prefix), removes nothing, and surfaces the classification as warnings.
    fn apply(
        &self,
        stable: &StablePrefix<'_>,
        _tail: &mut VolatileTail<'_>,
        cx: &PassContext<'_>,
    ) -> PassOutcome {
        let warnings = if stable.messages.is_empty() {
            Vec::new()
        } else {
            classify_qualified_prefix(&system_text(stable.messages), stable.est_tokens, cx)
        };
        PassOutcome {
            tokens_removed: 0,
            warnings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tt_shared::messages::{Message, MessageContent};
    use tt_shared::pricing::ModelPricing;

    const UUID_SYSTEM: &str =
        "Session 550e8400-e29b-41d4-a716-446655440000: you are a helpful, concise assistant.";

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
    fn req_with(messages: Vec<Message>) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: "gpt-x".into(),
            messages,
            ..Default::default()
        }
    }
    fn pricing(min: Option<u32>) -> ModelPricing {
        ModelPricing {
            input_per_million: 10.0,
            output_per_million: 30.0,
            cached_input_per_million: Some(1.0),
            cache_write_per_million: None,
            batch_input_per_million: None,
            batch_output_per_million: None,
            flex_input_per_million: None,
            flex_output_per_million: None,
            prompt_cache_min_tokens: min,
            effective_at: Utc::now(),
        }
    }

    fn classify(req: &ChatCompletionRequest, p: Option<&ModelPricing>) -> Vec<String> {
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-x",
            pricing: p,
        };
        CacheClassifierPass::classify(req, &cx)
    }

    #[test]
    fn classifier_fires_on_uuid_in_long_system() {
        let p = pricing(Some(8));
        let r = req_with(vec![sys(UUID_SYSTEM), user("question")]);
        assert_eq!(
            classify(&r, Some(&p)),
            vec!["cache_dynamic_prefix:uuid".to_string()]
        );
    }

    #[test]
    fn classifier_fires_on_iso_timestamp() {
        let p = pricing(Some(8));
        let r = req_with(vec![sys(
            "Current time: 2026-06-11T09:30:00Z. You are a helpful, concise assistant.",
        )]);
        assert_eq!(
            classify(&r, Some(&p)),
            vec!["cache_dynamic_prefix:timestamp".to_string()]
        );
    }

    #[test]
    fn classifier_fires_on_long_hex_token() {
        let p = pricing(Some(8));
        let token = "a1b2c3d4e5f6a7b8c9d0a1b2c3d4e5f6";
        let r = req_with(vec![sys(&format!(
            "Request token {token}: you are a helpful, concise assistant."
        ))]);
        assert_eq!(
            classify(&r, Some(&p)),
            vec!["cache_dynamic_prefix:hex-token".to_string()]
        );
    }

    #[test]
    fn classifier_silent_below_min_tokens() {
        // Same UUID, but the minimum is far above the prompt size — nothing
        // would cache, so there is no busted cache to report.
        let p = pricing(Some(100_000));
        let r = req_with(vec![sys(UUID_SYSTEM)]);
        assert!(classify(&r, Some(&p)).is_empty());
    }

    #[test]
    fn classifier_silent_on_clean_long_system() {
        let p = pricing(Some(8));
        let r = req_with(vec![sys(
            "You are a helpful, concise assistant. Always answer in plain prose, \
             cite sources, and keep responses under two hundred words.",
        )]);
        assert!(classify(&r, Some(&p)).is_empty());
    }

    /// Conservative scope: a volatile marker in USER content is normal payload
    /// (and a bare date without a time component never fires anywhere).
    #[test]
    fn classifier_silent_on_volatile_marker_in_user_message() {
        let p = pricing(Some(8));
        let r = req_with(vec![
            sys("You are a helpful, concise assistant. Follow all of the rules."),
            user("My meeting id is 550e8400-e29b-41d4-a716-446655440000 on 2026-06-11."),
        ]);
        assert!(classify(&r, Some(&p)).is_empty());
    }

    #[test]
    fn classifier_silent_when_provider_not_cache_capable() {
        // No pricing at all, and pricing with no documented minimum — both are
        // the not-cache-capable shape every test/local provider has.
        let r = req_with(vec![sys(UUID_SYSTEM)]);
        assert!(classify(&r, None).is_empty());
        let p = pricing(None);
        assert!(classify(&r, Some(&p)).is_empty());
    }

    #[test]
    fn classifier_never_mutates_request() {
        let p = pricing(Some(8));
        let r = req_with(vec![sys(UUID_SYSTEM), user("question")]);
        let before = serde_json::to_string(&r).unwrap();
        let _ = classify(&r, Some(&p));
        assert_eq!(serde_json::to_string(&r).unwrap(), before);
    }

    /// Multiple distinct kinds on one request each get a token.
    #[test]
    fn classifier_reports_each_kind_once() {
        let p = pricing(Some(8));
        let r = req_with(vec![sys(
            "Run 550e8400-e29b-41d4-a716-446655440000 at 2026-06-11T09:30 — be helpful.",
        )]);
        assert_eq!(
            classify(&r, Some(&p)),
            vec![
                "cache_dynamic_prefix:timestamp".to_string(),
                "cache_dynamic_prefix:uuid".to_string(),
            ]
        );
    }

    /// THE costliest real-world shape (#150): a volatile marker in a SHORT
    /// system prompt on a cache-qualified MULTI-TURN conversation. The system
    /// alone is below the minimum, but system + history clears it — the
    /// adapter's rolling breakpoint caches the whole conversation, so the
    /// marker busts the entire prefix every call and the classifier must fire.
    #[test]
    fn classifier_fires_on_short_system_in_qualified_multi_turn() {
        // min = 64: the ~20-token system prompt alone is below it; the whole
        // conversation is comfortably above it.
        let p = pricing(Some(64));
        let history_filler = "the previous answer covered the quarterly numbers in detail ";
        let r = req_with(vec![
            sys("Current time: 2026-06-11T09:30:00Z. Be helpful."),
            user(&history_filler.repeat(10)),
            Message::Assistant {
                content: Some(MessageContent::Text(history_filler.repeat(10))),
                tool_calls: vec![],
                name: None,
            },
            user("follow-up question"),
        ]);
        // Sanity: the system text alone is below the minimum.
        let sys_tokens =
            tt_tokenize::estimate_tokens_for_model("openai", "gpt-x", &system_text(&r.messages));
        assert!(
            sys_tokens < 64,
            "system alone must be below min (got {sys_tokens})"
        );
        assert_eq!(
            classify(&r, Some(&p)),
            vec!["cache_dynamic_prefix:timestamp".to_string()],
            "marker in a short system prompt of a qualified conversation must fire"
        );
    }

    /// The RequestPass impl is read-only and surfaces the same warnings.
    #[test]
    fn pipeline_impl_is_read_only_and_warns() {
        use crate::passes::split::SplitRequest;
        let p = pricing(Some(8));
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-x",
            pricing: Some(&p),
        };
        let mut r = req_with(vec![sys(UUID_SYSTEM), user("question")]);
        let before = serde_json::to_string(&r).unwrap();
        let out = {
            let mut split = SplitRequest::compute(&mut r, &cx);
            crate::passes::PassPipeline::new()
                .with(CacheClassifierPass)
                .run(&mut split, &cx)
        };
        assert_eq!(out.tokens_removed, 0);
        assert!(out.rejected.is_empty());
        assert_eq!(out.warnings, vec!["cache_dynamic_prefix:uuid".to_string()]);
        assert_eq!(serde_json::to_string(&r).unwrap(), before);
    }
}
