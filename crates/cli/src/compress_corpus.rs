//! `tt export compress-corpus` — materialize the opt-in content-compression
//! capture into a versioned Phase-2 training corpus.
//!
//! P1d's flywheel dataset export (the Phase-1→Phase-2 handoff artifact). The
//! gateway's content-aware compression pass, when an instance opts in via
//! `TT_COMPRESS_CAPTURE=1` + `TT_COMPRESS_CAPTURE_PATH=<file>`, appends one
//! JSONL [`CapturedPair`] per compacted block to that sink (see
//! `tt_core::content_compress::capture`). This module reads that JSONL back and
//! emits a versioned [`TrainingCorpus`] — the Phase-2 training input — with the
//! per-block token counts recomputed offline via `tt_tokenize`.
//!
//! # ZDR (the differentiator)
//! A capture record can ONLY exist when the instance opted in (the gateway's
//! `record_pair` is a no-op otherwise). Every record carries
//! `capture_opted_in: true`. The export REFUSES any record where this is not
//! `true` — naming the offending `trace_id` — and writes NO output on refuse.
//! This is the ZDR gate at the export boundary: a corpus is never produced from
//! a capture that wasn't explicitly opted in.
//!
//! # Offline purity
//! `run_export` reads one file, writes one file, touches no network/DB — mirrors
//! `tt verify-bundle`'s offline purity (the same reproducibility discipline).

use std::path::Path;

use clap::Subcommand;
use serde::{Deserialize, Serialize};

use anyhow::Context;

/// The `tt export` subcommands.
#[derive(Subcommand)]
pub enum ExportAction {
    /// Materialize the opt-in content-compression capture into a versioned
    /// Phase-2 training corpus (offline).
    ///
    /// Reads the JSONL sink produced when the gateway ran with
    /// `TT_COMPRESS_CAPTURE=1` + `TT_COMPRESS_CAPTURE_PATH=<file>`, recomputes
    /// the per-block token counts offline, and writes a versioned training
    /// corpus to `--output`. REFUSES any capture record not marked opted-in
    /// (ZDR — names the `trace_id`, writes no output). An empty capture yields
    /// an empty corpus (not an error).
    CompressCorpus {
        /// Path to the capture JSONL (`TT_COMPRESS_CAPTURE_PATH` sink).
        #[arg(long, value_name = "PATH")]
        input: String,
        /// Path to write the versioned training corpus JSON.
        #[arg(long, value_name = "PATH")]
        output: String,
    },
}

/// The corpus schema version. Bumped only on a breaking shape change; a future
/// exporter refuses a corpus whose `schema_version` it does not understand
/// rather than silently mis-reading it (mirrors `SavingsBundle`).
pub const CORPUS_SCHEMA_VERSION: u32 = 1;

/// The verdict-honesty caveat attached to every corpus. P1d can attach ONLY the
/// gate-trust verdict (`gate_committed`: the lossy gate trusted the class at
/// commit time); the richer paired recall-of-baseline verdict is a Phase-2
/// concern — it runs against the *response*, which `content_compress` never
/// sees. This note is surfaced in the corpus so Phase 2 does not mislabel.
pub const VERDICT_NOTE: &str =
    "gate_committed is the only verdict attached: true when the lossy gate trusted \
     the class at commit time (structural backends: always true). The paired \
     recall-of-baseline quality verdict is a Phase-2 concern (it runs against the \
     response, which content_compress never sees).";

/// One captured before/after pair, deserialized from the gateway's JSONL sink.
/// Shape matches `tt_core::content_compress::capture::CaptureRecord`; defined
/// here (not reused) so the CLI's read path is decoupled from the core's write
/// path — the JSON shape is the contract, not the Rust type (mirrors the
/// `SavingsBundle` discipline).
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // read-only deserialization target; fields kept for fidelity
struct CapturedPair {
    schema_version: u32,
    capture_opted_in: bool,
    kind: String,
    before: String,
    after: String,
    // Recorded 0 on the hot path; recomputed below.
    tokens_before: u32,
    tokens_after: u32,
    tokens_removed: u32,
    gate_committed: bool,
    org_id: String,
    trace_id: String,
    model: String,
    provider_id: String,
    ts: String,
}

/// A training pair with the per-block token counts recomputed offline. This is
/// the Phase-2 training unit: `{kind, before, after, tokens_before, tokens_after,
/// gate_committed, ...join keys}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusPair {
    pub kind: String,
    pub before: String,
    pub after: String,
    /// Recomputed via `tt_tokenize` from `before` (authoritative per-block count;
    /// the capture recorded 0).
    pub tokens_before: u32,
    /// Recomputed via `tt_tokenize` from `after`.
    pub tokens_after: u32,
    pub tokens_removed: u32,
    pub gate_committed: bool,
    pub org_id: String,
    pub trace_id: String,
    pub model: String,
    pub provider_id: String,
    pub ts: String,
}

