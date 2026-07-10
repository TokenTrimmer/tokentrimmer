//! Document Lane — the pre-routing image/document → text distillation seam.
//!
//! This module's prose docs wrap across many `//!` continuation lines (the
//! D4a–D4c slice history), which trips clippy's `doc_lazy_continuation` /
//! `doc list item without indentation` lints on the wrapped list-item prose.
//! Allow them here (mirrors the `quality_gate` module's allow) — the prose is
//! intentional + indented with `//!   ` for the list continuations.
#![allow(clippy::doc_lazy_continuation)]
//!
//! This module is the D4a **substrate**: it declares the shape the later slices
//! build on, but performs NO extraction, distillation, or cost reduction.
//!
//! - **D4a (this slice):** the [`DocDistillGate`] scaffold — a default-CLOSED
//!   gate whose [`should_distill`](DocDistillGate::should_distill) always returns
//!   `false`. No behavior change.
//! - **D4b:** the out-of-process OCR/parse sidecar + a fail-open Rust client.
//! - **D4c:** the pre-routing seam in `prepare()` that (when the org/route opts
//!   in via `RouteAction::document_lane`) distills image/document parts to text
//!   so routing's `target_model` rewrite downgrades to a text model, and books
//!   the isolated `doc_vision_saved_est_usd` counterfactual (D4c-v2: priced from
//!   the seam's bookkeeping via D0's [`document_projection::project`]). The
//!   downgrade is the route's `target_model` rewrite, NOT a capability-flag
//!   re-flip (see #307). The lossy-substitution [`DocDistillGate`] (recall judge
//!   + the sticky 0.90 auto-pause floor) is STILL the default-CLOSED scaffold —
//!   lossy spans stay verbatim until that slice; lossless PDF-text layers
//!   substitute unconditionally + book their saving now.
//!
//! Lossy substitution is **opt-in + judge-gated permanently**: the gate is
//! default-CLOSED and fails open to the verbatim request. Error blobs are never
//! distilled. See `docs/superpowers/specs/2026-07-01-document-lane-d4-server-seam-design.md`.

/// D4c — the pre-routing distillation seam (invoked in `prepare()`).
pub mod seam;
/// The fail-open client for the out-of-process `doc-sidecar` OCR/parse service
/// (D4b). Disabled unless `TT_DOC_SIDECAR_URL` is set; any error/timeout → no
/// extraction, so the request stays verbatim.
pub mod sidecar_client;

/// Whether a distillation span is lossless (a PDF text layer — structurally
/// safe to substitute) or lossy (OCR / a scanned page — must clear the judge +
/// floor before it may replace the verbatim image).
///
/// D4a defines the vocabulary; the sidecar (D4b) tags spans with it and the
/// gate (D4c) branches on it. Nothing constructs a non-default value yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpanFidelity {
    /// Verbatim text extracted from a document's own text layer — no model in
    /// the loop, so the substitution is structurally safe (skips the judge).
    #[default]
    Lossless,
    /// OCR / vision-model output for a scanned page — a lossy transform that the
    /// gate must judge against the baseline before allowing the swap.
    Lossy,
}

/// The lossy-substitution quality gate for the Document Lane.
///
/// **D4a scaffold:** default-CLOSED with no live judging — [`should_distill`]
/// always returns `false`, so wiring this type anywhere is a guaranteed no-op
/// (the request stays verbatim). D4c fills in the recall-of-baseline judge + the
/// sticky 0.90 `auto_pause` floor and replaces the body of [`should_distill`].
///
/// The gate ALWAYS fails open to the verbatim request: a closed gate, a judge
/// error, or a floor miss all mean "keep the image, book no saving". Error blobs
/// are never distilled.
///
/// [`should_distill`]: DocDistillGate::should_distill
#[derive(Debug, Clone, Default)]
pub struct DocDistillGate {
    /// Whether the route explicitly opted into LOSSY substitution (OCR/scanned
    /// pages). Lossless text-layer spans never need this. Default `false`
    /// (closed) — D4c reads it; D4a ignores it.
    lossy_opt_in: bool,
}

impl DocDistillGate {
    /// A default-CLOSED gate. Equivalent to [`Default::default`]; spelled out so
    /// call sites read intent-first.
    #[must_use]
    pub fn closed() -> Self {
        Self::default()
    }

    /// A gate that carries the route's LOSSY opt-in. In D4a this only records
    /// intent — [`should_distill`](Self::should_distill) is still fail-closed;
    /// D4c is the slice that lets the opt-in actually gate a lossy substitution.
    #[must_use]
    pub fn with_lossy_opt_in(lossy_opt_in: bool) -> Self {
        Self { lossy_opt_in }
    }

    /// Whether the route opted into LOSSY (OCR/scanned) substitution.
    #[must_use]
    pub fn lossy_opt_in(&self) -> bool {
        self.lossy_opt_in
    }

    /// Whether a distilled span may replace the verbatim image/document part.
    ///
    /// **D4a: always `false`** — the substrate ships the gate closed with no
    /// live judging. D4c replaces this body with the real decision (lossless →
    /// allow; lossy → allow only when opted-in AND the recall-of-baseline judge
    /// clears the 0.90 floor; judge-fail / floor-miss / error / error-blob →
    /// closed). The signature already carries the fidelity + a baseline-recall
    /// score so D4c is a body swap, not an API change; `lossy_opt_in` is read
    /// here so the plumbing shape is real (not dead), but the gate stays closed.
    #[must_use]
    pub fn should_distill(&self, fidelity: SpanFidelity, _baseline_recall: f64) -> bool {
        // Scaffold: fail-closed regardless of fidelity / opt-in / recall. No
        // distillation happens in D4a — D4c fills in the real decision.
        let _would_consider_lossy = fidelity == SpanFidelity::Lossy && self.lossy_opt_in;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_gate_is_closed() {
        let gate = DocDistillGate::default();
        assert!(
            !gate.should_distill(SpanFidelity::Lossless, 1.0),
            "D4a scaffold must never distill, even a perfect-recall lossless span"
        );
        assert!(!gate.should_distill(SpanFidelity::Lossy, 0.99));
        assert!(!DocDistillGate::closed().should_distill(SpanFidelity::Lossless, 1.0));
    }

    #[test]
    fn lossy_opt_in_records_intent_but_gate_stays_closed() {
        let opted = DocDistillGate::with_lossy_opt_in(true);
        assert!(opted.lossy_opt_in(), "opt-in must be recorded");
        // …but D4a never distills, even a perfect-recall lossy span.
        assert!(!opted.should_distill(SpanFidelity::Lossy, 1.0));
        assert!(!DocDistillGate::default().lossy_opt_in());
    }

    #[test]
    fn span_fidelity_defaults_to_lossless() {
        assert_eq!(SpanFidelity::default(), SpanFidelity::Lossless);
    }
}
