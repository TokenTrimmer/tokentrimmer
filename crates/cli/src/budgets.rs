//! Cost-budget-as-code: a `.tokentrimmer/budgets.toml` that turns the binary PR
//! cost-gate ([`crate::cost_diff`]) into declarative, per-glob ceilings.
//!
//! An offline PR cost-diff sees the per-call cost of *changed model references*
//! and the files they live in — but no request volume — so the budgets here are
//! the two things that ARE honestly computable from a diff:
//!
//! * `[global].max_pr_delta_usd` — a ceiling on the whole-PR net projected
//!   per-call delta (`CostDiffReport::net_projected_usd`). Generalizes the old
//!   binary gate: `0.0` reproduces "fail on any increase", a positive value
//!   grants a tolerance.
//! * `[globs."<pattern>"].max_call_usd` — no ADDED model call in a file matching
//!   the glob may project above this per-call cost. Catches expensive-model
//!   creep in hot paths (e.g. a `gpt-5.5` call added under `src/routes/**`).
//!
//! Per-tag and per-route budgets are intentionally NOT here: tags are runtime
//! request headers and routes live in the gateway config — neither is present in
//! an offline diff. Monthly-budget "burn %" needs request volume the diff lacks.
//!
//! ```toml
//! [global]
//! max_pr_delta_usd = 0.05
//!
//! [globs."src/routes/**"]
//! max_call_usd = 0.02
//!
//! [globs."src/experimental/**"]
//! max_call_usd = 0.20
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use crate::cost_diff::CostDiffReport;

/// A parsed `.tokentrimmer/budgets.toml`.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetFile {
    #[serde(default)]
    pub global: Option<GlobalBudget>,
    /// Glob pattern → ceiling. `BTreeMap` so violations report deterministically.
    #[serde(default)]
    pub globs: BTreeMap<String, GlobBudget>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalBudget {
    /// Ceiling on the whole-PR net projected per-call delta (USD).
    pub max_pr_delta_usd: f64,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobBudget {
    /// No added model call in a file matching this glob may project above this
    /// per-call cost (USD).
    pub max_call_usd: f64,
}

impl BudgetFile {
    /// Load + parse an explicit budget file. Unlike the infallible
    /// `cost-profile.toml` repo-default, an explicitly-pointed budget file is a
    /// money gate: a missing or malformed file is a hard error, never a silent
    /// "no budget".
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read budget file {}: {e}", path.display()))?;
        let parsed: BudgetFile = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parse budget file {}: {e}", path.display()))?;
        Ok(parsed)
    }

    /// True when no budget is declared (nothing to gate on).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.global.is_none() && self.globs.is_empty()
    }
}

/// A single tripped budget.
#[derive(Debug, Clone, PartialEq)]
pub enum Violation {
    /// The whole-PR net projected delta exceeded `[global].max_pr_delta_usd`.
    GlobalDelta { net_usd: f64, limit_usd: f64 },
    /// An added call under a glob exceeded its `max_call_usd` ceiling.
    GlobCall {
        glob: String,
        file: String,
        model: String,
        call_usd: f64,
        limit_usd: f64,
    },
}

/// Evaluate a [`CostDiffReport`] against the budgets. Returns every tripped
/// ceiling (empty = the PR is within budget).
#[must_use]
pub fn check(report: &CostDiffReport, budgets: &BudgetFile) -> Vec<Violation> {
    let mut violations = Vec::new();
    if let Some(g) = &budgets.global {
        if report.net_projected_usd > g.max_pr_delta_usd {
            violations.push(Violation::GlobalDelta {
                net_usd: report.net_projected_usd,
                limit_usd: g.max_pr_delta_usd,
            });
        }
    }
    for call in &report.added_calls {
        for (pattern, gb) in &budgets.globs {
            if call.projected_call_usd > gb.max_call_usd && glob_matches(pattern, &call.file) {
                violations.push(Violation::GlobCall {
                    glob: pattern.clone(),
                    file: call.file.clone(),
                    model: call.model.clone(),
                    call_usd: call.projected_call_usd,
                    limit_usd: gb.max_call_usd,
                });
            }
        }
    }
    violations
}

