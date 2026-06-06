//! AI pass for `tt init --ai`: one grounded model call → tailored `budget.toml`
//! caps + a marker-delimited `AGENTS.md` section. The deterministic init runs
//! first and unchanged; this layer is additive and opt-in.

use std::path::Path;

use anyhow::Context as _;
use serde::Deserialize;

use crate::advise::{detect_models, ModelUsage};
use crate::context::ResolvedContext;
use crate::ui;

const AI_START: &str = "<!-- tt:ai:start -->";
const AI_END: &str = "<!-- tt:ai:end -->";
const DEFAULT_MODEL: &str = "gpt-4o-mini";

const INIT_AI_SYSTEM: &str = "You are a TokenTrimmer setup assistant. Respond with ONLY a single \
JSON object — no prose, no markdown, no code fences. Shape: \
{\"daily_cap_usd\": <number>, \"weekly_cap_usd\": <number>, \
\"routes\": [{\"from\": \"<model>\", \"to\": \"<cheaper-equivalent model>\", \"reason\": \"<short>\"}], \
\"notes\": \"<one sentence>\"}. Recommend a cheaper equivalent for each detected model where one \
exists (use real model names), and sensible daily/weekly USD caps for a small team.";

#[derive(Debug, Deserialize)]
pub(crate) struct AiConfig {
    #[serde(default)]
    pub(crate) daily_cap_usd: Option<f64>,
    #[serde(default)]
    pub(crate) weekly_cap_usd: Option<f64>,
    #[serde(default)]
    pub(crate) routes: Vec<AiRoute>,
    #[serde(default)]
    pub(crate) notes: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AiRoute {
    pub(crate) from: String,
    pub(crate) to: String,
    #[serde(default)]
    pub(crate) reason: Option<String>,
}

/// Extract the JSON object (first `{` … last `}`) and parse it. Tolerates code
/// fences / surrounding prose. `None` on any failure.
fn parse_ai_config(text: &str) -> Option<AiConfig> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str(&text[start..=end]).ok()
}

