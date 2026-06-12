//! Contract-changing output shaping (research Phase 3.3 + 3.4).
//!
//! Two OPT-IN, per-route levers that change WHAT THE MODEL EMITS — the
//! highest-rate savings surface (output tokens bill ~4-5x input) and the one
//! requiring the most care:
//!
//! * **Format switch** ([`format_switch`]): instruct the model to emit CSV
//!   (flat-uniform records) or a bare value (single-value asks) instead of
//!   verbose JSON. The caller opted in via the route — the changed parse
//!   contract is the point — but every switched response is advertised with a
//!   `format_switch:<label>` warnings token, and the emission is
//!   strip-validated in code (fail OPEN to the untouched body on any
//!   mismatch). Savings are a labeled ESTIMATE (JSON-equivalent
//!   reconstruction), never folded into invoice-reconciled figures.
//!
//! * **Delta/diff responses** ([`diff`]): for edit/iteration routes where the
//!   prior artifact is identifiable FROM THE REQUEST, instruct the model to
//!   emit an anchored search/replace patch; the gateway applies + validates
//!   it and returns the FULL reconstructed artifact — the caller sees a
//!   normal completion (contract preserved); the provider bills the short
//!   patch. ANY validation failure fails CLOSED to a full re-emit, and the
//!   failed attempt's realized cost is booked honestly. Savings are MEASURED
//!   (real tokenizer counts on both sides).
//!
//! Both levers are OFF by default, never touch streaming/tool/n>1 traffic
//! (no-op + warning), are disabled by strict structured output (grammar
//! lock), and can never 5xx a request.

pub(crate) mod diff;
pub(crate) mod format_switch;

/// Response-shaping effects threaded into `compute_cost_full` —
/// the response-side sibling of [`crate::passes::PassEffects`].
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ShapeEffects {
    /// Output tokens the diff lane avoided billing:
    /// `tokenize(reconstructed artifact) − usage.completion_tokens`, floored
    /// at 0. Both sides are real tokenizer counts on real strings — this is
    /// the measured saving that folds into the headline via the baseline
    /// raise (compression precedent).
    pub diff_output_tokens_saved: u32,
    /// Pre-fee ESTIMATED format-switch saving (USD): JSON-equivalent
    /// reconstruction − emitted body, at the served output rate. 0.0 unless a
    /// switch validated AND the reconstruction was computable (else $0 +
    /// meter). NEVER folded into the headline — own labeled field only.
    pub format_switch_saved_est_usd: f64,
    /// Pre-fee realized cost of a FAILED patch attempt (fail-closed double
    /// dispatch). Folded into `cost_usd` (it is real invoice spend for this
    /// trace) and surfaced in its own column/header so a CFO can unpick it.
    pub diff_failed_cost_usd: f64,
}

/// Shared decision shape for the two planners: apply with a plan, or no-op
/// with the warnings-token reason suffix (`format_switch_skipped:<reason>` /
/// `diff_skipped:<reason>`). Skip reasons are a bounded `&'static str` set so
/// they can double as metric labels.
pub(crate) enum ShapeDecision<P> {
    Apply(P),
    Skip(&'static str),
}