/// The literal substring the cost-gate Action greps for to fail the check on a
/// budget violation. CONTRACT: keep this in `format_violations`' output (see
/// `verdict_substring_is_gate_contract`).
pub const BUDGET_GATE_SUBSTRING: &str = "Budget exceeded";

/// Render budget violations as a markdown section for the PR comment / check
/// summary. Empty input → an empty string (no section, no false alarm).
#[must_use]
pub fn format_violations(violations: &[Violation]) -> String {
    if violations.is_empty() {
        return String::new();
    }
    let mut out = format!("\n### 🚫 {BUDGET_GATE_SUBSTRING}\n\n");
    for v in violations {
        match v {
            Violation::GlobalDelta { net_usd, limit_usd } => {
                out.push_str(&format!(
                    "- **PR delta** `${net_usd:.4}/call` exceeds `[global].max_pr_delta_usd = ${limit_usd:.4}`\n"
                ));
            }
            Violation::GlobCall {
                glob,
                file,
                model,
                call_usd,
                limit_usd,
            } => {
                out.push_str(&format!(
                    "- `{file}` adds **{model}** at `${call_usd:.4}/call`, over `[globs.\"{glob}\"].max_call_usd = ${limit_usd:.4}`\n"
                ));
            }
        }
    }
    out
}

/// Match a path glob against a repo-relative path. Supports `*` (within a path
/// segment), `**` (across segments, with `**/` also matching zero directories),
/// and `?`. Built on the `regex` crate already in this crate — no new
/// dependency. An invalid pattern matches nothing (fail-closed: it can't
/// silently pass a budget it can't evaluate).
#[must_use]
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    regex::Regex::new(&glob_to_regex(pattern))
        .map(|re| re.is_match(path))
        .unwrap_or(false)
}

