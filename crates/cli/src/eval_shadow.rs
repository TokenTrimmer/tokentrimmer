//! `tt eval shadow` — offline shadow-log analysis for the P2c recall eval.
//!
//! Reads the structured `tt::compress::shadow` JSONL log (produced by the
//! P2b DARK SHADOW path) + joins `trace_id` to `quality_verdicts` (the RUNG 3
//! judge gold) + computes the recall-vs-deterministic delta. The output is a
//! JSON report the operator uses to decide P2d promotion.

/// The `tt eval` subcommands.
#[derive(clap::Subcommand)]
pub enum EvalAction {
    /// Evaluate the shadow log against judge verdicts (the P2d promotion gate).
    Shadow {
        /// Path to the structured `tt::compress::shadow` JSONL log.
        #[arg(long, value_name = "PATH")]
        shadow: String,
        /// Path to the `quality_verdicts` JSONL (the RUNG 3 judge gold).
        #[arg(long, value_name = "PATH")]
        verdicts: String,
        /// Path to write the JSON report.
        #[arg(long, value_name = "PATH")]
        output: String,
    },
}

use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// One row of the shadow JSONL (the structured `tt::compress::shadow` log).
#[derive(Debug, Clone, Deserialize)]
struct ShadowRow {
    content_hash: String,
    p1b_tokens_removed: usize,
    learned_tokens_removed: Option<usize>,
    // The trace_id is the join key to quality_verdicts (carried as a field
    // on the `tracing` structured event — the shadow emits it from the
    // `CaptureCtx` which carries the request's `trace_id`).
    trace_id: Option<String>,
}

/// One row of the quality_verdicts JSONL (the RUNG 3 judge gold).
#[derive(Debug, Clone, Deserialize)]
struct VerdictRow {
    request_id: String,
    verdict: String, // "acceptable" / "degraded" / "unclear"
}

/// The eval report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalReport {
    pub total_shadow_rows: usize,
    pub joined_verdicts: usize,
    pub p1b_total_tokens_removed: usize,
    pub learned_total_tokens_removed: usize,
    pub learned_acceptable_count: usize,
    pub p1b_acceptable_count: usize,
    /// The POSITIVE recall bar: the % of joined traces where the learned
    /// candidate removed MORE tokens than P1b AND the verdict was Acceptable.
    /// A value > 0 is a genuine improvement; 0 means the learned model never
    /// beat P1b on the held-out set. Promotion (P2d) requires this > 0.
    pub positive_recall_bar_pct: f64,
    /// Per-trace details (for the operator's inspection).
    pub traces: Vec<TraceDetail>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceDetail {
    pub trace_id: String,
    pub content_hash: String,
    pub p1b_tokens_removed: usize,
    pub learned_tokens_removed: Option<usize>,
    pub verdict: String,
    pub learned_beats_p1b: bool,
}

/// `tt eval shadow --input <shadow.jsonl> --verdicts <verdicts.jsonl> --output <report.json>`
///
/// Reads the shadow log + the verdicts, joins by `trace_id`, and emits a report
/// with the positive recall bar + per-trace details.
///
/// # Errors
/// Returns an error on a read/parse failure (a malformed line names its number).
pub fn run_eval_shadow(
    shadow_path: &Path,
    verdicts_path: &Path,
    output: &Path,
) -> anyhow::Result<()> {
    let shadow_raw = std::fs::read_to_string(shadow_path)
        .with_context(|| format!("read shadow {}", shadow_path.display()))?;
    let verdicts_raw = std::fs::read_to_string(verdicts_path)
        .with_context(|| format!("read verdicts {}", verdicts_path.display()))?;

    // Parse the shadow rows.
    let mut shadow_rows: Vec<ShadowRow> = Vec::new();
    for (i, line) in shadow_raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        // The shadow log is a `tracing` structured event; the JSON may be
        // embedded in the event line. Extract the JSON object.
        let json_str = extract_json(line).unwrap_or(line);
        let row: ShadowRow = serde_json::from_str(json_str)
            .with_context(|| format!("parse shadow line {}", i + 1))?;
        shadow_rows.push(row);
    }

    // Parse the verdict rows + index by request_id (the trace_id join key).
    let mut verdicts: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for (i, line) in verdicts_raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: VerdictRow =
            serde_json::from_str(line).with_context(|| format!("parse verdict line {}", i + 1))?;
        verdicts.insert(row.request_id, row.verdict);
    }

    // Join + compute.
    let mut traces: Vec<TraceDetail> = Vec::new();
    let mut p1b_total = 0usize;
    let mut learned_total = 0usize;
    let mut learned_acceptable = 0usize;
    let mut p1b_acceptable = 0usize;

    for row in &shadow_rows {
        let trace_id = row.trace_id.clone().unwrap_or_default();
        let verdict = verdicts
            .get(&trace_id)
            .cloned()
            .unwrap_or_else(|| "?".into());
        let learned_beats = row
            .learned_tokens_removed
            .map(|l| l > row.p1b_tokens_removed)
            .unwrap_or(false);

        p1b_total += row.p1b_tokens_removed;
        if let Some(l) = row.learned_tokens_removed {
            learned_total += l;
        }
        if verdict == "acceptable" {
            p1b_acceptable += 1;
            if learned_beats {
                learned_acceptable += 1;
            }
        }

        traces.push(TraceDetail {
            trace_id,
            content_hash: row.content_hash.clone(),
            p1b_tokens_removed: row.p1b_tokens_removed,
            learned_tokens_removed: row.learned_tokens_removed,
            verdict,
            learned_beats_p1b: learned_beats,
        });
    }

    let joined = traces.iter().filter(|t| t.verdict != "?").count();
    let positive_bar = if joined > 0 {
        (learned_acceptable as f64 / joined as f64) * 100.0
    } else {
        0.0
    };

    let report = EvalReport {
        total_shadow_rows: shadow_rows.len(),
        joined_verdicts: joined,
        p1b_total_tokens_removed: p1b_total,
        learned_total_tokens_removed: learned_total,
        learned_acceptable_count: learned_acceptable,
        p1b_acceptable_count: p1b_acceptable,
        positive_recall_bar_pct: positive_bar,
        traces,
    };

    let json = serde_json::to_string_pretty(&report).context("serialize report")?;
    std::fs::write(output, json).with_context(|| format!("write report {}", output.display()))?;

    eprintln!(
        "eval: {} shadow rows, {} joined verdicts, positive_recall_bar={:.1}% (learned beats P1b AND acceptable: {}/{})",
        report.total_shadow_rows, report.joined_verdicts, report.positive_recall_bar_pct,
        report.learned_acceptable_count, report.p1b_acceptable_count
    );

    Ok(())
}

