//! Flywheel telemetry for content-aware compression — the Phase-2 enabler.
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
//!    ON it persists `{content_type, before, after, tokens, gate_committed}`
//!    pairs to a JSONL file at `TT_COMPRESS_CAPTURE_PATH` for Phase-2 training.
//!    Default posture is metrics/hashes only. P1d made this sink REAL (P1a left
//!    it a no-op scaffold); the offline `tt export compress-corpus` CLI
//!    materializes the captured JSONL into a versioned training corpus.
//!
//! # ZDR + hot-path discipline
//! Capture is OFF by default. `capture_enabled()` + `capture_path()` are each
//! read ONCE and cached in a `OnceLock` (the classifier/dispatcher runs on the
//! hot path). [`record_pair`] is the only write API: it is a no-op unless BOTH
//! the flag and the path are set, and a write error is logged-then-swallowed
//! (capture must NEVER break a request — it is observability, not a control).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Serialize;

use chrono::Utc;

/// Environment flag that opts an instance into raw-pair capture. Default OFF.
pub const CAPTURE_ENV: &str = "TT_COMPRESS_CAPTURE";

/// Environment flag naming the JSONL sink file raw pairs are appended to.
/// Required for capture to actually persist; when `TT_COMPRESS_CAPTURE` is set
/// but this is unset, [`record_pair`] is a no-op (so enabling capture always
/// requires an explicit, writable destination — never a silent default path).
pub const CAPTURE_PATH_ENV: &str = "TT_COMPRESS_CAPTURE_PATH";

/// The capture-record schema version. Bumped only on a breaking shape change;
/// the export CLI refuses a record whose `schema_version` it does not understand
/// rather than silently mis-reading it (mirrors `SavingsBundle`'s discipline).
pub const CAPTURE_SCHEMA_VERSION: u32 = 1;

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

/// The capture sink file, when `TT_COMPRESS_CAPTURE_PATH` is set. Read ONCE and
/// cached (hot path). `None` when unset — capture then has nowhere to write and
/// [`record_pair`] is a no-op even if [`capture_enabled`] is true.
#[must_use]
pub fn capture_path() -> Option<PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        std::env::var_os(CAPTURE_PATH_ENV)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    })
    .clone()
}

/// One captured before/after pair. Serialized as one JSONL line by
/// [`record_pair`] (P1d: the write is real, not a scaffold); read back by the
/// `tt export compress-corpus` CLI.
///
/// `tokens_before`/`tokens_after` are recorded `0` on the hot path (the pass
/// self-measures only the whole-tail delta); the export CLI recomputes them
/// offline from `before`/`after` via `tt_tokenize`, so the per-block ratio is
/// authoritative without a per-block tokenize on the hot path.
///
/// `gate_committed` is the ONLY verdict P1d attaches: `true` when the block was
/// compacted (for the structural backends, always; for the lossy prose/code
/// backends, this is reached only when the gate trusted the class). The richer
/// paired recall-of-baseline verdict is a Phase-2 concern (it runs against the
/// response, which `content_compress` never sees).
#[derive(Debug, Clone, Serialize)]
pub struct CaptureRecord {
    /// See [`CAPTURE_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Always `true` on a persisted record — capture only writes when opted in.
    /// The export CLI REFUSES any record where this is not `true` (ZDR gate).
    pub capture_opted_in: bool,
    /// The compacted content kind: `"json"` / `"csv"` / `"log"` / `"prose"` /
    /// `"code"` (the `ContentKind` as a lowercase string).
    pub kind: String,
    /// The block text BEFORE compaction (raw request content — opt-in only).
    pub before: String,
    /// The block text AFTER compaction (the bytes actually dispatched).
    pub after: String,
    /// Placeholder `0` on the hot path; the export recomputes offline.
    pub tokens_before: u32,
    /// Placeholder `0` on the hot path; the export recomputes offline.
    pub tokens_after: u32,
    /// The pipeline-measured token delta for the WHOLE tail this block belonged
    /// to (carried for context; the per-block delta is recomputed at export).
    pub tokens_removed: u32,
    /// See [`CaptureRecord`] doc — the gate-trust verdict at commit time.
    pub gate_committed: bool,
    /// The org whose request was compressed (the ZDR opt-in is instance-level
    /// today; a per-org flag is a later cloud concern).
    pub org_id: String,
    /// The request's `trace_id` — the join key to the eventual response-side
    /// paired quality verdict (Phase 2).
    pub trace_id: String,
    /// The served model id (tokenization context for the export's recompute).
    pub model: String,
    /// The provider id (tokenization context for the export's recompute).
    pub provider_id: String,
    /// RFC 3339 capture timestamp (informational; explicitly NOT part of any
    /// reproduction check).
    pub ts: String,
}

impl CaptureRecord {
    /// Build a record for one compacted block. `tokens_removed` is the
    /// whole-tail delta the pass self-measured (context only); the per-block
    /// `tokens_before`/`tokens_after` are `0` here and recomputed at export.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: impl Into<String>,
        before: impl Into<String>,
        after: impl Into<String>,
        tokens_removed: u32,
        org_id: impl Into<String>,
        trace_id: impl Into<String>,
        model: impl Into<String>,
        provider_id: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: CAPTURE_SCHEMA_VERSION,
            capture_opted_in: true,
            kind: kind.into(),
            before: before.into(),
            after: after.into(),
            tokens_before: 0,
            tokens_after: 0,
            tokens_removed,
            gate_committed: true,
            org_id: org_id.into(),
            trace_id: trace_id.into(),
            model: model.into(),
            provider_id: provider_id.into(),
            ts: Utc::now().to_rfc3339(),
        }
    }
}

