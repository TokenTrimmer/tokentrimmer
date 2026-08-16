//! Invoice-reconcilable cost and savings accounting for chat dispatches.

use tt_shared::{
    CacheWriteTier, ModelPricing, RequestDeltaEvidenceState, RequestDeltaInput, Usage,
};

use crate::passes::PassEffects;

/// Result of [`compute_cost`]: the canonical cost/savings split.
///
/// Attribution rule (P0 #12 — invoice-reconciliation honesty): the headline
/// `saved_usd` may only contain savings *caused by TokenTrimmer* (routing to a
/// cheaper model, TT L1/L2 cache hits, failover choices). Discounts the
/// provider applies automatically to its own bill — prompt-cache read
/// discounts net of cache-write premiums — are surfaced separately as
/// `provider_cache_saved_usd` so the TT headline survives reconciliation
/// against the provider invoice.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CostBreakdown {
    /// What the provider actually bills (cache discounts included, fee applied).
    /// When the request was served via OpenAI Flex this is the **flex-rate**
    /// cost (~50% of standard).
    pub cost_usd: f64,
    /// What the request would have cost with no TokenTrimmer optimisation:
    /// the originally-requested model at full input price, no cache discount.
    pub baseline_cost_usd: f64,
    /// Provider-side automatic cache discount: served-model cost with no
    /// caching minus actual cost, clamped at 0 (a cache-write premium can make
    /// the cached request *more* expensive; we never report negative savings).
    pub provider_cache_saved_usd: f64,
    /// Savings attributed to the OpenAI **Flex** service tier specifically — the
    /// difference between the synchronous (standard) baseline cost and the flex
    /// cost for this token usage, at the served model. A distinct savings source
    /// from routing/cache so the headline + methodology can name it. Zero when
    /// the request was not served via flex. Already included in
    /// [`tt_saved_usd`](Self::tt_saved_usd) (flex lowers `cost_usd`).
    pub flex_saved_usd: f64,
    /// Savings attributed to the conservative **compression pass** specifically
    /// — the cost of the input tokens the pass removed before dispatch, priced
    /// at the served model's input rate (fee-applied). A distinct savings source
    /// from routing/cache/flex so the headline + methodology can name it. Zero
    /// when the request was not compressed. Already included in
    /// [`tt_saved_usd`](Self::tt_saved_usd): the removed tokens raise
    /// `baseline_cost_usd` (priced on the pre-compression prompt count) above the
    /// realized `cost_usd` (priced on the reduced count), so the baseline − cost
    /// delta picks the compression saving up. Catalog-priced like every other
    /// source; consistent with the provider-cache-vs-TT attribution rules (this
    /// is a genuine TT-caused reduction in billed input tokens, not a provider
    /// discount, so it belongs in the TT headline).
    pub compression_saved_usd: f64,
    /// Savings attributed to the lossless **document-compaction pass**
    /// specifically (Document Lane D2) — the cost of the input tokens the pass
    /// removed from LARGE non-prose documents before dispatch, priced at the
    /// served model's input rate (fee-applied). A distinct savings source from
    /// compression so the headline + methodology can name it. Zero when the
    /// route did not opt into `doc_compaction`. Already included in
    /// [`tt_saved_usd`](Self::tt_saved_usd) via the SAME baseline fold as
    /// `compression_saved_usd`: the removed tokens raise `baseline_cost_usd`
    /// above the realized `cost_usd`, so the `baseline − cost` delta picks the
    /// doc-compaction saving up. A genuine TT-caused reduction in billed input
    /// tokens (text-only, token-true-gated), not a provider discount.
    pub doc_compaction_saved_usd: f64,
    /// NEGATIVE savings entry: the estimated cost induced by a deliberate
    /// NON-deterministic stable-prefix mutation (a booked
    /// `CacheBustEstimate`; no shipped transform books one today — redaction
    /// is ingress-deterministic and busts nothing) — the prefix tokens
    /// repriced from the ~0.1x cache-read rate back to the full input rate,
    /// fee-applied. Zero on every request whose stable prefix was untouched.
    /// It REDUCES [`tt_saved_usd`](Self::tt_saved_usd) pre-clamp
    /// (conservative in TT's disfavor, same precedent as the cache-write
    /// premium) but is NEVER folded into `cost_usd` / `baseline_cost_usd`:
    /// it is an estimate of induced FUTURE cost, and those two fields must
    /// reconcile against the realized provider invoice. Persisted on the
    /// `request_logs` row (migration 0016) so the row-derived ledger agrees
    /// with the header/span headline.
    pub cache_bust_penalty_usd: f64,
    /// NEGATIVE savings entry: the REAL auxiliary-LLM spend of the agentic
    /// budget's summarizer calls (Sub-lever 2b), fee-applied. Aux spend is
    /// taxed, never free (spec §4.4 item 3) — so it REDUCES
    /// [`tt_saved_usd`](Self::tt_saved_usd) pre-clamp (the loop win is honestly
    /// net-of-tax, the cache-bust precedent) but is NEVER folded into
    /// `cost_usd` / `baseline_cost_usd`: those reconcile against the realized
    /// provider invoice, and the summarizer call bills the org on its OWN
    /// credentials (it is not part of THIS request's served dispatch). Surfaced
    /// on its own `X-TokenTrimmer-Summarizer-Tax-Usd` header. 0.0 on every
    /// request that ran no summarizer (all default-path traffic). An UNMETERED
    /// summarizer call (no catalog price / timed-out-but-possibly-billed) is
    /// NEVER coerced to a phantom `0.0` here — the wiring (Task 9) surfaces it
    /// as an honest warning rather than booking unknown spend as free.
    pub summarizer_tax_usd: f64,
    /// FORGONE batch discount (USD): what the async Batch Lane would have
    /// saved on this request — realized cost minus the served model's
    /// batch-rate cost on the full prompt+completion, floored at 0, fee-
    /// applied. ADVISORY: the gateway dispatched synchronously and billed
    /// `cost_usd`; this is NEVER included in `tt_saved_usd()` or `saved_usd`
    /// (nothing was actually saved — the savings-ledger headline must stay
    /// invoice-reconcilable). Surfaced on its own
    /// `X-TokenTrimmer-Batch-Forgone-Usd` header and persisted on the
    /// `request_logs` row (migration 0017). 0.0 unless the request was marked
    /// batch-eligible AND the served model carries catalog batch rates.
    pub batch_forgone_usd: f64,
    /// ESTIMATED saving from minified-JSON output steering
    /// (`RouteAction::minify_json`, research Phase 3.1): the pretty-printed
    /// re-rendering of the emitted JSON, re-tokenized with the served model's
    /// tokenizer, minus the tokens actually emitted — priced at the output
    /// rate the request was actually billed at (flex out-rate when flex
    /// applied, else standard), fee-applied. An ESTIMATE of an unmeasurable
    /// counterfactual (the model might have emitted minified JSON anyway):
    /// NEVER included in [`tt_saved_usd`](Self::tt_saved_usd) / `saved-usd`
    /// and never folded into `cost_usd` / `baseline_cost_usd` (those
    /// reconcile against the invoice). Surfaced on its own
    /// `X-TokenTrimmer-Minify-Saved-Est-Usd` header and `request_logs` column
    /// (migration 0020). 0.0 when the instruction was not injected, when the
    /// response is not valid JSON, and on streaming (estimate not computed in
    /// v1 — metered only).
    pub minify_saved_est_usd: f64,
    /// MEASURED diff-lane saving (research Phase 3.4): the output tokens the
    /// applied patch avoided billing (tokenized reconstructed artifact −
    /// billed patch completion tokens) priced at the served model's output
    /// rate, fee-applied. Both sides are real tokenizer counts on real
    /// strings — the brief's "genuinely measurable" case — so it rides the
    /// [`tt_saved_usd`](Self::tt_saved_usd) headline via the baseline fold
    /// (the compression precedent) AND is isolated here for the methodology
    /// breakdown. Zero when no diff applied.
    pub diff_saved_usd: f64,
    /// ESTIMATED format-switch saving (research Phase 3.3): tokens of a
    /// JSON-equivalent reconstruction minus tokens of the emitted body, at
    /// the served output rate, fee-applied. A LABELED ESTIMATE ("Est" in the
    /// header name) — NEVER folded into baseline / [`tt_saved_usd`]
    /// (Self::tt_saved_usd): a reconstruction is not an invoice figure (the
    /// batch_forgone precedent). Zero when no switch validated or the
    /// reconstruction was not computable ($0 + meter).
    pub format_switch_saved_est_usd: f64,
    /// Realized cost of a FAILED diff patch attempt on a fail-closed double
    /// dispatch, fee-applied. FOLDED into `cost_usd` (real invoice spend for
    /// this trace — budget/spend-sink must see it) AND duplicated here so a
    /// CFO can unpick the retry tax. The baseline stays re-emit-only, so a
    /// pure-failure trace's headline clamps to 0 — the honest outcome. Zero
    /// when no diff failed.
    pub diff_failed_cost_usd: f64,
    /// ESTIMATED vision-avoided saving from the Document Lane seam (D4): when the
    /// post-route-match distillation seam swaps an image/document part for distilled
    /// TEXT, the request that actually dispatched never contained the image, so
    /// this saving is a COUNTERFACTUAL (the raw image tokens that WOULD have been
    /// billed minus the distilled text tokens, priced at the input rate; $0 for
    /// Gemini per the D0 direction guard). Like `minify_saved_est_usd` it is
    /// ISOLATED: NEVER folded into `cost_usd` / `baseline_cost_usd` /
    /// [`tt_saved_usd`](Self::tt_saved_usd) (those reconcile against the realized
    /// invoice — a request that never sent the image cannot be invoice-reconciled
    /// on it). Surfaced on its own `X-TokenTrimmer-Doc-Vision-Saved-Est-Usd`
    /// header + `request_logs` column (migration 0032). **Always 0.0 in D4a**
    /// (substrate only — the seam that sets a non-zero value is D4c).
    pub doc_vision_saved_est_usd: f64,
    /// ESTIMATED content-aware compression saving (P1a): the input tokens the
    /// content_compress structural backend (JSON/CSV/log, opt-in via
    /// `RouteAction::content_compress`) removed before dispatch, priced at the
    /// served model's input rate, fee-applied. Like `doc_vision_saved_est_usd` it
    /// is ISOLATED: NEVER folded into `cost_usd` / `baseline_cost_usd` /
    /// [`tt_saved_usd`](Self::tt_saved_usd) (a conservative estimate, not an
    /// invoice-reconciled figure — the JSON/log/CSV compaction is content-
    /// lossless and could fold like `compression`, but P1a books it here to keep
    /// the reconciled headline clean and the isolated pattern consistent).
    /// Surfaced on its own `X-TokenTrimmer-Content-Compress-Saved-Est-Usd` header
    /// and the `request_logs` column (migration 0033). Zero when the route did
    /// not opt into `content_compress`.
    pub content_compress_saved_est_usd: f64,
}

