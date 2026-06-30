//! Cost-diff analysis for CI (`tt inspect --cost-diff`).
//!
//! Given a unified git diff, estimate the projected per-call cost change from
//! the LLM model identifiers added/removed in the diff. The aim is a fast,
//! cloud-free PR check: swapping `gpt-4o` → `gpt-4o-mini` shows up as a
//! projected saving; adding an expensive model shows up as a regression.
//!
//! Rates come from [`tt_preview::pricing`] (the shared catalog) — no network,
//! no cloud. We cannot recover the real prompt size from source, so per-call
//! cost is projected against a fixed token profile ([`CostProfile`], default
//! [`STD_INPUT_TOKENS`]/[`STD_OUTPUT_TOKENS`]) and labelled as such; the *delta*
//! between models is what matters. A repo may override the profile via a
//! `.tokentrimmer/cost-profile.toml` file (see [`CostProfile::load_from_repo`]).
//!
//! **Cross-language by construction.** [`analyze`] operates on the raw unified
//! diff text — it never parses a host language — so the cost-diff gate works on
//! *any* repository's diff regardless of language. (The language-specific lint
//! rule engine is a separate concern.)
//!
//! Detection is deliberately simple and documented. On each added/removed line
//! we match a key that *contains* the word `model` (`model`, `MODEL`,
//! `OPENAI_MODEL`, `model_name`, `chat_model`, `"model"`), a `:` or `=`, then a
//! model id in two forms:
//!
//! * **Quoted** — `model = "…"`, `model: "…"`, `"model": "…"`, `MODEL="…"`
//!   (single or double quotes). Covers code, JSON, YAML and TOML.
//! * **Unquoted** — env/YAML/TOML style such as `OPENAI_MODEL=gpt-4o` or
//!   `model: gpt-4o`. To avoid catching variable names or keywords
//!   (`model = None`, `model = self.default`), an unquoted value must look like
//!   a model id (contain at least one digit, `-`, or `/`).
//!
//! It intentionally does not try to parse every host language's call graph.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use tt_preview::pricing;

/// Representative input-token count for the projected per-call cost. Fixed so
/// numbers are comparable across PRs (real prompt sizes aren't recoverable
/// from source).
pub const STD_INPUT_TOKENS: u32 = 1_000;
/// Representative output-token count for the projected per-call cost.
pub const STD_OUTPUT_TOKENS: u32 = 500;

/// Token profile used to project per-call cost. Defaults to the fixed standard
/// profile ([`STD_INPUT_TOKENS`] / [`STD_OUTPUT_TOKENS`]); a repo may override
/// it (e.g. to reflect its typical prompt size) via a per-repo config file —
/// see [`CostProfile::load_from_repo`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CostProfile {
    /// Representative input-token count per call.
    pub input_tokens: u32,
    /// Representative output-token count per call.
    pub output_tokens: u32,
}

impl Default for CostProfile {
    fn default() -> Self {
        Self {
            input_tokens: STD_INPUT_TOKENS,
            output_tokens: STD_OUTPUT_TOKENS,
        }
    }
}

impl CostProfile {
    /// Load a per-repo token profile from
    /// `<repo_root>/.tokentrimmer/cost-profile.toml` if present, else the
    /// default standard profile.
    ///
    /// Infallible by design: an absent, unreadable, malformed, or partial file
    /// falls back to the default (a bad config must never break the CI gate).
    /// Recognised keys (both optional): `input_tokens`, `output_tokens`.
    #[must_use]
    pub fn load_from_repo(repo_root: &Path) -> Self {
        let path = repo_root.join(".tokentrimmer").join("cost-profile.toml");
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| Self::from_toml_str(&text))
            .unwrap_or_default()
    }

    /// Parse a profile from TOML text, falling back to the default for any
    /// missing or non-positive field. Returns `None` only when the TOML itself
    /// is malformed.
    fn from_toml_str(text: &str) -> Option<Self> {
        #[derive(serde::Deserialize)]
        struct Raw {
            input_tokens: Option<u32>,
            output_tokens: Option<u32>,
        }
        let raw: Raw = toml::from_str(text).ok()?;
        let d = Self::default();
        Some(Self {
            input_tokens: raw
                .input_tokens
                .filter(|v| *v > 0)
                .unwrap_or(d.input_tokens),
            output_tokens: raw
                .output_tokens
                .filter(|v| *v > 0)
                .unwrap_or(d.output_tokens),
        })
    }
}

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