/// Extract the JSON object from a tracing structured-log line (the line may
/// be `2026-07-08T... INFO tt::compress::shadow: {"content_hash":...}`).
fn extract_json(line: &str) -> Option<&str> {
    line.find('{').and_then(|start| {
        let end = line.rfind('}')?;
        if end > start {
            Some(&line[start..=end])
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shadow_line(content_hash: &str, p1b: usize, learned: Option<usize>, trace: &str) -> String {
        serde_json::json!({
            "content_hash": content_hash,
            "p1b_tokens_removed": p1b,
            "learned_tokens_removed": learned,
            "trace_id": trace,
        })
        .to_string()
    }

    fn verdict_line(trace: &str, verdict: &str) -> String {
        serde_json::json!({"request_id": trace, "verdict": verdict}).to_string()
    }

    #[test]
    fn eval_joins_and_computes_positive_bar() {
        let dir = tempfile::tempdir().unwrap();
        let shadow = dir.path().join("shadow.jsonl");
        let verdicts = dir.path().join("verdicts.jsonl");
        let report = dir.path().join("report.json");

        std::fs::write(
            &shadow,
            format!(
                "{}\n{}\n{}\n",
                shadow_line("h1", 100, Some(120), "t1"),
                shadow_line("h2", 80, Some(60), "t2"),
                shadow_line("h3", 50, None, "t3"), // no learned candidate
            ),
        )
        .unwrap();

        std::fs::write(
            &verdicts,
            format!(
                "{}\n{}\n",
                verdict_line("t1", "acceptable"),
                verdict_line("t2", "degraded"),
            ),
        )
        .unwrap();

        run_eval_shadow(&shadow, &verdicts, &report).expect("eval succeeds");

        let raw = std::fs::read_to_string(&report).unwrap();
        let r: EvalReport = serde_json::from_str(&raw).unwrap();
        assert_eq!(r.total_shadow_rows, 3);
        assert_eq!(r.joined_verdicts, 2); // t1+t2 joined; t3 has no verdict
                                          // t1: learned(120) > p1b(100) AND acceptable → counts toward the positive bar.
        assert_eq!(r.learned_acceptable_count, 1);
        assert_eq!(r.p1b_acceptable_count, 1); // t1 acceptable
        assert!(
            r.positive_recall_bar_pct > 0.0,
            "positive bar > 0 (t1 beats P1b + acceptable)"
        );
    }

    #[test]
    fn eval_empty_shadow_yields_empty_report() {
        let dir = tempfile::tempdir().unwrap();
        let shadow = dir.path().join("empty.jsonl");
        let verdicts = dir.path().join("v.jsonl");
        let report = dir.path().join("r.json");
        std::fs::write(&shadow, "").unwrap();
        std::fs::write(&verdicts, "").unwrap();
        run_eval_shadow(&shadow, &verdicts, &report).expect("empty = not an error");
        let r: EvalReport =
            serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
        assert_eq!(r.total_shadow_rows, 0);
        assert_eq!(r.positive_recall_bar_pct, 0.0);
    }

    #[test]
    fn extract_json_from_tracing_line() {
        let line = r#"2026-07-08T14:00:00Z INFO tt::compress::shadow: {"content_hash":"abc","p1b_tokens_removed":100,"learned_tokens_removed":120,"trace_id":"t1"}"#;
        let json = extract_json(line).unwrap();
        assert!(json.contains("\"content_hash\""));
        let row: ShadowRow = serde_json::from_str(json).unwrap();
        assert_eq!(row.content_hash, "abc");
        assert_eq!(row.p1b_tokens_removed, 100);
        assert_eq!(row.learned_tokens_removed, Some(120));
    }
}