impl CostBreakdown {
    /// Classify the provenance behind this exact strict-formula tuple. Pricing
    /// presence is passed separately because an absent catalog entry is
    /// intentionally flattened to numeric zero by the cost calculator.
    pub(crate) fn request_delta_evidence_state(
        &self,
        served_pricing_known: bool,
        baseline_pricing_known: bool,
    ) -> RequestDeltaEvidenceState {
        tt_shared::classify_request_delta_evidence_v1(
            served_pricing_known,
            baseline_pricing_known,
            RequestDeltaInput {
                baseline_cost_usd: Some(self.baseline_cost_usd),
                cost_usd: Some(self.cost_usd),
                provider_cache_saved_usd: Some(self.provider_cache_saved_usd),
                cache_bust_penalty_usd: Some(self.cache_bust_penalty_usd),
                summarizer_tax_usd: Some(self.summarizer_tax_usd),
            },
        )
    }

    /// TokenTrimmer-attributed savings: baseline minus actual cost, minus the
    /// provider-side cache discount (which TokenTrimmer did not cause), minus
    /// any booked cache-bust penalty (a cost TokenTrimmer DID cause).
    ///
    /// With no routing/caching by TT this is exactly 0 even when the provider
    /// reports cached tokens. When a cache-write premium exceeds the read
    /// discount (`provider_cache_saved_usd` clamped to 0), the premium reduces
    /// the TT claim instead — conservative in TT's disfavor; the cache-bust
    /// penalty follows the same precedent (it subtracts pre-clamp, so a bust
    /// can wipe the headline to 0 but never report a negative saving). The
    /// summarizer-LLM tax (`summarizer_tax_usd`, REAL aux spend) follows the
    /// SAME pre-clamp precedent — the loop win is reported net-of-tax — and is
    /// likewise never folded into `cost_usd` / `baseline_cost_usd`. Flex
    /// savings are included here automatically: serving via flex lowers
    /// `cost_usd`, so the baseline − cost delta picks the flex saving up (and
    /// `flex_saved_usd` isolates the flex component for the methodology
    /// breakdown).
    pub fn tt_saved_usd(&self) -> f64 {
        tt_shared::estimate_request_delta_v1(tt_shared::RequestDeltaInput {
            baseline_cost_usd: Some(self.baseline_cost_usd),
            cost_usd: Some(self.cost_usd),
            provider_cache_saved_usd: Some(self.provider_cache_saved_usd),
            cache_bust_penalty_usd: Some(self.cache_bust_penalty_usd),
            summarizer_tax_usd: Some(self.summarizer_tax_usd),
        })
        .map_or(0.0, |estimate| estimate.positive_request_delta_usd)
    }
}

