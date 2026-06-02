//! Cost-diff analysis for CI (`tt inspect --cost-diff`).
//!
//! Given a unified git diff, estimate the projected per-call cost change from
//! the LLM model identifiers added/removed in the diff. The aim is a fast,
//! cloud-free PR check: swapping `gpt-4o` → `gpt-4o-mini` shows up as a
//! projected saving; adding an expensive model shows up as a regression.
//!
//! Rates come from [`tt_preview::pricing`] (the shared catalog) — no network,
//! no cloud. We cannot recover the real prompt size from source, so per-call
//! cost is projected against a fixed [`STD_INPUT_TOKENS`]/[`STD_OUTPUT_TOKENS`]
//! profile and labelled as such; the *delta* between models is what matters.
//!
//! Detection is deliberately simple and documented: a `model`-keyed string
//! assignment on an added/removed line — `model = "…"`, `model: "…"`,
//! `"model": "…"`, `model="…"` (single or double quotes). It intentionally
//! does not try to parse every host language's call graph.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use regex::Regex;
use tt_preview::pricing;

/// Representative input-token count for the projected per-call cost. Fixed so
/// numbers are comparable across PRs (real prompt sizes aren't recoverable
/// from source).
pub const STD_INPUT_TOKENS: u32 = 1_000;
/// Representative output-token count for the projected per-call cost.
pub const STD_OUTPUT_TOKENS: u32 = 500;

/// Per-model cost contribution within a diff.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ModelDelta {
    pub model: String,
    pub provider: String,
    pub input_per_million: f64,
    pub output_per_million: f64,
    /// Projected cost of one call under the standard token profile.
    pub projected_call_usd: f64,
    /// Number of added (`+`) call-sites referencing this model.
    pub added: u32,
    /// Number of removed (`-`) call-sites referencing this model.
    pub removed: u32,
}

/// Result of analysing a diff: per-model deltas, models we couldn't price, and
/// the net projected per-call cost change across the whole diff.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CostDiffReport {
    /// Priced models, sorted by model id.
    pub models: Vec<ModelDelta>,
    /// Model ids referenced in the diff but absent from the pricing catalog.
    pub unknown_models: Vec<String>,
    /// `Σ (added − removed) × projected_call_usd` over all priced models.
    /// Positive = projected cost increase; negative = projected saving.
    pub net_projected_usd: f64,
}

impl CostDiffReport {
    /// Whether the diff projects a net cost increase.
    pub fn is_increase(&self) -> bool {
        self.net_projected_usd > 0.0
    }
}

/// Matches a `model`-keyed string assignment and captures the model id.
fn model_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // (?i): case-insensitive key. The key is a whole word `model` with an
        // optional closing quote (for `"model"`), then `:` or `=`, then a
        // single- or double-quoted model id.
        Regex::new(r#"(?i)\bmodel"?\s*[:=]\s*['"]([A-Za-z0-9._/\-]+)['"]"#)
            .expect("model regex is valid")
    })
}

/// Parse a unified diff and build a [`CostDiffReport`].
pub fn analyze(diff_text: &str) -> CostDiffReport {
    // model -> (added, removed)
    let mut counts: BTreeMap<String, (u32, u32)> = BTreeMap::new();

    for line in diff_text.lines() {
        // Only content +/- lines; skip the `+++`/`---` file headers.
        let (is_add, content) = if line.starts_with("+++") || line.starts_with("---") {
            continue;
        } else if let Some(rest) = line.strip_prefix('+') {
            (true, rest)
        } else if let Some(rest) = line.strip_prefix('-') {
            (false, rest)
        } else {
            continue;
        };

        for cap in model_regex().captures_iter(content) {
            let model = cap[1].to_string();
            let entry = counts.entry(model).or_default();
            if is_add {
                entry.0 += 1;
            } else {
                entry.1 += 1;
            }
        }
    }

    let mut models = Vec::new();
    let mut unknown_models = Vec::new();
    let mut net_projected_usd = 0.0;

    for (model, (added, removed)) in counts {
        match pricing::lookup(&model) {
            Ok(hit) => {
                let projected_call_usd =
                    pricing::cost_usd(STD_INPUT_TOKENS, STD_OUTPUT_TOKENS, &hit);
                net_projected_usd += (f64::from(added) - f64::from(removed)) * projected_call_usd;
                models.push(ModelDelta {
                    model,
                    provider: hit.provider.to_string(),
                    input_per_million: hit.input_per_m,
                    output_per_million: hit.output_per_m,
                    projected_call_usd,
                    added,
                    removed,
                });
            }
            Err(_) => unknown_models.push(model),
        }
    }

    CostDiffReport {
        models,
        unknown_models,
        net_projected_usd,
    }
}

