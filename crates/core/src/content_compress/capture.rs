//! Flywheel telemetry for content-aware compression (P1a) — the Phase-2 enabler.
//!
//! Two planes, by design:
//!
//! 1. **Metrics (ALWAYS on).** Every compressed request records its dominant
//!    compacted kind in the `request_logs.content_compress_kind` column and its
//!    isolated saving in `content_compress_saved_est_usd`. This is metrics/labels
//!    only — no request content — matching TT's zero-data-retention posture.
//!
//! 2. **Raw before/after capture (OPT-IN, default OFF).** Gated by the
//!    `TT_COMPRESS_CAPTURE` environment flag (and, later, a per-org flag). When
//!    ON it would persist `{content_type, before, after, judge_verdict}` pairs to
//!    a capture sink for Phase-2 training. In P1a this is a SCAFFOLD ONLY: the
//!    sink is a no-op that emits a metrics-only structured event when enabled;
//!    the actual raw before/after persistence is DEFERRED to P1d
//!    (`flywheel dataset export`). Default posture is metrics/hashes only.

use std::sync::OnceLock;

/// Environment flag that opts an instance into raw-pair capture. Default OFF.
pub const CAPTURE_ENV: &str = "TT_COMPRESS_CAPTURE";

/// True when raw before/after capture is enabled for this instance
/// (`TT_COMPRESS_CAPTURE` set to a truthy value). Read ONCE and cached — the
/// classifier/dispatcher runs on the hot path, so this must not re-read the env
/// per request. Default (unset / `0` / `false` / empty) is OFF.
#[must_use]
pub fn capture_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var(CAPTURE_ENV)
            .ok()
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                !(v.is_empty() || v == "0" || v == "false" || v == "no" || v == "off")
            })
            .unwrap_or(false)
    })
}

/// Record a compression event into the flywheel's OPT-IN raw-capture sink.
///
/// - Default (capture OFF): a NO-OP — nothing is emitted here (the metrics-plane
///   `content_compress_kind` column carries the ZDR-safe signal).
/// - Capture ON: emits a metrics-only structured event (`content_type` +
///   token counts, NO request text). Persisting the raw before/after pair for
///   Phase-2 training is DEFERRED to P1d; this is the scaffold + the gate.
pub fn record(kind: Option<&str>, tokens_removed: u32) {
    if !capture_enabled() {
        return;
    }
    // Scaffold sink (P1a): metrics only. Raw before/after persistence is P1d.
    tracing::info!(
        target: "tt.content_compress.capture",
        content_type = kind.unwrap_or("unknown"),
        tokens_removed,
        "content_compress capture (metrics-only scaffold; raw pair deferred to P1d)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_is_a_noop_when_capture_disabled() {
        // The default posture is OFF — `record` must not panic and must emit no
        // raw text. (We cannot flip the OnceLock-cached env mid-process, so this
        // asserts the default path is inert.)
        record(Some("json"), 128);
        record(None, 0);
    }

    #[test]
    fn capture_disabled_by_default_in_test_env() {
        // TT_COMPRESS_CAPTURE is unset in the test environment → OFF.
        assert!(
            !capture_enabled(),
            "raw capture must be OFF by default (ZDR posture)"
        );
    }
}