/// One ADDED (`+`) priced model call-site, attributed to the file it was added
/// in. Drives per-glob budget gating (`budgets::check`): each glob's
/// `max_call_usd` ceiling is checked against the `projected_call_usd` of every
/// added call in a matching file. Removals carry no cost, so only additions are
/// collected.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AddedCall {
    /// Repo-relative path of the file the call was added in (the diff's `+++`
    /// target, `b/` prefix and any trailing timestamp stripped).
    pub file: String,
    pub model: String,
    pub provider: String,
    /// Projected cost of this one call under the active token profile.
    pub projected_call_usd: f64,
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
    /// Every added priced call-site with its file + projected per-call cost,
    /// in diff order. Empty when no `budgets.toml` gating is needed; populated
    /// regardless (cheap) so per-glob budget checks have what they need.
    pub added_calls: Vec<AddedCall>,
}

impl CostDiffReport {
    /// Whether the diff projects a net cost increase.
    pub fn is_increase(&self) -> bool {
        self.net_projected_usd > 0.0
    }
}

/// Matches a `model`-keyed assignment with a **quoted** value and captures the
/// model id. The key is any identifier that contains the word `model` (so
/// `model`, `MODEL`, `OPENAI_MODEL`, `model_name`, `chat_model`, and the JSON
/// `"model"` key all qualify), then `:` or `=`, then a single/double-quoted id.
/// Covers code, JSON, YAML and TOML where the model id is quoted.
fn quoted_model_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)\b[a-z0-9_]*model[a-z0-9_]*"?\s*[:=]\s*['"]([A-Za-z0-9._/\-]+)['"]"#)
            .expect("quoted model regex is valid")
    })
}

/// Matches a `model`-keyed assignment with an **unquoted** value (env / YAML /
/// TOML style: `OPENAI_MODEL=gpt-4o`, `MODEL=o3`, `model: gpt-4o`). To avoid
/// catching variable names or language keywords (`model = None`,
/// `model = self.default`), the unquoted value must look like a model id —
/// i.e. contain at least one digit, `-`, or `/`. This is mutually exclusive
/// with [`quoted_model_regex`] on any given assignment (a quoted value starts
/// with `'`/`"`, which this pattern's value class rejects), so the two never
/// double-count the same reference.
fn unquoted_model_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)\b[a-z0-9_]*model[a-z0-9_]*\s*[:=]\s*([a-z0-9._]*[0-9/\-][a-z0-9._/\-]*)"#,
        )
        .expect("unquoted model regex is valid")
    })
}

/// Record one model reference against the added/removed counters.
fn bump(counts: &mut BTreeMap<String, (u32, u32)>, model: &str, is_add: bool) {
    let entry = counts.entry(model.to_string()).or_default();
    if is_add {
        entry.0 += 1;
    } else {
        entry.1 += 1;
    }
}

/// Parse a unified diff and build a [`CostDiffReport`] under the default
/// standard token profile. Cross-language: operates purely on the raw unified
/// diff, so it works on any repository's diff regardless of language.
pub fn analyze(diff_text: &str) -> CostDiffReport {
    analyze_with_profile(diff_text, &CostProfile::default())
}