/// Render a [`CostDiffReport`] as markdown suitable for a PR comment / GitHub
/// check-run summary.
pub fn format_markdown(report: &CostDiffReport) -> String {
    let mut out = String::new();
    out.push_str("## 💸 TokenTrimmer cost-diff\n\n");

    if report.models.is_empty() && report.unknown_models.is_empty() {
        out.push_str("No LLM model changes detected in this diff.\n");
        return out;
    }

    let verdict = if report.net_projected_usd > 0.0 {
        format!(
            "⚠️ **Projected cost increase: +${:.6}/call** (standard profile: {STD_INPUT_TOKENS} in / {STD_OUTPUT_TOKENS} out)",
            report.net_projected_usd
        )
    } else if report.net_projected_usd < 0.0 {
        format!(
            "✅ **Projected saving: −${:.6}/call** (standard profile: {STD_INPUT_TOKENS} in / {STD_OUTPUT_TOKENS} out)",
            report.net_projected_usd.abs()
        )
    } else {
        "➖ No net projected per-call cost change.".to_string()
    };
    out.push_str(&verdict);
    out.push_str("\n\n");

    if !report.models.is_empty() {
        out.push_str("| Model | Provider | +added | −removed | $/call (std) |\n");
        out.push_str("|---|---|--:|--:|--:|\n");
        for m in &report.models {
            out.push_str(&format!(
                "| `{}` | {} | {} | {} | ${:.6} |\n",
                m.model, m.provider, m.added, m.removed, m.projected_call_usd
            ));
        }
        out.push('\n');
    }

    if !report.unknown_models.is_empty() {
        out.push_str("> Unpriced models (not in catalog, ignored in totals): ");
        out.push_str(
            &report
                .unknown_models
                .iter()
                .map(|m| format!("`{m}`"))
                .collect::<Vec<_>>()
                .join(", "),
        );
        out.push('\n');
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_swap_as_saving() {
        // Swap a flagship for its mini sibling.
        let diff = r#"
--- a/app.py
+++ b/app.py
@@ -1,3 +1,3 @@
-    resp = client.chat(model="gpt-4o", messages=msgs)
+    resp = client.chat(model="gpt-4o-mini", messages=msgs)
"#;
        let r = analyze(diff);
        // Both models priced, one removed and one added.
        let gpt4o = r.models.iter().find(|m| m.model == "gpt-4o").unwrap();
        let mini = r.models.iter().find(|m| m.model == "gpt-4o-mini").unwrap();
        assert_eq!((gpt4o.added, gpt4o.removed), (0, 1));
        assert_eq!((mini.added, mini.removed), (1, 0));
        // mini is far cheaper than 4o → removing 4o and adding mini saves money.
        assert!(r.net_projected_usd < 0.0, "net = {}", r.net_projected_usd);
        assert!(!r.is_increase());
    }

    #[test]
    fn detects_added_expensive_call_as_increase() {
        let diff = r#"+++ b/x.py
+resp = client.chat(model = 'o3', messages=msgs)
"#;
        let r = analyze(diff);
        let o3 = r.models.iter().find(|m| m.model == "o3").unwrap();
        assert_eq!((o3.added, o3.removed), (1, 0));
        assert!(r.is_increase());
    }

    #[test]
    fn json_key_style_and_unknown_models() {
        let diff = r#"+    "model": "claude-sonnet-4-6",
+    "model": "totally-made-up-model",
"#;
        let r = analyze(diff);
        assert!(r.models.iter().any(|m| m.model == "claude-sonnet-4-6"));
        assert_eq!(r.unknown_models, vec!["totally-made-up-model".to_string()]);
    }

    #[test]
    fn ignores_diff_header_lines() {
        // `+++`/`---` headers mention a path; must never be parsed for models.
        let diff = "--- a/model=\"gpt-4o\".py\n+++ b/model=\"gpt-4o\".py\n context line\n";
        let r = analyze(diff);
        assert!(r.models.is_empty(), "headers must be ignored");
        assert!(r.unknown_models.is_empty());
    }

    #[test]
    fn unchanged_model_nets_to_zero() {
        // Same model added and removed (e.g. a reformat) → net zero.
        let diff = "-x = dict(model=\"gpt-4o\")\n+x = dict(model = \"gpt-4o\")\n";
        let r = analyze(diff);
        let m = r.models.iter().find(|m| m.model == "gpt-4o").unwrap();
        assert_eq!((m.added, m.removed), (1, 1));
        assert!(r.net_projected_usd.abs() < 1e-12);
    }

    #[test]
    fn markdown_renders_verdict_and_table() {
        let diff = "+client.chat(model=\"gpt-4o-mini\")\n";
        let md = format_markdown(&analyze(diff));
        assert!(md.contains("cost-diff"));
        assert!(md.contains("gpt-4o-mini"));
        assert!(md.contains("$/call"));
    }

    #[test]
    fn empty_diff_reports_no_changes() {
        let md = format_markdown(&analyze(" no model here\n+just text\n"));
        assert!(md.contains("No LLM model changes detected"));
    }
}