fn glob_to_regex(pattern: &str) -> String {
    let mut re = String::with_capacity(pattern.len() + 8);
    re.push('^');
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next(); // consume the second '*'
                    if chars.peek() == Some(&'/') {
                        chars.next(); // `**/` also matches zero leading dirs
                        re.push_str("(?:.*/)?");
                    } else {
                        re.push_str(".*");
                    }
                } else {
                    re.push_str("[^/]*");
                }
            }
            '?' => re.push_str("[^/]"),
            '.' | '+' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '\\' => {
                re.push('\\');
                re.push(c);
            }
            _ => re.push(c),
        }
    }
    re.push('$');
    re
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost_diff::{analyze, AddedCall};

    fn report_with(calls: Vec<AddedCall>, net: f64) -> CostDiffReport {
        CostDiffReport {
            models: vec![],
            unknown_models: vec![],
            net_projected_usd: net,
            added_calls: calls,
        }
    }

    fn call(file: &str, usd: f64) -> AddedCall {
        AddedCall {
            file: file.into(),
            model: "gpt-5.5".into(),
            provider: "openai".into(),
            projected_call_usd: usd,
        }
    }

    #[test]
    fn glob_star_stays_within_a_segment() {
        assert!(glob_matches("src/*.py", "src/app.py"));
        assert!(!glob_matches("src/*.py", "src/sub/app.py"));
    }

    #[test]
    fn glob_doublestar_crosses_segments_and_matches_zero_dirs() {
        assert!(glob_matches("src/routes/**", "src/routes/a.py"));
        assert!(glob_matches("src/routes/**", "src/routes/sub/b.py"));
        assert!(!glob_matches("src/routes/**", "src/other/a.py"));
        // `**/` also matches zero leading directories.
        assert!(glob_matches("**/handlers.py", "handlers.py"));
        assert!(glob_matches("**/handlers.py", "src/api/handlers.py"));
    }

    #[test]
    fn glob_dot_is_literal_not_any_char() {
        assert!(glob_matches("a.py", "a.py"));
        assert!(!glob_matches("a.py", "axpy"));
    }

    #[test]
    fn glob_call_ceiling_trips_only_in_matching_files_over_limit() {
        let budgets = BudgetFile {
            global: None,
            globs: [(
                "src/routes/**".to_string(),
                GlobBudget { max_call_usd: 0.02 },
            )]
            .into_iter()
            .collect(),
        };
        // Over the ceiling, in a matching file → violation.
        let over = report_with(vec![call("src/routes/chat.py", 0.05)], 0.05);
        let v = check(&over, &budgets);
        assert_eq!(v.len(), 1);
        assert!(matches!(v[0], Violation::GlobCall { .. }));
        // Same cost, but a NON-matching file → no violation.
        let elsewhere = report_with(vec![call("src/jobs/chat.py", 0.05)], 0.05);
        assert!(check(&elsewhere, &budgets).is_empty());
        // Matching file but UNDER the ceiling → no violation.
        let under = report_with(vec![call("src/routes/chat.py", 0.01)], 0.01);
        assert!(check(&under, &budgets).is_empty());
    }

    #[test]
    fn global_delta_ceiling_trips_over_limit() {
        let budgets = BudgetFile {
            global: Some(GlobalBudget {
                max_pr_delta_usd: 0.05,
            }),
            globs: BTreeMap::new(),
        };
        assert!(check(&report_with(vec![], 0.06), &budgets).len() == 1);
        // Exactly at the limit is allowed (strict `>`); a saving is fine.
        assert!(check(&report_with(vec![], 0.05), &budgets).is_empty());
        assert!(check(&report_with(vec![], -1.0), &budgets).is_empty());
    }

    #[test]
    fn verdict_substring_is_gate_contract() {
        let v = vec![Violation::GlobalDelta {
            net_usd: 1.0,
            limit_usd: 0.5,
        }];
        assert!(format_violations(&v).contains(BUDGET_GATE_SUBSTRING));
        // No violations → no section, no false gate trip.
        assert!(format_violations(&[]).is_empty());
    }

    #[test]
    fn toml_round_trips_and_rejects_unknown_keys() {
        let toml = r#"
[global]
max_pr_delta_usd = 0.05

[globs."src/routes/**"]
max_call_usd = 0.02
"#;
        let b: BudgetFile = toml::from_str(toml).unwrap();
        assert_eq!(b.global.unwrap().max_pr_delta_usd, 0.05);
        assert_eq!(b.globs["src/routes/**"].max_call_usd, 0.02);
        // A typo'd key must fail loudly, not silently disable a budget.
        assert!(toml::from_str::<BudgetFile>("[global]\nmax_delta = 1.0\n").is_err());
    }

    #[test]
    fn end_to_end_diff_attributes_added_call_to_its_file() {
        // A real unified diff: a gpt-5.5 call added under src/routes/**.
        let diff = "--- a/src/routes/chat.py\n+++ b/src/routes/chat.py\n@@ -1 +1,2 @@\n+resp = client.chat(model=\"gpt-5.5\", messages=msgs)\n";
        let report = analyze(diff);
        assert!(
            report
                .added_calls
                .iter()
                .any(|c| c.file == "src/routes/chat.py" && c.model == "gpt-5.5"),
            "added call must be attributed to its file: {:?}",
            report.added_calls
        );
        let budgets = BudgetFile {
            global: None,
            globs: [(
                "src/routes/**".to_string(),
                GlobBudget {
                    max_call_usd: 0.0001,
                },
            )]
            .into_iter()
            .collect(),
        };
        // A near-zero ceiling under the route glob must trip on the added call.
        assert!(!check(&report, &budgets).is_empty());
    }
}