/// Append one `CaptureRecord` as a JSONL line to the configured sink. NO-OP
/// when [`capture_enabled`] is false OR [`capture_path`] is unset. A write error
/// is logged-then-swallowed (capture is observability, not a control — it must
/// never break a request).
///
/// This is the hot-path write API; the export CLI reads the resulting JSONL
/// back offline. The sink is [`write_pair_to`] (injectable path) so the write
/// is unit-testable without flipping the process-cached env gate.
pub fn record_pair(rec: &CaptureRecord) {
    if !capture_enabled() {
        return;
    }
    let Some(path) = capture_path() else {
        return;
    };
    if let Err(e) = write_pair_to(&path, rec) {
        tracing::warn!(
            target: "tt.content_compress.capture",
            path = %path.display(),
            error = %e,
            "content_compress capture write failed (continuing; capture is best-effort)"
        );
    }
}

/// The pure sink: append `rec` as one JSON line to `path`, creating the file if
/// it does not exist. Injectable for tests (no env gate, no OnceLock). Returns
/// the `io::Error` on failure so the caller can decide to log/swallow.
fn write_pair_to(path: &Path, rec: &CaptureRecord) -> std::io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    // A single `write_all` of the serialized line + newline keeps each record
    // atomic w.r.t. other appenders (one `write` syscall on POSIX append mode).
    serde_json::to_writer(&mut file, rec)?;
    file.write_all(b"\n")?;
    Ok(())
}

/// Record a compression event into the flywheel's OPT-IN raw-capture sink.
///
/// - Default (capture OFF): a NO-OP — nothing is emitted here (the metrics-plane
///   `content_compress_kind` column carries the ZDR-safe signal).
/// - Capture ON (P1d): emits a metrics-only structured event. The per-block
///   before/after pair is written by [`record_pair`] at the compact point; this
///   per-request summary is kept for the existing call site that has no
///   before/after text (the kind + whole-tail delta).
pub fn record(kind: Option<&str>, tokens_removed: u32) {
    if !capture_enabled() {
        return;
    }
    tracing::info!(
        target: "tt.content_compress.capture",
        content_type = kind.unwrap_or("unknown"),
        tokens_removed,
        "content_compress capture (metrics-only event; raw pairs via record_pair)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default posture is OFF — `record_pair` must not write any file.
    #[test]
    fn record_pair_is_a_noop_when_capture_disabled() {
        // capture_enabled() is OnceLock-cached from the test env (unset → OFF),
        // so record_pair returns before touching the filesystem.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let rec = CaptureRecord::new(
            "json", "before", "after", 10, "org", "trace", "gpt-4o", "openai",
        );
        record_pair(&rec);
        // No path is configured (capture_path is None in the test env), so the
        // temp file is untouched.
        assert!(
            std::fs::read_to_string(tmp.path()).unwrap().is_empty(),
            "no file written when capture is disabled"
        );
    }

    #[test]
    fn capture_disabled_by_default_in_test_env() {
        assert!(
            !capture_enabled(),
            "raw capture must be OFF by default (ZDR posture)"
        );
        assert!(
            capture_path().is_none(),
            "no capture path by default (ZDR posture)"
        );
    }

    /// The pure sink `write_pair_to` appends one JSONL line that round-trips to
    /// the same `CaptureRecord` (the export CLI's read path relies on this).
    #[test]
    fn write_pair_appends_round_trippable_jsonl() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let rec = CaptureRecord::new(
            "json",
            "{ \"k\": 1 }",
            "{\"k\":1}",
            42,
            "00000000-0000-0000-0000-000000000001",
            "00000000-0000-0000-0000-000000000002",
            "gpt-4o",
            "openai",
        );
        write_pair_to(&path, &rec).unwrap();

        // Two appends = two lines.
        let rec2 = CaptureRecord::new(
            "code",
            "fn a() { long body }",
            "fn a() { /* elided */ }",
            7,
            "00000000-0000-0000-0000-000000000003",
            "00000000-0000-0000-0000-000000000004",
            "gpt-4o",
            "openai",
        );
        write_pair_to(&path, &rec2).unwrap();

        let buf = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = buf.lines().collect();
        assert_eq!(lines.len(), 2, "two appends = two JSONL lines");

        let got1: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(got1["schema_version"], CAPTURE_SCHEMA_VERSION);
        assert_eq!(got1["capture_opted_in"], true);
        assert_eq!(got1["kind"], "json");
        assert_eq!(got1["before"], "{ \"k\": 1 }");
        assert_eq!(got1["after"], "{\"k\":1}");
        assert_eq!(got1["tokens_removed"], 42);
        assert_eq!(got1["gate_committed"], true);
        assert_eq!(got1["org_id"], "00000000-0000-0000-0000-000000000001");
        assert_eq!(got1["model"], "gpt-4o");

        let got2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(got2["kind"], "code");
        assert_eq!(got2["trace_id"], "00000000-0000-0000-0000-000000000004");
    }

    /// `write_pair_to` creates the file when it does not exist (the first
    /// capture on a fresh sink path).
    #[test]
    fn write_pair_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("compress_capture.jsonl");
        assert!(!path.exists());
        let rec = CaptureRecord::new("log", "a", "b", 1, "o", "t", "m", "p");
        write_pair_to(&path, &rec).unwrap();
        assert!(path.exists(), "the sink file is created on first write");
    }
}