/// Compute the [`CostBreakdown`] from token usage and pricing.
///
/// `pricing` is the served model's rate; `cost_usd` meters each prompt-token
/// bucket at its catalog rate — fresh input at `input_per_million`, cache reads
/// at the discounted `cached_input_per_million`, and cache writes
/// (`cache_creation_input_tokens`) at the cache-write premium. Writes are priced
/// at the **5-minute TTL tier** (`cache_write_per_million`, ~1.25× base input):
/// that is the only tier the gateway writes (the Anthropic adapter emits a bare
/// `ephemeral` breakpoint, which Anthropic defaults to the 5-minute TTL), and
/// the provider's flat `cache_creation_input_tokens` carries no per-tier split.
/// See the inline note in the body and
/// [`tt_shared::pricing::CacheWriteTier`].
///
/// `baseline_pricing` is the rate the request WOULD have paid without any
/// TokenTrimmer optimisation — i.e. the originally-requested model's rate at
/// full input price with no cache discount. When routing did not rewrite the
/// model, callers pass the same pricing for both so the baseline reflects the
/// served model's pre-discount cost. If `baseline_pricing` is `None`, it
/// falls back to `pricing` (conservative: reports no routing saving).
///
/// Attribution note: provider-reported cache reads/writes are attributed to
/// the *provider* side in full. For OpenAI/Gemini they are automatic. For
/// Anthropic the gateway's adapter may have injected the `cache_control`
/// breakpoint itself (model-aware prompt-cache-minimum gate in
/// `tt-provider-anthropic::translate`), but the usage that flows back carries
/// no signal distinguishing TT-injected breakpoints from caller-driven reuse,
/// so the whole class is conservatively credited to the provider rather than
/// inflating the TT headline.
pub(crate) fn compute_cost(
    usage: &Usage,
    pricing: Option<&ModelPricing>,
    baseline_pricing: Option<&ModelPricing>,
    fee_multiplier: f64,
) -> CostBreakdown {
    compute_cost_with_flex(usage, pricing, baseline_pricing, fee_multiplier, false)
}