/// Like [`analyze`] but projects per-call cost against an explicit token
/// [`CostProfile`] (e.g. one loaded from `.tokentrimmer/cost-profile.toml`).
pub fn analyze_with_profile(diff_text: &str, profile: &CostProfile) -> CostDiffReport {
    // model -> (added, removed)
    let mut counts: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    // Added priced calls, attributed to the file they were added in (for
    // per-glob budget gating). Collected as (file, model) during the scan, then
    // priced below.
    let mut added_occ: Vec<(String, String)> = Vec::new();
    // The file the current hunk targets, from the most recent `+++ b/<path>`
    // header. Empty for `/dev/null` (a pure deletion) or before any header.
    let mut current_file = String::new();

    for line in diff_text.lines() {
        // A `+++ ` file header names the target file for the lines that follow.
        if let Some(rest) = line.strip_prefix("+++ ") {
            current_file = normalize_diff_target(rest);
            continue;
        }
        // Only content +/- lines; skip the remaining `+++`/`---` headers.
        let (is_add, content) = if line.starts_with("+++") || line.starts_with("---") {
            continue;
        } else if let Some(rest) = line.strip_prefix('+') {
            (true, rest)
        } else if let Some(rest) = line.strip_prefix('-') {
            (false, rest)
        } else {
            continue;
        };

        // Quoted and unquoted forms are mutually exclusive per assignment, so
        // scanning both never double-counts the same reference.
        for cap in quoted_model_regex().captures_iter(content) {
            bump(&mut counts, &cap[1], is_add);
            if is_add {
                added_occ.push((current_file.clone(), cap[1].to_string()));
            }
        }
        for cap in unquoted_model_regex().captures_iter(content) {
            bump(&mut counts, &cap[1], is_add);
            if is_add {
                added_occ.push((current_file.clone(), cap[1].to_string()));
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
                    pricing::cost_usd(profile.input_tokens, profile.output_tokens, &hit);
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

    // Price each added occurrence (skip unknown models — they carry no budget).
    let added_calls = added_occ
        .into_iter()
        .filter_map(|(file, model)| {
            pricing::lookup(&model).ok().map(|hit| AddedCall {
                file,
                provider: hit.provider.to_string(),
                projected_call_usd: pricing::cost_usd(
                    profile.input_tokens,
                    profile.output_tokens,
                    &hit,
                ),
                model,
            })
        })
        .collect();

    CostDiffReport {
        models,
        unknown_models,
        net_projected_usd,
        added_calls,
    }
}

/// Normalize a unified-diff `+++` target into a repo-relative path: strip the
/// `b/` prefix git emits, drop any trailing `\t<timestamp>`, and map
/// `/dev/null` (a pure deletion) to the empty string.
fn normalize_diff_target(raw: &str) -> String {
    let path = raw.split('\t').next().unwrap_or(raw).trim();
    if path == "/dev/null" {
        return String::new();
    }
    path.strip_prefix("b/").unwrap_or(path).to_string()
}

/// Render a [`CostDiffReport`] as markdown (default standard token profile)
/// suitable for a PR comment / GitHub check-run summary.
pub fn format_markdown(report: &CostDiffReport) -> String {
    format_markdown_with_profile(report, &CostProfile::default())
}

/// Like [`format_markdown`] but labels the projection with the given token
/// [`CostProfile`] (must match the profile passed to [`analyze_with_profile`]).
pub fn format_markdown_with_profile(report: &CostDiffReport, profile: &CostProfile) -> String {
    let (n_in, n_out) = (profile.input_tokens, profile.output_tokens);
    let mut out = String::new();
    out.push_str("## 💸 TokenTrimmer cost-diff\n\n");

    if report.models.is_empty() && report.unknown_models.is_empty() {
        out.push_str("No LLM model changes detected in this diff.\n");
        return out;
    }

    // CONTRACT: the cost-gate GitHub Action (`inspect-action/action.yml`)
    // decides pass/fail by grepping the rendered report for the literal
    // substring "Projected cost increase". Do not reword the increase verdict
    // below without updating that action and the `verdict_string_is_gate_contract`
    // test, or the gate will silently stop failing.
    let verdict = if report.net_projected_usd > 0.0 {
        format!(
            "⚠️ **Projected cost increase: +${:.6}/call** (profile: {n_in} in / {n_out} out)",
            report.net_projected_usd
        )
    } else if report.net_projected_usd < 0.0 {
        format!(
            "✅ **Projected saving: −${:.6}/call** (profile: {n_in} in / {n_out} out)",
            report.net_projected_usd.abs()
        )
    } else {
        "➖ No net projected per-call cost change.".to_string()
    };
    out.push_str(&verdict);
    out.push_str("\n\n");

    // Framing notes. The cross-language note (PROD-11) and the fixed-profile
    // caveat (PROD-8) MUST NOT contain the literal "Projected cost increase"
    // (the gate substring above), or a green diff would falsely trip the gate.
    out.push_str(&format!(
        "> ℹ️ Cross-language: this gate reads the raw unified diff, so it works on **any** repo's diff regardless of language.\n>\n> ⚠️ Caveat: a green result does **not** guarantee the change is free of any possible cost regression. Costs use a fixed {n_in}/{n_out}-token profile and do not see prompt-size changes or per-call volume.\n\n"
    ));

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

    #[test]
    fn verdict_string_is_gate_contract() {
        // The cost-gate GitHub Action (`inspect-action/action.yml`) decides
        // pass/fail by grepping the rendered report for this exact substring.
        // If this assertion fails because the verdict copy changed, update the
        // action's grep AND the cross-reference comment above the verdict.
        const GATE_SUBSTRING: &str = "Projected cost increase";

        // An increase MUST render the gate substring.
        let increase = "+resp = client.chat(model = 'o3', messages=msgs)\n";
        let inc_md = format_markdown(&analyze(increase));
        assert!(
            analyze(increase).is_increase(),
            "fixture must project an increase"
        );
        assert!(
            inc_md.contains(GATE_SUBSTRING),
            "increase verdict must contain the gate substring {GATE_SUBSTRING:?}; \
             got:\n{inc_md}"
        );

        // A saving MUST NOT render the gate substring (else false-positive gate).
        let saving = "-client.chat(model=\"gpt-4o\")\n+client.chat(model=\"gpt-4o-mini\")\n";
        let sav_md = format_markdown(&analyze(saving));
        assert!(
            !analyze(saving).is_increase(),
            "fixture must project a saving"
        );
        assert!(
            !sav_md.contains(GATE_SUBSTRING),
            "saving verdict must not contain the gate substring; got:\n{sav_md}"
        );

        // No change MUST NOT render the gate substring either.
        let no_change = format_markdown(&analyze("+just text\n"));
        assert!(!no_change.contains(GATE_SUBSTRING));
    }

    // ── PROD-8: extended detection forms ───────────────────────────────────────

    #[test]
    fn detects_uppercase_constant_assignment() {
        // `MODEL = "…"` — a constant/config assignment, not an inline call.
        let r = analyze("+MODEL = \"gpt-4o\"\n");
        let m = r.models.iter().find(|m| m.model == "gpt-4o").unwrap();
        assert_eq!((m.added, m.removed), (1, 0));
    }

    #[test]
    fn detects_env_style_unquoted_values() {
        // `.env` / shell style: prefixed key, unquoted value, no spaces.
        let diff = "+OPENAI_MODEL=gpt-4o\n+MODEL=o3\n";
        let r = analyze(diff);
        assert!(
            r.models.iter().any(|m| m.model == "gpt-4o" && m.added == 1),
            "OPENAI_MODEL=gpt-4o should be detected; got {:?}",
            r.models
        );
        assert!(r.models.iter().any(|m| m.model == "o3" && m.added == 1));
    }

    #[test]
    fn detects_unquoted_yaml_value() {
        // YAML config with an unquoted scalar value.
        let r = analyze("+  model: claude-sonnet-4-6\n");
        assert!(
            r.models.iter().any(|m| m.model == "claude-sonnet-4-6"),
            "unquoted YAML model value should be detected; got {:?}",
            r.models
        );
    }

    #[test]
    fn detects_prefixed_quoted_constant() {
        // `LLM_MODEL: "…"` — prefixed identifier, quoted value (TOML/JSON-ish).
        let r = analyze("+LLM_MODEL = \"gpt-4o-mini\"\n");
        assert!(r.models.iter().any(|m| m.model == "gpt-4o-mini"));
    }

    #[test]
    fn unquoted_ignores_non_model_values() {
        // Variable assignments / keywords whose RHS is not a model id must NOT
        // be captured (no digit/`-`/`/` in the value).
        let diff = "+model = None\n+model = self.default\n+model = chosen_model\n";
        let r = analyze(diff);
        assert!(r.models.is_empty(), "models = {:?}", r.models);
        assert!(
            r.unknown_models.is_empty(),
            "unknown = {:?}",
            r.unknown_models
        );
    }

    #[test]
    fn quoted_and_unquoted_do_not_double_count() {
        // A quoted assignment must be counted exactly once (only the quoted
        // regex fires; the unquoted regex's value class rejects the leading
        // quote).
        let r = analyze("+model = \"gpt-4o\"\n");
        let m = r.models.iter().find(|m| m.model == "gpt-4o").unwrap();
        assert_eq!((m.added, m.removed), (1, 0), "must count once, not twice");
    }

    // ── PROD-8: fixed-profile caveat + PROD-11 cross-language framing ──────────

    #[test]
    fn markdown_includes_caveat_and_cross_language_note() {
        // A genuine saving (swap flagship → mini) → green verdict.
        let saving = "-client.chat(model=\"gpt-4o\")\n+client.chat(model=\"gpt-4o-mini\")\n";
        let report = analyze(saving);
        assert!(!report.is_increase(), "fixture must project a saving");
        let md = format_markdown(&report);
        // Fixed-profile caveat present.
        assert!(
            md.contains("Caveat") && md.contains("fixed") && md.contains("profile"),
            "markdown should carry the fixed-profile caveat; got:\n{md}"
        );
        // Cross-language framing present.
        assert!(
            md.contains("Cross-language") && md.contains("any"),
            "markdown should frame the gate as cross-language; got:\n{md}"
        );
        // The caveat/notes must never contain the gate substring on a green
        // diff, or it would falsely trip the fail-on-increase gate.
        assert!(
            !md.contains("Projected cost increase"),
            "a saving diff must not contain the gate substring; got:\n{md}"
        );
    }

    // ── PROD-8: per-repo token profile override ────────────────────────────────

    #[test]
    fn profile_overrides_projected_cost() {
        let diff = "+client.chat(model=\"gpt-4o\")\n";
        let default_report = analyze(diff);
        // Double both token counts → projected per-call cost (and net) doubles.
        let doubled = CostProfile {
            input_tokens: STD_INPUT_TOKENS * 2,
            output_tokens: STD_OUTPUT_TOKENS * 2,
        };
        let doubled_report = analyze_with_profile(diff, &doubled);
        let d0 = default_report.models[0].projected_call_usd;
        let d1 = doubled_report.models[0].projected_call_usd;
        assert!(d0 > 0.0, "default projected cost should be positive");
        assert!(
            (d1 - 2.0 * d0).abs() < 1e-12,
            "doubling the profile should double the projection: {d0} -> {d1}"
        );
    }

    #[test]
    fn cost_profile_from_toml_full_partial_and_malformed() {
        // Full override.
        let full = CostProfile::from_toml_str("input_tokens = 4000\noutput_tokens = 1000\n")
            .expect("valid toml");
        assert_eq!((full.input_tokens, full.output_tokens), (4000, 1000));

        // Partial: only input given → output falls back to default.
        let partial = CostProfile::from_toml_str("input_tokens = 4000\n").expect("valid toml");
        assert_eq!(partial.input_tokens, 4000);
        assert_eq!(partial.output_tokens, STD_OUTPUT_TOKENS);

        // Non-positive values fall back to default.
        let zero = CostProfile::from_toml_str("input_tokens = 0\noutput_tokens = 0\n")
            .expect("valid toml");
        assert_eq!(zero, CostProfile::default());

        // Malformed TOML → None (caller falls back to default).
        assert!(CostProfile::from_toml_str("input_tokens = = oops").is_none());

        // Empty / unrelated keys → default.
        assert_eq!(
            CostProfile::from_toml_str("unrelated = 1\n").unwrap(),
            CostProfile::default()
        );
    }

    #[test]
    fn load_from_repo_defaults_when_absent_and_reads_when_present() {
        let dir = tempfile::tempdir().unwrap();
        // Absent file → default.
        assert_eq!(
            CostProfile::load_from_repo(dir.path()),
            CostProfile::default()
        );

        // Present file → parsed.
        let cfg_dir = dir.path().join(".tokentrimmer");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("cost-profile.toml"),
            "input_tokens = 8000\noutput_tokens = 2000\n",
        )
        .unwrap();
        let loaded = CostProfile::load_from_repo(dir.path());
        assert_eq!((loaded.input_tokens, loaded.output_tokens), (8000, 2000));
    }

    #[test]
    fn markdown_with_profile_shows_profile_numbers() {
        let diff = "+client.chat(model=\"gpt-4o-mini\")\n";
        let profile = CostProfile {
            input_tokens: 3000,
            output_tokens: 700,
        };
        let md = format_markdown_with_profile(&analyze_with_profile(diff, &profile), &profile);
        assert!(
            md.contains("3000 in / 700 out"),
            "verdict should reflect the custom profile; got:\n{md}"
        );
        assert!(
            md.contains("3000/700-token profile"),
            "caveat should reflect the custom profile; got:\n{md}"
        );
    }
}