/// The versioned Phase-2 training corpus. Serialized as pretty JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrainingCorpus {
    pub schema_version: u32,
    pub tool_version: String,
    pub produced_at: String,
    /// The verdict-honesty caveat — see [`VERDICT_NOTE`].
    pub note: String,
    pub pairs: Vec<CorpusPair>,
}

impl TrainingCorpus {
    /// The number of pairs in the corpus.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    /// `true` when the corpus has no pairs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }
}

/// Read the capture JSONL at `input`, REFUSE any record not marked opted-in
/// (ZDR), recompute the per-block token counts offline, and write a versioned
/// [`TrainingCorpus`] to `output`. Returns the pair count on success.
///
/// # Errors
/// Returns an error — so the process exits non-zero — when:
/// - the capture file cannot be read/parsed (a malformed line names its number);
/// - a record is not marked `capture_opted_in: true` (ZDR refuse — names the
///   `trace_id`, writes NO output);
/// - the corpus cannot be serialized/written.
///
/// An empty capture file yields an empty corpus (NOT an error) — a valid
/// Phase-1 state when no block compressed.
pub fn run_export(input: &Path, output: &Path) -> anyhow::Result<usize> {
    let raw = std::fs::read_to_string(input)
        .with_context(|| format!("read capture {}", input.display()))?;

    let mut pairs: Vec<CorpusPair> = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        let line_no = i + 1;
        if line.trim().is_empty() {
            continue;
        }
        let rec: CapturedPair =
            serde_json::from_str(line).with_context(|| format!("parse capture line {line_no}"))?;
        // ZDR gate: refuse any record not marked opted-in. A capture record can
        // only exist when the instance opted in, so this should never fire on a
        // well-formed sink — but the export is the ZDR boundary, so it refuses
        // loudly rather than silently emit a pair from a non-opted capture.
        if !rec.capture_opted_in {
            anyhow::bail!(
                "ZDR refuse: capture line {line_no} (trace_id={}) is not marked \
                 capture_opted_in=true; refusing to export a non-opted pair",
                rec.trace_id
            );
        }
        // Recompute the authoritative per-block token counts offline (the hot
        // path recorded 0 to avoid a per-block tokenize).
        let tokens_before =
            tt_tokenize::estimate_tokens_for_model(&rec.provider_id, &rec.model, &rec.before);
        let tokens_after =
            tt_tokenize::estimate_tokens_for_model(&rec.provider_id, &rec.model, &rec.after);
        pairs.push(CorpusPair {
            kind: rec.kind,
            before: rec.before,
            after: rec.after,
            tokens_before,
            tokens_after,
            tokens_removed: tokens_before.saturating_sub(tokens_after),
            gate_committed: rec.gate_committed,
            org_id: rec.org_id,
            trace_id: rec.trace_id,
            model: rec.model,
            provider_id: rec.provider_id,
            ts: rec.ts,
        });
    }

    let corpus = TrainingCorpus {
        schema_version: CORPUS_SCHEMA_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        produced_at: chrono::Utc::now().to_rfc3339(),
        note: VERDICT_NOTE.to_string(),
        pairs,
    };
    let count = corpus.len();
    let json = serde_json::to_string_pretty(&corpus).context("serialize corpus")?;
    std::fs::write(output, json).with_context(|| format!("write corpus {}", output.display()))?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A well-formed opted-in capture record matching the gateway's
    /// `CaptureRecord` JSON shape.
    fn capture_line(kind: &str, before: &str, after: &str, trace_id: &str) -> String {
        let rec = serde_json::json!({
            "schema_version": 1u32,
            "capture_opted_in": true,
            "kind": kind,
            "before": before,
            "after": after,
            "tokens_before": 0u32,
            "tokens_after": 0u32,
            "tokens_removed": 0u32,
            "gate_committed": true,
            "org_id": "00000000-0000-0000-0000-000000000001",
            "trace_id": trace_id,
            "model": "gpt-4o",
            "provider_id": "openai",
            "ts": "2026-07-06T19:00:00Z",
        });
        serde_json::to_string(&rec).unwrap()
    }

    /// N opted-in records → an N-pair corpus with recomputed token counts that
    /// round-trips, and the corpus JSON is well-formed.
    #[test]
    fn export_produces_versioned_corpus_with_recomputed_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("capture.jsonl");
        let output = dir.path().join("corpus.json");

        let before = "{ \"k\": 1 }\n".repeat(20);
        let after = "{\"k\":1}".repeat(20);
        let mut s = String::new();
        s.push_str(&capture_line("json", &before, &after, "trace-1"));
        s.push('\n');
        s.push_str(&capture_line("json", &before, &after, "trace-2"));
        std::fs::write(&input, s).unwrap();

        let count = run_export(&input, &output).expect("export succeeds");
        assert_eq!(count, 2, "two opted-in records → two pairs");

        let raw = std::fs::read_to_string(&output).unwrap();
        let corpus: TrainingCorpus = serde_json::from_str(&raw).expect("corpus round-trips");
        assert_eq!(corpus.schema_version, CORPUS_SCHEMA_VERSION);
        assert_eq!(corpus.pairs.len(), 2);
        assert_eq!(corpus.note, VERDICT_NOTE);
        assert!(
            !corpus.note.is_empty(),
            "the verdict-honesty caveat is attached"
        );

        // The per-block token counts were recomputed (not 0): `after` is shorter
        // → tokens_after < tokens_before, tokens_removed > 0.
        let p = &corpus.pairs[0];
        assert_eq!(p.kind, "json");
        assert_eq!(p.before, before);
        assert_eq!(p.after, after);
        assert_eq!(p.trace_id, "trace-1");
        assert!(p.tokens_before > 0, "tokens_before recomputed");
        assert!(
            p.tokens_after < p.tokens_before,
            "after is shorter → fewer tokens"
        );
        assert!(p.tokens_removed > 0, "tokens_removed recomputed");
        // And the recomputed count matches a direct tt_tokenize call.
        assert_eq!(
            p.tokens_before,
            tt_tokenize::estimate_tokens_for_model("openai", "gpt-4o", &before)
        );
    }

    /// A record with `capture_opted_in: false` → the export REFUSES (ZDR),
    /// names the trace_id, and writes NO output file.
    #[test]
    fn export_refuses_non_opted_pair_zdr() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("capture.jsonl");
        let output = dir.path().join("corpus.json");

        let mut f = std::fs::File::create(&input).unwrap();
        // A well-formed opted-in record, then a NON-opted record.
        writeln!(f, "{}", capture_line("json", "a", "b", "trace-good")).unwrap();
        let non_opted = serde_json::json!({
            "schema_version": 1u32,
            "capture_opted_in": false,
            "kind": "json", "before": "x", "after": "y",
            "tokens_before": 0u32, "tokens_after": 0u32, "tokens_removed": 0u32,
            "gate_committed": true, "org_id": "o", "trace_id": "trace-bad",
            "model": "gpt-4o", "provider_id": "openai", "ts": "2026-07-06T19:00:00Z",
        });
        writeln!(f, "{}", serde_json::to_string(&non_opted).unwrap()).unwrap();

        let err = run_export(&input, &output).expect_err("non-opted record must refuse");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("trace-bad") && msg.contains("ZDR refuse"),
            "the refuse names the trace_id + the ZDR reason: {msg}"
        );
        assert!(!output.exists(), "NO output written on ZDR refuse");
    }

    /// A malformed JSONL line → an error naming the line number.
    #[test]
    fn export_errors_on_malformed_line() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("capture.jsonl");
        let output = dir.path().join("corpus.json");
        std::fs::write(
            &input,
            format!("{}\n{{not json\n", capture_line("json", "a", "b", "t")),
        )
        .unwrap();

        let err = run_export(&input, &output).expect_err("malformed line must error");
        assert!(
            format!("{err:#}").contains("line 2"),
            "the error names the offending line number"
        );
        assert!(!output.exists(), "NO output written on parse error");
    }

    /// An empty capture file → an empty corpus (NOT an error — a valid Phase-1
    /// state when nothing compressed).
    #[test]
    fn export_empty_capture_yields_empty_corpus() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("capture.jsonl");
        let output = dir.path().join("corpus.json");
        std::fs::write(&input, "").unwrap();

        let count = run_export(&input, &output).expect("empty capture is not an error");
        assert_eq!(count, 0, "empty capture → zero pairs");
        let corpus: TrainingCorpus =
            serde_json::from_str(&std::fs::read_to_string(&output).unwrap()).unwrap();
        assert!(corpus.is_empty());
        assert_eq!(
            corpus.schema_version, CORPUS_SCHEMA_VERSION,
            "still versioned"
        );
    }

    /// Blank lines in the sink (a trailing newline, or a gap) are skipped, not
    /// treated as malformed.
    #[test]
    fn export_skips_blank_lines() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("capture.jsonl");
        let output = dir.path().join("corpus.json");
        std::fs::write(
            &input,
            format!(
                "{}\n\n{}\n\n",
                capture_line("json", "a", "b", "t1"),
                capture_line("log", "c", "d", "t2")
            ),
        )
        .unwrap();

        let count = run_export(&input, &output).expect("blank lines are skipped");
        assert_eq!(count, 2, "two real records (blank lines skipped)");
    }
}
