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
/// P2a: bumped 1→2 to add `confidence` + `billed_metric_tokens_removed` (the
/// High-confidence-only billed-metric ground-truth + the per-pair Confidence
/// label). A v1 corpus (no confidence / no billed_metric) is NOT mis-read as
/// v2 — a future importer refuses unknown schema versions.
pub const CORPUS_SCHEMA_VERSION: u32 = 2;

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
/// gate_committed, confidence, billed_metric_tokens_removed, ...join keys}`.
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
    /// P2a: the tokenizer Confidence for this pair's provider/model — "high"
    /// (OpenAI tiktoken), "medium" (Anthropic/BPE-proxy ~15-20% off), or "low"
    /// (tiktoken-load-failure → chars/4 → the live gate books $0 on Low). The
    /// billed_metric ground-truth is High-only; Medium rows are kept in the
    /// corpus for training but EXCLUDED from the billed-metric delta column.
    pub confidence: String,
    /// P2a: the billed-reconcilable token delta (Some only on High-confidence
    /// rows). `None` for Medium/Low (Anthropic Medium proxy + Low chars/4 are
    /// NOT billed-reconcilable). Phase-2 training optimizes this where present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billed_metric_tokens_removed: Option<u32>,
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
        // P2a corpus hygiene: post-filter pipeline-REJECTED pairs. The gateway
        // writes the capture INSIDE compact_block (structural.rs) BEFORE the
        // pipeline token-true gate (passes/mod.rs) — so the sink holds rewrites
        // whose `after` never shipped (the gate rolled the tail back). Training
        // on those would be dishonest (a pair whose `after` was rejected is NOT
        // a real compression the model served). Re-run each pair through the
        // pipeline offline (sync + pure + CLI-callable) + drop on rejection.
        if replay_rejected(&rec.before, &rec.provider_id, &rec.model, &rec.after) {
            tracing::debug!(
                line_no,
                trace_id = %rec.trace_id,
                kind = %rec.kind,
                "dropping capture pair whose rewrite the pipeline token-true gate rejected"
            );
            continue;
        }
        // Recompute the authoritative per-block token counts offline (the hot
        // path recorded 0 to avoid a per-block tokenize) WITH the Confidence
        // label so the billed-metric ground-truth can be High-only.
        let est_before =
            tt_tokenize::estimate_input_tokens_for_model(&rec.provider_id, &rec.model, &rec.before);
        let est_after =
            tt_tokenize::estimate_input_tokens_for_model(&rec.provider_id, &rec.model, &rec.after);
        let tokens_before = est_before.tokens;
        let tokens_after = est_after.tokens;
        let confidence: &'static str = confidence_label(est_before.confidence);
        // Billed-metric ground-truth: High-only. Medium (Anthropic ~15-20% off
        // + other-providers BPE-proxy) + Low (chars/4, the live gate books $0)
        // are NOT billed-reconcilable → None (kept in the corpus for training,
        // excluded from the billed-metric delta column).
        let billed_metric_tokens_removed = if est_before.confidence == tt_tokenize::Confidence::High
            && est_after.confidence == tt_tokenize::Confidence::High
        {
            Some(tokens_before.saturating_sub(tokens_after))
        } else {
            None
        };
        pairs.push(CorpusPair {
            kind: rec.kind,
            before: rec.before,
            after: rec.after,
            tokens_before,
            tokens_after,
            tokens_removed: tokens_before.saturating_sub(tokens_after),
            gate_committed: rec.gate_committed,
            confidence: confidence.to_string(),
            billed_metric_tokens_removed,
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

/// P2a corpus hygiene: was this captured `after` actually SHIPPED? The gateway
/// writes the capture INSIDE `compact_block` (structural.rs) BEFORE the pipeline
/// token-true gate (passes/mod.rs), so the sink can hold a captured `after`
/// that the gate then REJECTED (rolling the tail back to `before` verbatim). A
/// pair whose `after` never shipped is NOT a real compression the model served;
/// training on it would be dishonest, so it's dropped.
///
/// Re-runs the captured `before` through the content_compress pass offline
/// (sync + pure + CLI-callable), then compares the pipeline's COMMITTED
/// tail-after to the captured `after` — if they don't match, the captured
/// `after` was not what shipped (the gate rejected it + restored verbatim, or a
/// different transform committed) so the pair is dropped. `compact_block` is
/// `pub(crate)`, so the replay goes through the public `PassPipeline::run`
/// boundary, reading the post-run `req.messages`.
fn replay_rejected(before: &str, provider_id: &str, model: &str, captured_after: &str) -> bool {
    use std::sync::Arc;
    use tt_core::passes::agentic_budget::summarize_judge::NeverCommitGate;
    use tt_core::passes::{PassContext, PassPipeline, SplitRequest};
    use tt_shared::messages::{ChatCompletionRequest, Message, MessageContent};

    let mut req = ChatCompletionRequest {
        model: model.to_string(),
        messages: vec![Message::System {
            content: MessageContent::Text(before.to_string()),
        }],
        ..Default::default()
    };
    let cx = PassContext {
        provider_id,
        model,
        pricing: None,
    };
    let mut split = SplitRequest::compute(&mut req, &cx);
    let _ =
        PassPipeline::content_compress_with_gates(Arc::new(NeverCommitGate)).run(&mut split, &cx);
    // The pass mutates `tail` (which borrows `req`); read the post-run system
    // message text + compare to the captured `after`. A mismatch means the
    // captured `after` did not ship.
    let shipped = match req.messages.first() {
        Some(Message::System {
            content: MessageContent::Text(t),
        }) => t.as_str(),
        _ => before, // non-Text / non-System → treat as verbatim (not rejected)
    };
    shipped != captured_after
}

/// P2a: map a `tt_tokenize::Confidence` to a lowercase string label for the
/// corpus (the JSON shape is the contract; a string avoids a serde-rename on
/// the enum).
#[must_use]
pub fn confidence_label(c: tt_tokenize::Confidence) -> &'static str {
    match c {
        tt_tokenize::Confidence::High => "high",
        tt_tokenize::Confidence::Medium => "medium",
        tt_tokenize::Confidence::Low => "low",
    }
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

    /// A pretty single-JSON-object `before` (clears MIN_BLOB_CHARS + classifies
    /// as ContentKind::Json → the pipeline minifies it) + its canonical minified
    /// `after` (what the pipeline's `minify_json_whitespace` ships). Using
    /// `serde_json`'s canonical minify here matches the pipeline's output BYTE-
    /// for-byte (both validate the JSON + strip inter-token whitespace), so the
    /// post-filter's `shipped != captured_after` check passes for ACCEPTED pairs.
    fn real_json_pair() -> (String, String) {
        let mut obj = String::from("{\n");
        for i in 0..40 {
            obj.push_str(&format!("  \"key_{i}\": \"value {i}\",\n"));
        }
        obj.push_str("  \"last\": true\n}");
        let before = obj;
        // The pipeline's `minify_json_whitespace` preserves key order (it
        // strips inter-token whitespace, NOT a serde round-trip which would
        // reorder keys alphabetically). Compute the `after` by running the
        // pipeline itself — the honest "what the gateway ships" — so the
        // post-filter's `shipped == captured_after` check holds.
        let after = pipeline_minify(&before);
        (before, after)
    }

    /// Run the content_compress pass on a single-System-message request whose
    /// text is `before` + return the shipped system-message text (the pipeline's
    /// committed minify, or verbatim if the pass no-op'd). The test
    /// post-filter mirrors this exact path, so a captured `after` produced here
    /// is byte-identical to what the gateway would ship.
    fn pipeline_minify(before: &str) -> String {
        use std::sync::Arc;
        use tt_core::passes::agentic_budget::summarize_judge::NeverCommitGate;
        use tt_core::passes::{PassContext, PassPipeline, SplitRequest};
        use tt_shared::messages::{ChatCompletionRequest, Message, MessageContent};
        let mut req = ChatCompletionRequest {
            model: "gpt-4o".to_string(),
            messages: vec![Message::System {
                content: MessageContent::Text(before.to_string()),
            }],
            ..Default::default()
        };
        let cx = PassContext {
            provider_id: "openai",
            model: "gpt-4o",
            pricing: None,
        };
        let mut split = SplitRequest::compute(&mut req, &cx);
        let _ = PassPipeline::content_compress_with_gates(Arc::new(NeverCommitGate))
            .run(&mut split, &cx);
        match req.messages.first() {
            Some(Message::System {
                content: MessageContent::Text(t),
            }) => t.clone(),
            _ => before.to_string(),
        }
    }

    /// N opted-in records → an N-pair corpus with recomputed token counts that
    /// round-trips, and the corpus JSON is well-formed.
    #[test]
    fn export_produces_versioned_corpus_with_recomputed_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("capture.jsonl");
        let output = dir.path().join("corpus.json");

        let (before, after) = real_json_pair();
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
        // P2a: the Confidence label is recorded (OpenAI tiktoken = High).
        assert_eq!(p.confidence, "high", "OpenAI → high confidence");
        // P2a: the billed-metric ground-truth is Some(delta) on High-confidence
        // rows (the billed-reconcilable signal Phase-2 training optimizes).
        assert_eq!(
            p.billed_metric_tokens_removed,
            Some(p.tokens_before - p.tokens_after),
            "High-confidence → billed-metric delta is Some"
        );
        // And the recomputed count matches a direct tt_tokenize call.
        assert_eq!(
            p.tokens_before,
            tt_tokenize::estimate_tokens_for_model("openai", "gpt-4o", &before)
        );
    }

    /// P2a corpus hygiene: a captured pair whose `after` the pipeline token-true
    /// gate would REJECT (the rewrite never shipped — the gate rolled the tail
    /// back byte-identical) is DROPPED from the export. Training on a rejected
    /// `after` would be dishonest (it's not a real compression the model
    /// served). The replay runs the `before` through `PassPipeline::run`; if the
    /// gate rejects, the pair is dropped.
    #[test]
    fn export_drops_pair_whose_after_the_pipeline_rejects() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("capture.jsonl");
        let output = dir.path().join("corpus.json");

        // A genuine minify the pipeline ACCEPTS (control): the real_json_pair's
        // `after` matches the pipeline's `minify_json_whitespace` output byte-
        // for-byte, so the post-filter's `shipped == captured_after` keeps it.
        let (good_before, good_after) = real_json_pair();
        // A pair whose captured `after` is NOT what the pipeline would ship: a
        // valid JSON `before` (so the pass classifies + minifies) but an `after`
        // that differs from the pipeline's minified output (e.g. a padded
        // variant). Replayed, the pipeline ships its OWN minify ≠ the captured
        // `after` → the pair is dropped (the captured `after` never shipped).
        let (bad_before, _) = real_json_pair();
        let bad_after =
            format!("{bad_before}\n  // padding the after differs from the pipeline's minify\n");

        let mut s = String::new();
        s.push_str(&capture_line("json", &bad_before, &bad_after, "trace-bad"));
        s.push('\n');
        s.push_str(&capture_line(
            "json",
            &good_before,
            &good_after,
            "trace-good",
        ));
        std::fs::write(&input, s).unwrap();

        let count = run_export(&input, &output).expect("export succeeds");
        assert_eq!(
            count, 1,
            "the rejected pair is dropped; the accepted pair is kept"
        );
        let corpus: TrainingCorpus =
            serde_json::from_str(&std::fs::read_to_string(&output).unwrap()).unwrap();
        assert_eq!(
            corpus.pairs[0].trace_id, "trace-good",
            "only the accepted pair survives"
        );
    }

    /// P2a Confidence filter: an Anthropic-Medium row is KEPT in the corpus
    /// (for training) but its `billed_metric_tokens_removed` is `None` (the
    /// cl100k+correction Medium proxy is ~15-20% off → not billed-reconcilable).
    /// The billed-metric ground-truth is High-only.
    #[test]
    fn export_excludes_anthropic_medium_from_billed_metric() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("capture.jsonl");
        let output = dir.path().join("corpus.json");

        // An OpenAI-High row (billed-metric Some) + an Anthropic-Medium row
        // (billed-metric None, kept in corpus). Both use real_json_pair() so the
        // post-filter accepts them (the captured `after` matches the pipeline's
        // minify); the Confidence filter splits them on provider_id.
        let (before_oai, after_oai) = real_json_pair();
        // A second distinct pair for Anthropic (a different JSON object so the
        // two rows are distinguishable) — same provider_id=anthropic → Medium.
        // The `after` is the pipeline's minify (key-order-preserving, computed
        // via the same path as real_json_pair).
        let (before_ant, after_ant) = {
            let mut obj = String::from("{\n");
            for i in 0..30 {
                obj.push_str(&format!("  \"ant_{i}\": \"v {i}\",\n"));
            }
            obj.push_str("  \"done\": true\n}");
            let after = pipeline_minify(&obj);
            (obj, after)
        };

        let mut s = String::new();
        // Anthropic row — provider_id=anthropic, model=claude-sonnet-4-6 → Medium.
        s.push_str(
            &serde_json::to_string(&serde_json::json!({
                "schema_version": 1u32,
                "capture_opted_in": true,
                "kind": "json",
                "before": before_ant,
                "after": after_ant,
                "tokens_before": 0u32, "tokens_after": 0u32, "tokens_removed": 0u32,
                "gate_committed": true,
                "org_id": "00000000-0000-0000-0000-0000000000a1",
                "trace_id": "trace-anthropic",
                "model": "claude-sonnet-4-6",
                "provider_id": "anthropic",
                "ts": "2026-07-06T19:00:00Z",
            }))
            .unwrap(),
        );
        s.push('\n');
        s.push_str(&capture_line(
            "json",
            &before_oai,
            &after_oai,
            "trace-openai",
        ));
        std::fs::write(&input, s).unwrap();

        let count = run_export(&input, &output).expect("export succeeds");
        assert_eq!(count, 2, "both rows kept in the corpus");
        let corpus: TrainingCorpus =
            serde_json::from_str(&std::fs::read_to_string(&output).unwrap()).unwrap();

        let ant = corpus
            .pairs
            .iter()
            .find(|p| p.trace_id == "trace-anthropic")
            .expect("Anthropic row kept");
        assert_eq!(ant.confidence, "medium", "Anthropic → medium confidence");
        assert_eq!(
            ant.billed_metric_tokens_removed, None,
            "Medium-confidence → billed-metric delta excluded (not billed-reconcilable)"
        );
        assert!(
            ant.tokens_before > ant.tokens_after,
            "the per-block token counts ARE still recomputed (informational, not billed-metric)"
        );

        let oai = corpus
            .pairs
            .iter()
            .find(|p| p.trace_id == "trace-openai")
            .expect("OpenAI row kept");
        assert_eq!(oai.confidence, "high", "OpenAI → high confidence");
        assert_eq!(
            oai.billed_metric_tokens_removed,
            Some(oai.tokens_before - oai.tokens_after),
            "High-confidence → billed-metric delta Some"
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
        // A well-formed opted-in record (before==after → the post-filter accepts
        // it as a verbatim/no-op pair), then a NON-opted record.
        writeln!(f, "{}", capture_line("json", "x", "x", "trace-good")).unwrap();
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
            format!("{}\n{{not json\n", capture_line("json", "x", "x", "t")),
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
                capture_line("json", "x", "x", "t1"),
                capture_line("log", "x", "x", "t2")
            ),
        )
        .unwrap();

        let count = run_export(&input, &output).expect("blank lines are skipped");
        assert_eq!(count, 2, "two real records (blank lines skipped)");
    }
}
