//! CI Action and PR Comment Formatter (`tt ci-comment`).
//!
//! Generates rich GitHub PR markdown comments combining:
//! 1. Cost-diff analysis (model additions, removals, $/call delta, monthly projection).
//! 2. Tier-1 TokenTrimmer AST inspect findings (cache rules, loop tripwires, deprecated models).
//! 3. Actionable route suggestions to optimize PR spend before merging.

use serde::{Deserialize, Serialize};
use tt_inspect_core::Finding;

use crate::cost_diff::{self, CostDiffReport, CostProfile};

/// Configuration options for PR comment formatting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CiCommentConfig {
    /// Assumed monthly call volume for PR cost impact calculations.
    pub monthly_calls: u64,
    /// Whether to fail CI if cost increase exceeds budget threshold.
    pub max_allowed_monthly_increase_usd: Option<f64>,
}

impl Default for CiCommentConfig {
    fn default() -> Self {
        Self {
            monthly_calls: 50_000,
            max_allowed_monthly_increase_usd: Some(100.0),
        }
    }
}

/// Generate a rich, formatted PR comment in Markdown.
#[must_use]
pub fn generate_pr_comment(
    cost_report: &CostDiffReport,
    findings: &[Finding],
    config: &CiCommentConfig,
    _profile: &CostProfile,
) -> String {
    let mut out = String::new();
    out.push_str("### ✂️ TokenTrimmer PR Cost & Waste Audit\n\n");

    let monthly_delta = cost_report.net_projected_usd * (config.monthly_calls as f64);

    // 1. Cost impact banner
    if monthly_delta > 0.01 {
        out.push_str(&format!(
            "⚠️ **Projected Monthly Impact:** `+${:.2}/mo` (+${:.5}/call, based on {}k monthly calls)\n\n",
            monthly_delta,
            cost_report.net_projected_usd,
            config.monthly_calls / 1000
        ));
    } else if monthly_delta < -0.01 {
        out.push_str(&format!(
            "✅ **Projected Monthly Savings:** `−${:.2}/mo` (−${:.5}/call, based on {}k monthly calls)\n\n",
            monthly_delta.abs(),
            cost_report.net_projected_usd.abs(),
            config.monthly_calls / 1000
        ));
    } else {
        out.push_str("➖ **No net projected per-call cost change.**\n\n");
    }

    // 2. Model changes table
    if !cost_report.models.is_empty() {
        out.push_str("#### 📊 Model Changes in PR\n\n");
        out.push_str("| Model | Provider | Added | Removed | $/call (std) |\n");
        out.push_str("|---|---|--:|--:|--:|\n");
        for m in &cost_report.models {
            out.push_str(&format!(
                "| `{}` | {} | {} | {} | ${:.5} |\n",
                m.model, m.provider, m.added, m.removed, m.projected_call_usd
            ));
        }
        out.push('\n');
    }

    // 3. Inspect findings
    if !findings.is_empty() {
        out.push_str(&format!(
            "#### 🔍 TokenTrimmer Findings ({})\n\n",
            findings.len()
        ));
        out.push_str("| Rule | Severity | File | Line | Message |\n");
        out.push_str("|---|---|--:|--:|---|\n");
        for f in findings {
            out.push_str(&format!(
                "| `{}` | `{:?}` | `{}` | {} | {} |\n",
                f.rule_id, f.severity, f.file, f.line, f.message
            ));
        }
        out.push('\n');
    } else {
        out.push_str("✅ **No token-waste or prompt-caching rule violations detected.**\n\n");
    }

    // 4. Recommendations & Router Hints
    if monthly_delta > 0.0 || !findings.is_empty() {
        out.push_str("#### 💡 Suggested Optimizations\n");
        out.push_str("- Enable TokenTrimmer semantic prompt caching to avoid re-billing static system prefixes.\n");
        out.push_str(
            "- Consider routing routine classification / extraction calls to a cheaper model.\n\n",
        );
    }

    out.push_str("> *Verified by TokenTrimmer CI Gate*\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_inspect_core::Severity;

    #[test]
    fn test_generate_pr_comment_renders_savings_and_clean_audit() {
        let report = CostDiffReport {
            models: vec![],
            unknown_models: vec![],
            net_projected_usd: -0.0025,
            added_calls: vec![],
        };
        let findings = vec![];
        let cfg = CiCommentConfig::default();
        let comment = generate_pr_comment(&report, &findings, &cfg, &CostProfile::default());

        assert!(comment.contains("Projected Monthly Savings:"));
        assert!(comment.contains("No token-waste or prompt-caching rule violations detected"));
    }

    #[test]
    fn test_generate_pr_comment_renders_findings() {
        let report = CostDiffReport {
            models: vec![],
            unknown_models: vec![],
            net_projected_usd: 0.005,
            added_calls: vec![],
        };
        let findings = vec![Finding {
            rule_id: "agent-runaway-loop-tripwire".into(),
            severity: Severity::High,
            file: "agent/loop.py".into(),
            line: 42,
            message: "Unbounded loop".into(),
            confidence: 0.9,
            fix_hint: None,
        }];
        let cfg = CiCommentConfig::default();
        let comment = generate_pr_comment(&report, &findings, &cfg, &CostProfile::default());

        assert!(comment.contains("agent-runaway-loop-tripwire"));
        assert!(comment.contains("Projected Monthly Impact:"));
    }
}

/// Options for the `tt ci-comment` command.
#[derive(Debug, Clone)]
pub struct CiCommentOpts {
    /// Path to scope the git diff + findings scan to (relative to the repo root).
    pub path: String,
    /// Base git ref to diff the working tree against.
    pub base: String,
    /// Assumed monthly call volume for the projected-impact figures.
    pub monthly_calls: u64,
    /// Exit non-zero when the projected monthly increase exceeds this USD
    /// amount (the gate the CI wrapper can act on).
    pub max_allowed_monthly_increase_usd: Option<f64>,
}

/// Run the local PR-comment generator: analyze the working-tree diff against
/// `base`, scan Tier-1 findings, and print the markdown comment to stdout.
pub fn run_ci_comment(opts: CiCommentOpts) -> anyhow::Result<()> {
    use std::process::Command as ProcCommand;

    // 1. `git diff <base> -- <path>` — same shape as `tt inspect --cost-diff`.
    let out = ProcCommand::new("git")
        .args(["diff", &opts.base, "--", &opts.path])
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run `git diff` (is git installed?): {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "`git diff {} -- {}` failed: {}",
            opts.base,
            opts.path,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let diff_text = String::from_utf8_lossy(&out.stdout);

    // 2. Per-repo cost profile (`.tokentrimmer/cost-profile.toml`), falling
    //    back to the default standard profile — never breaks the command.
    let repo_root = ProcCommand::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| ".".to_string());
    let profile = CostProfile::load_from_repo(std::path::Path::new(&repo_root));
    let report = cost_diff::analyze_with_profile(&diff_text, &profile);

    // 3. Tier-1 findings scan over the scoped path.
    let mut engine = tt_inspect_core::Engine::new();
    for rule in tt_inspect_rules_tier1::all_rules() {
        engine.add_rule(rule);
    }
    let findings = engine.scan(std::path::Path::new(&opts.path));

    // 4. Render + print (stdout is the machine-readable surface for CI).
    let config = CiCommentConfig {
        monthly_calls: opts.monthly_calls,
        max_allowed_monthly_increase_usd: opts.max_allowed_monthly_increase_usd,
    };
    let comment = generate_pr_comment(&report, &findings, &config, &profile);
    println!("{comment}");

    // 5. Optional gate semantics: exit non-zero when the projected monthly
    //    increase exceeds the allowed ceiling.
    if let Some(max) = opts.max_allowed_monthly_increase_usd {
        let projected = report.net_projected_usd * (opts.monthly_calls as f64);
        if projected > max {
            anyhow::bail!("projected monthly impact ${projected:.2} exceeds the allowed ${max:.2}");
        }
    }
    Ok(())
}