/// Replace the `daily_cap_usd` / `weekly_cap_usd` value lines with the rounded
/// whole-dollar AI values (only those provided); preserve all other lines +
/// comments. Whole integers keep the bash cost-cap hook parsing them.
fn apply_budget_caps(content: &str, cfg: &AiConfig) -> String {
    let round = |v: f64| -> u64 { v.max(0.0).round() as u64 };
    content
        .lines()
        .map(|line| {
            let key = line.split('=').next().unwrap_or("").trim();
            match key {
                "daily_cap_usd" => cfg
                    .daily_cap_usd
                    .map(|v| format!("daily_cap_usd = {}", round(v)))
                    .unwrap_or_else(|| line.to_string()),
                "weekly_cap_usd" => cfg
                    .weekly_cap_usd
                    .map(|v| format!("weekly_cap_usd = {}", round(v)))
                    .unwrap_or_else(|| line.to_string()),
                _ => line.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the AI section (wrapped in the markers).
fn render_ai_section(detected: &[ModelUsage], cfg: &AiConfig) -> String {
    let mut s = String::new();
    s.push_str(AI_START);
    s.push_str("\n## TokenTrimmer (AI) recommendations\n\n");
    if detected.is_empty() {
        s.push_str("No model usage was detected in the codebase.\n\n");
    } else {
        s.push_str("Detected models:\n");
        for m in detected {
            s.push_str(&format!(
                "- `{}` ({} file(s), e.g. {})\n",
                m.id, m.count, m.example_file
            ));
        }
        s.push('\n');
    }
    if !cfg.routes.is_empty() {
        s.push_str("Recommended routes (apply in your TokenTrimmer dashboard):\n");
        for r in &cfg.routes {
            match &r.reason {
                Some(reason) => {
                    s.push_str(&format!("- `{}` → `{}` — {}\n", r.from, r.to, reason))
                }
                None => s.push_str(&format!("- `{}` → `{}`\n", r.from, r.to)),
            }
        }
        s.push('\n');
    }
    if let Some(notes) = &cfg.notes {
        s.push_str(notes);
        s.push('\n');
    }
    s.push_str(AI_END);
    s
}

/// Insert (append) the section, or replace the existing marked block. Idempotent.
fn upsert_marked_section(content: &str, section: &str) -> String {
    if let (Some(s), Some(e)) = (content.find(AI_START), content.find(AI_END)) {
        let e_end = e + AI_END.len();
        format!("{}{}{}", &content[..s], section, &content[e_end..])
    } else {
        let mut out = content.to_string();
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(section);
        out.push('\n');
        out
    }
}

/// The user message: the detected models + the JSON-shape ask.
fn build_init_context(detected: &[ModelUsage]) -> String {
    let mut s = String::from(
        "Tailor a TokenTrimmer starter config for this repo. Return the JSON object only.\n\n",
    );
    if detected.is_empty() {
        s.push_str("No model usage was detected by scanning the code.\n");
    } else {
        s.push_str("Models referenced in the codebase:\n");
        for m in detected {
            s.push_str(&format!(
                "- {} (in {} file(s), e.g. {})\n",
                m.id, m.count, m.example_file
            ));
        }
    }
    s
}

/// AI pass for `tt init --ai`. Scans the repo, makes one grounded model call, and
/// tailors `.claude/budget.toml` caps + a marked `AGENTS.md` section. Deterministic
/// init must have already run (these files exist). Degrades gracefully: an
/// unparseable model reply or a missing artifact is a warning, not a failure.
///
/// # Errors
/// Missing API key, or a transport/gateway error from the SDK.
pub async fn ai_tailor(
    root: &Path,
    model: Option<String>,
    flag_key: Option<String>,
    flag_base: Option<String>,
) -> anyhow::Result<()> {
    let ctx = ResolvedContext::load(flag_key, flag_base)?;
    let key = ctx
        .api_key_string()
        .context("no API key — run `tt login` or set TT_API_KEY")?;
    let base = ctx.base_url.trim_end_matches('/').to_string();
    let client = tt_client::Client::new(base, key);

    let detected = detect_models(root);
    ui::note(&format!(
        "AI pass: scanned {} model reference(s)",
        detected.len()
    ));

    let out = client
        .chat()
        .model(model.unwrap_or_else(|| DEFAULT_MODEL.to_string()))
        .message(tt_client::system(INIT_AI_SYSTEM))
        .message(tt_client::user(build_init_context(&detected)))
        .send()
        .await?;

    let Some(cfg) = out.text().and_then(parse_ai_config) else {
        ui::warn(
            "AI pass: could not parse the model's JSON — skipped (deterministic init is intact)",
        );
        return Ok(());
    };

    // Tailor .claude/budget.toml caps.
    let budget_path = root.join(".claude/budget.toml");
    if let Ok(content) = std::fs::read_to_string(&budget_path) {
        let updated = apply_budget_caps(&content, &cfg);
        std::fs::write(&budget_path, updated).context("writing .claude/budget.toml")?;
        ui::ok(&format!(
            "tailored .claude/budget.toml (daily={:?}, weekly={:?})",
            cfg.daily_cap_usd, cfg.weekly_cap_usd
        ));
    } else {
        ui::warn("AI pass: .claude/budget.toml not found — skipped");
    }

    // Tailor AGENTS.md (marked section).
    let agents_path = root.join("AGENTS.md");
    if let Ok(content) = std::fs::read_to_string(&agents_path) {
        let updated = upsert_marked_section(&content, &render_ai_section(&detected, &cfg));
        std::fs::write(&agents_path, updated).context("writing AGENTS.md")?;
        ui::ok(&format!(
            "tailored AGENTS.md ({} recommended route(s))",
            cfg.routes.len()
        ));
    } else {
        ui::warn("AI pass: AGENTS.md not found — skipped");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(daily: Option<f64>, weekly: Option<f64>) -> AiConfig {
        AiConfig {
            daily_cap_usd: daily,
            weekly_cap_usd: weekly,
            routes: vec![AiRoute {
                from: "gpt-4o".into(),
                to: "gpt-4o-mini".into(),
                reason: Some("cheaper".into()),
            }],
            notes: Some("Start with these caps.".into()),
        }
    }

    #[test]
    fn parse_ai_config_plain_and_fenced_and_bad() {
        let plain = r#"{"daily_cap_usd": 12, "weekly_cap_usd": 60, "routes": [], "notes": "n"}"#;
        let c = parse_ai_config(plain).expect("plain");
        assert_eq!(c.daily_cap_usd, Some(12.0));
        let fenced = "```json\n{\"daily_cap_usd\": 5}\n```";
        assert_eq!(parse_ai_config(fenced).unwrap().daily_cap_usd, Some(5.0));
        assert!(parse_ai_config("no json here").is_none());
        assert!(parse_ai_config("{not valid}").is_none());
    }

    #[test]
    fn apply_budget_caps_replaces_only_provided_and_preserves_comments() {
        let orig = "# comment\n\ndaily_cap_usd = 10\nweekly_cap_usd = 50\n";
        let out = apply_budget_caps(orig, &cfg(Some(24.7), None));
        assert!(out.contains("# comment"), "{out}");
        assert!(out.contains("daily_cap_usd = 25"), "rounds: {out}");
        assert!(out.contains("weekly_cap_usd = 50"), "untouched: {out}");
        let both = apply_budget_caps(orig, &cfg(Some(3.0), Some(20.0)));
        assert!(both.contains("daily_cap_usd = 3") && both.contains("weekly_cap_usd = 20"));
    }

    #[test]
    fn upsert_marked_section_appends_then_replaces() {
        let base = "# AGENTS\n\nbody\n";
        let sec1 = render_ai_section(&[], &cfg(Some(1.0), Some(2.0)));
        let once = upsert_marked_section(base, &sec1);
        assert_eq!(once.matches(AI_START).count(), 1);
        assert!(once.contains("# AGENTS"));
        let sec2 = render_ai_section(&[], &cfg(Some(9.0), Some(9.0)));
        let twice = upsert_marked_section(&once, &sec2);
        assert_eq!(twice.matches(AI_START).count(), 1, "idempotent: {twice}");
        assert_eq!(twice.matches(AI_END).count(), 1);
    }

    #[test]
    fn render_ai_section_has_models_routes_notes_and_markers() {
        let detected = vec![ModelUsage {
            id: "gpt-4o".into(),
            count: 2,
            example_file: "src/a.rs".into(),
        }];
        let s = render_ai_section(&detected, &cfg(Some(1.0), Some(2.0)));
        assert!(s.starts_with(AI_START) && s.trim_end().ends_with(AI_END));
        assert!(s.contains("gpt-4o"));
        assert!(s.contains("`gpt-4o` → `gpt-4o-mini`"));
        assert!(s.contains("Start with these caps."));
    }
}