/// Like [`compute_cost`] but with a `flex_applied` flag for requests served via
/// OpenAI's Flex service tier (`service_tier="flex"`).
///
/// When `flex_applied` is true, `cost_usd` is metered at the served model's
/// **flex** rates (~50% of standard) and [`CostBreakdown::flex_saved_usd`] is set
/// to the standard-vs-flex delta on this token usage at the served model — the
/// synchronous (standard) baseline cost minus the flex cost — a distinct savings
/// source named `flex`. Flex is only ever applied to a flex-eligible model (the
/// caller gates on [`ModelPricing::flex_eligible`]); if for some reason the
/// served model carries no flex rate, the flex path is a no-op and pricing falls
/// back to standard (no phantom saving).
///
/// Cache attribution is unchanged: provider-side cache discounts are still
/// computed against the served model's *standard* rates and surfaced via
/// `provider_cache_saved_usd`. For the flex-cost figure we conservatively apply
/// flex rates to the full prompt + completion (the hermetic flex path carries no
/// cached tokens; OpenAI's additional flex prompt-cache discount is not modeled
/// here, keeping the flex saving an exact, reconcilable standard−flex delta).
pub(crate) fn compute_cost_with_flex(
    usage: &Usage,
    pricing: Option<&ModelPricing>,
    baseline_pricing: Option<&ModelPricing>,
    fee_multiplier: f64,
    flex_applied: bool,
) -> CostBreakdown {
    compute_cost_full(
        usage,
        pricing,
        baseline_pricing,
        fee_multiplier,
        flex_applied,
        false,
        PassEffects::default(),
        0,
        crate::shaping::ShapeEffects::default(),
    )
}

/// Like [`compute_cost_with_flex`] but additionally attributes the
/// request-pass [`PassEffects`]: the conservative **compression pass** saving
/// and any **cache-bust penalty** (negative savings entry).
///
/// `effects.compression_tokens_removed` is the pipeline-MEASURED input-token
/// count the request-pass pipeline trimmed before dispatch (0 when the pass
/// did not run; the token-true gate guarantees it is never an inflation).
/// Those tokens are no longer in `usage.prompt_tokens` (the upstream metered
/// the reduced prompt), so the realized `cost_usd` already excludes them. To
/// attribute the saving we:
///
/// - value the removed tokens at the served model's **standard input rate**
///   (fee-applied) → [`CostBreakdown::compression_saved_usd`]: an exact,
///   invoice-reconcilable reduction in billed input tokens, and
/// - add that amount to `baseline_cost_usd` so the no-TT baseline reflects the
///   *uncompressed* prompt the request would have sent without TokenTrimmer.
///   This keeps [`CostBreakdown::tt_saved_usd`] honest — the compression saving
///   shows up in the headline as `baseline − cost`, the same way routing/flex
///   savings do.
///
/// Compression is a genuine TT-caused reduction in the input the customer sends
/// upstream (not a provider discount), so it belongs in the TT headline —
/// consistent with the provider-cache-vs-TT attribution rules.
///
/// `effects.doc_compaction_tokens_removed` (Document Lane D2) is the lossless
/// document-compaction pass's pipeline-MEASURED input-token removal — identical
/// in kind to compression (text-only, token-true-gated) but attributed to its
/// OWN [`CostBreakdown::doc_compaction_saved_usd`] bucket for the methodology
/// breakdown. It is valued at the served input rate and folded into the baseline
/// the SAME way as compression (via `input_tokens_removed`), so it rides
/// [`CostBreakdown::tt_saved_usd`] through `baseline − cost` without
/// double-counting against `compression_saved_usd` (the two buckets partition
/// the removed tokens). Zero when the route did not opt into `doc_compaction`.
///
/// `effects.elide_field_drop_tokens_removed` + `effects.elide_summary_tokens_removed`
/// are the agentic budget's Sub-lever 2 input-token removals (field-drop is
/// lossless + token-true-gated; summary tokens are counted only once the blind
/// paired judge committed the rewrite, caveat C1). They are pipeline-MEASURED
/// billed-input reductions identical in kind to compression, so they are summed
/// WITH `compression_tokens_removed` and valued / baseline-folded exactly the
/// same — riding [`CostBreakdown::tt_saved_usd`] via `baseline − cost`.
///
/// `effects.cache_bust_penalty_usd` is the (pre-fee) estimated cost of a
/// deliberate stable-prefix mutation booked via
/// [`CacheBustEstimate`](crate::passes::CacheBustEstimate). It lands
/// fee-applied in [`CostBreakdown::cache_bust_penalty_usd`] and reduces
/// [`CostBreakdown::tt_saved_usd`] pre-clamp — but is NEVER folded into
/// `cost_usd` / `baseline_cost_usd` (an estimate of induced future cost must
/// not contaminate fields that reconcile against the realized invoice). Caveat
/// C3: when the estimate is priced from the Anthropic cl100k proxy it
/// UNDER-counts (~15–20%), so the penalty is systematically LOW — acceptable
/// ONLY because under-booking a negative favors TT and the figure never reaches
/// the invoice fields.
///
/// `effects.summarizer_tax_usd` is the REAL auxiliary-LLM spend of the
/// summarizer calls (Sub-lever 2b). Aux spend is taxed, never free (spec §4.4
/// item 3): it lands fee-applied in [`CostBreakdown::summarizer_tax_usd`] and
/// reduces [`CostBreakdown::tt_saved_usd`] pre-clamp (the loop win is reported
/// net-of-tax) — but, like the cache-bust penalty, is NEVER folded into
/// `cost_usd` / `baseline_cost_usd` (the summarizer call bills the org on its
/// own credentials, not THIS request's served dispatch).
///
/// `batch_marked` flags a request the advisory batch-eligibility route action
/// marked (see `maybe_mark_batch_eligible`). It changes NO realized figure:
/// it only populates [`CostBreakdown::batch_forgone_usd`] — the discount the
/// async Batch Lane would have delivered, priced from the served model's REAL
/// catalog batch rate against the realized (flex-or-standard, cache-metered)
/// cost. A served model with no batch tier (possible after failover) forgoes
/// 0.0 — never a fabricated 0.5×.
///
/// `minify_saved_tokens_est` is the tokenizer-grounded minify estimate from
/// [`minify_saved_tokens_est`] (0 when the instruction was not injected, the
/// response was not valid JSON, or on streaming). Priced inside at the BILLED
/// output rate — the flex out-rate when `flex_applied` and flex rates exist,
/// else the standard rate — fee-applied, into
/// [`CostBreakdown::minify_saved_est_usd`] ONLY. Like `batch_forgone_usd`, it
/// changes NO realized or headline figure.
/// `shape` carries the response-side output-shaping effects
/// ([`crate::shaping::ShapeEffects`], research Phase 3.3 + 3.4), attributed
/// per the measured-vs-estimated line:
///
/// - `diff_output_tokens_saved` (MEASURED — real tokenizer counts on both
///   sides) is valued at the served output rate into
///   [`CostBreakdown::diff_saved_usd`] and the SAME token count raises
///   `baseline_cost_usd` at the baseline output rate, so the saving rides
///   [`CostBreakdown::tt_saved_usd`] exactly like compression.
/// - `format_switch_saved_est_usd` (ESTIMATE — a JSON-equivalent
///   reconstruction) lands fee-applied in its OWN field and is NEVER folded
///   into baseline / the headline (the batch_forgone precedent).
/// - `diff_failed_cost_usd` (REALIZED spend of a failed patch attempt) is
///   FOLDED into `cost_usd` — the trace's cost must reconcile against the
///   invoice, which billed both dispatches — and duplicated into its own
///   field so it can be unpicked. Baseline stays re-emit-only ⇒ a
///   pure-failure trace's headline clamps to 0.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_cost_full(
    usage: &Usage,
    pricing: Option<&ModelPricing>,
    baseline_pricing: Option<&ModelPricing>,
    fee_multiplier: f64,
    flex_applied: bool,
    batch_marked: bool,
    effects: PassEffects,
    minify_saved_tokens_est: u32,
    shape: crate::shaping::ShapeEffects,
) -> CostBreakdown {
    let Some(pricing) = pricing else {
        return CostBreakdown::default();
    };

    // Token breakdown (no double-counting):
    //   cache_read   = cached_tokens (already a subset of prompt_tokens)
    //   cache_write  = cache_creation_input_tokens (also in prompt_tokens)
    //   fresh_input  = prompt_tokens - cache_read - cache_write
    //
    // Rates:
    //   cache_read  → cached_input_per_million  (or base if absent)
    //   cache_write → cache_write_per_million   (5-min tier; or base if absent)
    //   fresh_input → input_per_million
    let cache_read = usage.cached_tokens.min(usage.prompt_tokens);
    let cache_write = usage
        .cache_creation_input_tokens
        .unwrap_or(0)
        .min(usage.prompt_tokens.saturating_sub(cache_read));
    let fresh_input = usage
        .prompt_tokens
        .saturating_sub(cache_read)
        .saturating_sub(cache_write);

    // Use cached rate when available; fall back to regular input rate.
    let cached_rate = pricing
        .cached_input_per_million
        .unwrap_or(pricing.input_per_million);
    // Cache-write TTL tier (write-premium selection):
    //
    // Anthropic bills cache *writes* at a per-TTL premium — the default 5-minute
    // ephemeral tier at ~1.25× base input, the opt-in 1-hour tier at ~2×. We
    // meter at the **5-minute tier** because that is the only tier the gateway
    // ever writes: the Anthropic adapter injects a bare
    // `cache_control: {"type": "ephemeral"}` with no `ttl` field (see
    // `tt-provider-anthropic::translate::maybe_inject_cache_control`), and
    // Anthropic defaults a bare `ephemeral` breakpoint to the 5-minute TTL.
    // The flat `cache_creation_input_tokens` the provider returns carries no
    // per-tier breakdown (the granular `cache_creation` split is an opt-in beta
    // we do not request), so there is no signal that would let us attribute any
    // write to the 1-hour tier even if one occurred. The 2× one-hour rate is
    // available via `ModelPricing::cache_write_rate_per_million(OneHour)` for
    // when a 1-hour write is introduced. Fall back to the base input rate when
    // the model documents no write premium (non-Anthropic — cost unchanged).
    let write_rate = pricing
        .cache_write_rate_per_million(CacheWriteTier::FiveMin)
        .unwrap_or(pricing.input_per_million);

    let standard_cost_usd = (fresh_input as f64) * pricing.input_per_million / 1_000_000.0
        + (cache_read as f64) * cached_rate / 1_000_000.0
        + (cache_write as f64) * write_rate / 1_000_000.0
        + (usage.completion_tokens as f64) * pricing.output_per_million / 1_000_000.0;

    // Flex (OpenAI service_tier="flex"): when applied AND the served model
    // carries a flex rate, the actual bill is the flex-rate cost (~50% of
    // standard). The flex saving is the standard−flex delta on this usage at the
    // served model, priced on the full prompt + completion so the figure is an
    // exact, invoice-reconcilable difference. Falls back to standard if a flex
    // opt-in ever reaches a model with no flex rate (no phantom saving).
    let (cost_usd, flex_cost_basis) = match (flex_applied, pricing.flex_rates_per_million()) {
        (true, Some((flex_in, flex_out))) => {
            // Respect prompt cache discounts under flex execution
            let flex_cost = (fresh_input as f64) * flex_in / 1_000_000.0
                + (cache_read as f64) * cached_rate / 1_000_000.0
                + (cache_write as f64) * write_rate / 1_000_000.0
                + (usage.completion_tokens as f64) * flex_out / 1_000_000.0;
            (flex_cost, Some(flex_cost))
        }
        _ => (standard_cost_usd, None),
    };
    // Standard cost at the served model on the SAME basis the flex cost uses —
    // the comparison point for the flex saving so the delta is exactly standard − flex.
    let standard_full_cost_usd = standard_cost_usd;
    let flex_saved_usd = match flex_cost_basis {
        Some(flex_cost) => (standard_full_cost_usd - flex_cost).max(0.0),
        None => 0.0,
    };

    // Batch (advisory, research Phase 2.1): the FORGONE Batch-API discount —
    // what the request would have saved had it gone through the (future) async
    // Batch Lane. Priced from the served model's REAL catalog batch rate on
    // the full prompt + completion (no cache-discount stacking — the same
    // conservative basis as the flex cost and `tt_shared::batch_advisor`),
    // compared against the realized pre-fee `cost_usd` so the figure is "50%
    // off the actual dispatch cost". Floored at 0. NEVER added to any realized
    // or saved figure: the gateway dispatched synchronously and billed
    // `cost_usd` in full.
    let batch_forgone_usd = if batch_marked {
        match pricing.batch_rates_per_million() {
            Some((batch_in, batch_out)) => {
                let batch_cost = (usage.prompt_tokens as f64) * batch_in / 1_000_000.0
                    + (usage.completion_tokens as f64) * batch_out / 1_000_000.0;
                (cost_usd - batch_cost).max(0.0)
            }
            // Failover may serve a model with no batch tier — no real rate,
            // no fabricated claim.
            None => 0.0,
        }
    } else {
        0.0
    };

    // Served-model cost as if no provider caching had occurred: all prompt
    // tokens at the full input rate. The delta against the (standard) cost is the
    // provider's automatic cache discount (read discount net of any
    // cache-write premium) — savings the provider grants with or without
    // TokenTrimmer, so they are excluded from the TT-attributed figure. Computed
    // on standard rates (flex never widens the cache-attributed figure).
    let no_cache_cost_usd = standard_full_cost_usd;

    // Baseline: full input × input rate + output × output rate (no cache
    // discount), priced against the originally-requested model.
    let baseline_pricing = baseline_pricing.unwrap_or(pricing);
    let baseline_cost_usd = (usage.prompt_tokens as f64) * baseline_pricing.input_per_million
        / 1_000_000.0
        + (usage.completion_tokens as f64) * baseline_pricing.output_per_million / 1_000_000.0;

    // Compression saving: the input tokens the pass removed are no longer in
    // `usage.prompt_tokens`, so the realized cost already excludes them. Value
    // them at the served model's STANDARD input rate (a genuine, reconcilable
    // reduction in billed input tokens) and add the SAME amount to the baseline
    // so the no-TT baseline reflects the *uncompressed* prompt — the
    // `baseline − cost` headline then includes the compression saving. Zero when
    // the pass did not run.
    //
    // The agentic budget's Sub-lever 2 input-token removals ride the SAME
    // bucket: `elide_field_drop_tokens_removed` (lossless, token-true-gated) and
    // `elide_summary_tokens_removed` (lossy, but only counted AFTER the blind
    // paired judge COMMITTED the rewrite) are both genuine, pipeline-MEASURED
    // reductions in billed input tokens — identical in kind to compression — so
    // they are valued at the served input rate and folded into the baseline the
    // same way. (Caveat C1: summary tokens enter this sum only once judge-gated;
    // the planner books the un-summarized count otherwise.) The summarizer TAX
    // for those calls is a separate negative entry below — never netted here.
    let compression_input_tokens_removed = effects.compression_tokens_removed
        + effects.elide_field_drop_tokens_removed
        + effects.elide_summary_tokens_removed;
    // Document-compaction (Document Lane D2) is a SEPARATE lossless input-token
    // removal lever. It is valued into its OWN bucket for the methodology
    // breakdown but folded into the baseline the SAME way as compression, so
    // the two never double-count in the `baseline − cost` headline (each token
    // is removed from the dispatched prompt exactly once and re-added to the
    // baseline exactly once).
    let doc_compaction_tokens_removed = effects.doc_compaction_tokens_removed;
    // Total removed input tokens folded into the baseline (all lossless,
    // token-true-gated reductions of billed input).
    let input_tokens_removed = compression_input_tokens_removed + doc_compaction_tokens_removed;
    let compression_saved_usd =
        (compression_input_tokens_removed as f64) * pricing.input_per_million / 1_000_000.0;
    let doc_compaction_saved_usd =
        (doc_compaction_tokens_removed as f64) * pricing.input_per_million / 1_000_000.0;
    // Content-aware compression (P1a): the input tokens the content_compress
    // structural backend removed, valued at the served model's input rate. This
    // is ISOLATED — NOT added to `input_tokens_removed`/the baseline fold below
    // (unlike compression/doc_compaction) — so it never enters the reconciled
    // `baseline − cost` headline; it books ONLY into the estimate field. Zero
    // when the route did not opt into content_compress.
    let content_compress_saved_est_usd =
        (effects.content_compress_tokens_removed as f64) * pricing.input_per_million / 1_000_000.0;
    // Fold the removed-token value into the baseline at the baseline model's
    // input rate (what the customer would have paid sending the un-trimmed
    // prompt to the baseline model). Includes BOTH the compression and the
    // doc-compaction removals so each lever's saving rides `baseline − cost`.
    let baseline_compression_usd =
        (input_tokens_removed as f64) * baseline_pricing.input_per_million / 1_000_000.0;

    // Minify estimate: the saved-output-token estimate priced at the rate the
    // request's output was actually BILLED at — the flex out-rate when flex
    // applied (and the model carries one), else the standard output rate.
    // Lands in its own ESTIMATE field only; never touches cost/baseline/
    // headline (those reconcile against the invoice).
    let billed_output_rate = match (flex_applied, pricing.flex_rates_per_million()) {
        (true, Some((_, flex_out))) => flex_out,
        _ => pricing.output_per_million,
    };
    let minify_saved_est_usd = (minify_saved_tokens_est as f64) * billed_output_rate / 1_000_000.0;
    // Diff saving (research Phase 3.4, MEASURED): the output tokens the
    // applied patch avoided billing, valued at the served output rate, with
    // the SAME token count folded into the baseline at the baseline model's
    // output rate (what the customer would have paid receiving the full
    // re-emission without TokenTrimmer) — the compression precedent, so the
    // saving rides the `baseline − cost` headline. Zero when no diff applied.
    let diff_saved_usd =
        f64::from(shape.diff_output_tokens_saved) * pricing.output_per_million / 1_000_000.0;
    let baseline_diff_usd = f64::from(shape.diff_output_tokens_saved)
        * baseline_pricing.output_per_million
        / 1_000_000.0;

    // Apply the provider surcharge (e.g. OpenRouter's 5% BYOK fee) to all
    // figures so the saved splits stay consistent (same scale factor). The
    // provider-cache discount is metered against the STANDARD cost (not the
    // flex cost) so flex and cache savings stay independent and don't
    // double-count. The failed-patch cost (`shape.diff_failed_cost_usd`,
    // pre-fee) folds into `cost_usd` BEFORE the fee — both dispatches carry
    // the same provider surcharge on the real invoice.
    CostBreakdown {
        cost_usd: (cost_usd + shape.diff_failed_cost_usd) * fee_multiplier,
        baseline_cost_usd: (baseline_cost_usd + baseline_compression_usd + baseline_diff_usd)
            * fee_multiplier,
        provider_cache_saved_usd: ((no_cache_cost_usd - standard_cost_usd) * fee_multiplier)
            .max(0.0),
        flex_saved_usd: flex_saved_usd * fee_multiplier,
        compression_saved_usd: compression_saved_usd * fee_multiplier,
        doc_compaction_saved_usd: doc_compaction_saved_usd * fee_multiplier,
        cache_bust_penalty_usd: effects.cache_bust_penalty_usd * fee_multiplier,
        summarizer_tax_usd: effects.summarizer_tax_usd * fee_multiplier,
        batch_forgone_usd: batch_forgone_usd * fee_multiplier,
        minify_saved_est_usd: minify_saved_est_usd * fee_multiplier,
        diff_saved_usd: diff_saved_usd * fee_multiplier,
        format_switch_saved_est_usd: shape.format_switch_saved_est_usd * fee_multiplier,
        diff_failed_cost_usd: shape.diff_failed_cost_usd * fee_multiplier,
        // Document Lane (D4a): always 0 — the post-match distillation seam that
        // books a non-zero vision-avoided saving on this isolated field is D4c.
        // Isolated: NOT folded into cost_usd/baseline_cost_usd above.
        doc_vision_saved_est_usd: 0.0,
        // Content-aware compression (P1a): the ISOLATED estimated saving from the
        // content_compress backend's removed input tokens. NOT folded into
        // cost_usd/baseline_cost_usd above (unlike compression/doc_compaction).
        content_compress_saved_est_usd: content_compress_saved_est_usd * fee_multiplier,
    }
}
