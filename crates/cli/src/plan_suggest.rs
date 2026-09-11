//! Convert `tt_preview::RouteSuggestion` → `tt_plan_core::ProposedRoute` and
//! build a skeleton [`tt_plan_core::PlanInput`] from a scanned source tree.
//!
//! This is the glue that feeds `tt inspect --suggest-plan`: the inspector scans
//! the codebase for model names, the preview engine generates route suggestions
//! for each, and this module converts those suggestions into `ProposedRoute`
//! entries that the user can drop straight into `tt plan --input <file>`.
//!
//! # Dependency direction
//! `tt-cli` already depends on both `tt-preview` and `tt-plan-core`; the
//! converter lives here to avoid introducing a new crate dependency.

use std::collections::BTreeMap;

use chrono::Utc;
use uuid::Uuid;

use tt_plan_core::types::{ProposedRoute, RouteAction, RouteConditions};
use tt_preview::{route_suggestions, RouteSuggestion};

// ---------------------------------------------------------------------------
// Converter: RouteSuggestion → ProposedRoute
// ---------------------------------------------------------------------------

/// Convert a slice of [`RouteSuggestion`] (all for the same source model) into
/// a `Vec<ProposedRoute>`.
///
/// Each suggestion becomes one route:
/// - `when.model_in` = `[current_model]` (matches the expensive model the code
///   is currently using)
/// - `then.target_model` = the cheaper model the suggestion recommends
/// - `id` is deterministic: `uuid::Uuid::new_v4()` at call time (stable within
///   a single `--suggest-plan` run; users should pin ids before committing)
/// - `priority` starts at 100 and decrements by 10 per suggestion so the
///   cheapest-first ordering from the preview engine is preserved
/// - `name` is the `route` slug from the suggestion (e.g. `"swap-to-claude-haiku-4-5"`)
/// - routes are enabled by default
pub fn suggestions_to_proposed_routes(
    current_model: &str,
    suggestions: &[RouteSuggestion],
) -> Vec<ProposedRoute> {
    suggestions
        .iter()
        .enumerate()
        .map(|(i, s)| ProposedRoute {
            id: Uuid::new_v4(),
            name: s.route.clone(),
            priority: 100u32.saturating_sub(i as u32 * 10),
            enabled: true,
            when: RouteConditions {
                model_in: vec![current_model.to_string()],
                ..Default::default()
            },
            then: RouteAction {
                format_switch: None,
                diff: false,
                target_model: Some(s.model.clone()),
                fallbacks: Vec::new(),
                disable_cache: false,
                max_cost_usd: None,
                flex: false,
                batch: false,
                redact: false,
                traffic_pct: None,
                shadow_model: None,
                auto_pause: false,
                pause_floor_pass_rate: None,
                pause_min_verdicts: None,
                minify_json: false,
                reasoning_max_effort: None,
                reasoning_budget_tokens: None,
                ..Default::default()
            },
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Skeleton PlanInput builder
// ---------------------------------------------------------------------------

/// Standard token profile used when no real usage data is available.
/// Matches the same constants used by `cost_diff` so numbers are comparable.
const STD_INPUT_TOKENS: u32 = 1_000;
/// Standard output token count for projected cost estimates.
const STD_OUTPUT_TOKENS: u32 = 500;

/// Scan `path` for LLM model string literals, generate preview route
/// suggestions for each unique model found, convert them to
/// `ProposedRoute` entries, and return a skeleton `PlanInput` JSON string
/// with `proposed_routes` pre-filled.
///
/// The skeleton has placeholder `requests` / `pricing` fields; users fill
/// those in (or load from Postgres) and pass to `tt plan --input <file>`.
///
/// Returns `Ok(json_string)`.
pub fn build_plan_input_json(path: &str) -> anyhow::Result<String> {
    let now = Utc::now();
    build_plan_input_json_inner(path, Uuid::nil(), &[], now - chrono::Duration::days(7), now)
}

/// Build a `PlanInput` JSON for `path`, injecting a concrete `org_id`, a frozen
/// set of `requests`, and an explicit replay window. `build_plan_input_json`
/// calls this with the skeleton defaults (nil org, empty requests, 7-day
/// window); `--from-db` calls it with a real telemetry window.
pub fn build_plan_input_json_inner(
    path: &str,
    org_id: Uuid,
    requests: &[tt_plan_core::types::RequestLog],
    window_start: chrono::DateTime<Utc>,
    window_end: chrono::DateTime<Utc>,
) -> anyhow::Result<String> {
    let models = collect_models_from_path(path)?;

    let mut proposed_routes: Vec<ProposedRoute> = Vec::new();
    for current_model in &models {
        if let Ok(hit) = tt_preview::pricing::lookup(current_model) {
            let current_cost =
                tt_preview::pricing::cost_usd(STD_INPUT_TOKENS, STD_OUTPUT_TOKENS, &hit);
            // Use Classification as the widest task class; users can narrow
            // routes in the produced file.
            let suggestions = route_suggestions::suggest(
                current_model,
                current_cost,
                STD_INPUT_TOKENS,
                STD_OUTPUT_TOKENS,
                tt_preview::classifier::TaskClass::Classification,
            );
            proposed_routes.extend(suggestions_to_proposed_routes(current_model, &suggestions));
        } else {
            tracing::warn!(
                model = %current_model,
                "model not in pricing catalog — skipping route suggestion"
            );
        }
    }

    // G06: fold in inspect-rule-driven routes — the same tier-1 scan `tt
    // inspect` runs maps actionable findings (flagship-for-classification,
    // extraction, reasoning-effort-defaults-high) to cheaper-model rewrites
    // via `inspect_route_suggest`. Deduplicated by (source, target) against
    // the pricing-catalog routes already collected above, so a model both
    // engines flag produces one route, not two.
    let covered: std::collections::HashSet<(String, String)> = proposed_routes
        .iter()
        .filter_map(|r| {
            Some((
                r.when.model_in.first()?.clone(),
                r.then.target_model.clone()?,
            ))
        })
        .collect();
    for route in finding_routes_from_scan(path) {
        let key = (
            route.when.model_in[0].clone(),
            route.then.target_model.clone().unwrap_or_default(),
        );
        if !covered.contains(&key) {
            proposed_routes.push(route);
        }
    }

    let plan_id = Uuid::new_v4();

    // Build the pricing table for every target model referenced.
    let mut pricing_table: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for route in &proposed_routes {
        // A modifier-only route (`target_model == None`) keeps the caller's
        // model and adds no new model to price; skip it. The suggestion
        // converter only ever emits `Some` targets, so this is defensive.
        let Some(target) = route.then.target_model.as_deref() else {
            continue;
        };
        if let Ok(hit) = tt_preview::pricing::lookup(target) {
            pricing_table
                .entry(format!("{}:{}", hit.provider, target))
                .or_insert_with(|| {
                    serde_json::json!({
                        "input_per_million": hit.input_per_m,
                        "output_per_million": hit.output_per_m,
                        "cached_input_per_million": null
                    })
                });
        }
    }

    let plan_input = serde_json::json!({
        "plan_id": plan_id,
        "org_id": org_id,
        "window_start": window_start.to_rfc3339(),
        "window_end": window_end.to_rfc3339(),
        "requests": requests,
        "proposed_routes": proposed_routes,
        "pricing": pricing_table,
        "config": {
            "l1_ttl_seconds": null,
            "l2_threshold_sweep": [0.85, 0.90, 0.92, 0.95],
            "l2_ttl_seconds": null
        },
        "seed": 42,
        "bootstrap_iterations": 1000
    });

    serde_json::to_string_pretty(&plan_input).map_err(|e| anyhow::anyhow!("serialize: {e}"))
}

/// Run the inspect tier-1 rule scan over `path` and convert its actionable
/// findings to `ProposedRoute`s (G06). Each suggestion becomes one route:
/// `when.model_in` = the expensive model the finding detected, `then.target_model`
/// = the cheaper rewrite, `name` = a `swap-<source>-to-<target>` slug. Priority
/// starts at 50 (below the catalog-priced preview routes at 100-10N) so the
/// preview engine's cheapest-first ordering wins ties; finding routes fill the
/// gaps the pricing catalog alone cannot see (rule-detected workloads).
fn finding_routes_from_scan(path: &str) -> Vec<ProposedRoute> {
    let mut engine = tt_inspect_core::Engine::new();
    for rule in tt_inspect_rules_tier1::all_rules() {
        engine.add_rule(rule);
    }
    let findings = engine.scan(std::path::Path::new(path));
    let suggestions = crate::inspect_route_suggest::route_suggestions_from_findings(&findings);
    suggestions
        .iter()
        .enumerate()
        .map(|(i, s)| ProposedRoute {
            id: Uuid::new_v4(),
            name: format!("swap-{}-to-{}", s.source_model, s.target_model),
            priority: 50u32.saturating_sub(i as u32),
            enabled: true,
            when: RouteConditions {
                model_in: vec![s.source_model.clone()],
                ..Default::default()
            },
            then: RouteAction {
                target_model: Some(s.target_model.clone()),
                ..Default::default()
            },
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Walk `path` (file or directory) and collect unique model strings recognised
/// by the pricing catalog. Uses the same `model`-keyed regex as `cost_diff`.
fn collect_models_from_path(path: &str) -> anyhow::Result<Vec<String>> {
    use std::sync::OnceLock;

    use regex::Regex;

    static MODEL_RE: OnceLock<Regex> = OnceLock::new();
    let re = MODEL_RE.get_or_init(|| {
        Regex::new(r#"(?i)\bmodel"?\s*[:=]\s*['"]([A-Za-z0-9._/\-]+)['"]"#)
            .expect("model regex is valid")
    });

    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    let p = std::path::Path::new(path);
    if p.is_file() {
        collect_from_file(p, re, &mut seen);
    } else if p.is_dir() {
        for entry in walkdir::WalkDir::new(p)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            collect_from_file(entry.path(), re, &mut seen);
        }
    } else {
        anyhow::bail!("path not found or not accessible: {path}");
    }

    Ok(seen.into_iter().collect())
}

fn collect_from_file(
    path: &std::path::Path,
    re: &regex::Regex,
    seen: &mut std::collections::BTreeSet<String>,
) {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    // Only scan source/config files; skip binaries, lock files, etc.
    let relevant = matches!(
        ext.as_str(),
        "py" | "ts" | "js" | "tsx" | "jsx" | "json" | "toml" | "yaml" | "yml" | "md" | "rs"
    );
    if !relevant {
        return;
    }
    let Ok(src) = std::fs::read_to_string(path) else {
        return;
    };
    for cap in re.captures_iter(&src) {
        let model = cap[1].to_string();
        // Only include models that are in the pricing catalog.
        if tt_preview::pricing::lookup(&model).is_ok() {
            seen.insert(model);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use tt_plan_core::types::PlanInput;

    // Helper: build a minimal RouteSuggestion.
    fn make_suggestion(route: &str, model: &str, savings_usd: f64) -> RouteSuggestion {
        RouteSuggestion {
            route: route.to_string(),
            model: model.to_string(),
            cost_usd: 0.001,
            savings_usd,
            quality_risk_band: tt_preview::QualityRiskBand::Unknown,
            rationale: "test".to_string(),
            applicable: true,
        }
    }

    // (a) Converter maps RouteSuggestion to ProposedRoute with correct
    //     source-model condition and target_model.
    #[test]
    fn converter_maps_source_model_and_target() {
        let suggestions = vec![
            make_suggestion("swap-to-haiku", "claude-haiku-4-5", 0.05),
            make_suggestion("swap-to-mini", "gpt-4o-mini", 0.03),
        ];

        let routes = suggestions_to_proposed_routes("claude-sonnet-4-6", &suggestions);

        assert_eq!(routes.len(), 2);

        // First route: matches the source model, targets haiku.
        assert_eq!(routes[0].when.model_in, vec!["claude-sonnet-4-6"]);
        assert_eq!(
            routes[0].then.target_model.as_deref(),
            Some("claude-haiku-4-5")
        );
        assert_eq!(routes[0].name, "swap-to-haiku");
        assert!(routes[0].enabled);
        // Priority decrements: first is higher than second.
        assert!(routes[0].priority > routes[1].priority);

        // Second route: same source model condition.
        assert_eq!(routes[1].when.model_in, vec!["claude-sonnet-4-6"]);
        assert_eq!(routes[1].then.target_model.as_deref(), Some("gpt-4o-mini"));
    }

    // (b) Empty suggestion list yields empty proposed_routes.
    #[test]
    fn empty_suggestions_yield_empty_routes() {
        let routes = suggestions_to_proposed_routes("claude-opus", &[]);
        assert!(routes.is_empty());
    }

    // (c) The emitted PlanInput JSON round-trips and deserializes with the
    //     proposed_routes populated.
    #[test]
    fn plan_input_json_round_trips() {
        // Build a PlanInput directly from two routes to avoid needing real
        // files on disk.
        let suggestions = vec![make_suggestion("swap-to-haiku", "claude-haiku-4-5", 0.05)];
        let routes = suggestions_to_proposed_routes("claude-sonnet-4-6", &suggestions);

        let now = Utc::now();
        let plan_input_value = serde_json::json!({
            "plan_id": Uuid::new_v4(),
            "org_id": Uuid::nil(),
            "window_start": (now - chrono::Duration::days(7)).to_rfc3339(),
            "window_end": now.to_rfc3339(),
            "requests": [],
            "proposed_routes": routes,
            "pricing": {},
            "seed": 42,
            "bootstrap_iterations": 1000
        });

        let json = serde_json::to_string_pretty(&plan_input_value).expect("should serialize");

        // Deserialize as PlanInput — proves the shape is valid.
        let parsed: PlanInput =
            serde_json::from_str(&json).expect("should deserialize as PlanInput");

        assert_eq!(parsed.proposed_routes.len(), 1);
        assert_eq!(
            parsed.proposed_routes[0].when.model_in,
            vec!["claude-sonnet-4-6"]
        );
        assert_eq!(
            parsed.proposed_routes[0].then.target_model.as_deref(),
            Some("claude-haiku-4-5")
        );
    }

    // (d) Single suggestion produces exactly one route with correct fields.
    #[test]
    fn single_suggestion_priority_is_100() {
        let suggestions = vec![make_suggestion("swap-to-mini", "gpt-4o-mini", 0.02)];
        let routes = suggestions_to_proposed_routes("gpt-4o", &suggestions);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].priority, 100);
        assert!(routes[0].when.input_tokens_lt.is_none());
        assert!(routes[0].when.tag_equals.is_none());
    }

    // (e) The inner builder injects org_id + frozen requests into the PlanInput.
    #[test]
    fn inner_builder_freezes_org_and_requests() {
        use tt_plan_core::types::{L2TaskClass, RequestLog};

        let req = RequestLog {
            id: Uuid::new_v4(),
            org_id: Uuid::from_u128(7),
            ts: Utc::now(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
            requested_model: Some("gpt-4o".into()),
            input_tokens: 1000,
            output_tokens: 500,
            cached_tokens: 0,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            cost_usd: 0.005,
            baseline_cost_usd: 0.010,
            cached: false,
            cache_layer: None,
            matched_route_id: None,
            latency_ms: 100,
            upstream_latency_ms: None,
            status: 200,
            tag: None,
            embedding: None,
            finish_reason: None,
            body: None,
            response_body: None,
            task_class: L2TaskClass::default(),
            diff_saved_usd: None,
            minify_saved_est_usd: None,
        };
        let now = Utc::now();
        // A path with no model strings → empty proposed_routes, but requests + org
        // must still be frozen in.
        let dir = std::env::temp_dir();
        let json = build_plan_input_json_inner(
            dir.to_str().unwrap(),
            Uuid::from_u128(7),
            &[req],
            now - chrono::Duration::days(3),
            now,
        )
        .expect("inner builder ok");

        let parsed: PlanInput = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.org_id, Uuid::from_u128(7));
        assert_eq!(parsed.requests.len(), 1);
        assert_eq!(parsed.requests[0].model, "gpt-4o");
    }
    // (g) G06: the inspect-rule scan folds finding-driven routes into the
    //     plan input. A flagship model the pricing CATALOG does not price
    //     (the preview loop warns and skips it) still yields a route when the
    //     classification rule detects it — the rule engine sees what the
    //     catalog cannot.
    #[test]
    fn finding_scan_adds_route_for_unpriced_flagship_model() {
        let dir = std::env::temp_dir().join(format!("tt-g06-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let file = dir.join("moderator.ts");
        std::fs::write(
            &file,
            r#"const classify = async (text: string) => {
  const resp = await client.chat.completions.create({
    model: "gpt-4-turbo",
    messages: [{ role: "user", content: `Label the sentiment of: ${text}` }],
  });
  return resp.choices[0].message.content;
};"#,
        )
        .expect("fixture");

        let json = build_plan_input_json(dir.to_str().unwrap()).expect("plan input");
        let parsed: PlanInput = serde_json::from_str(&json).expect("deserialize");

        let finding_route = parsed
            .proposed_routes
            .iter()
            .find(|r| r.name == "swap-gpt-4-turbo-to-gpt-4o-mini");
        let route = finding_route
            .expect("the unpriced-but-rule-flagged gpt-4-turbo must yield a finding-driven route");
        assert_eq!(route.when.model_in, vec!["gpt-4-turbo"]);
        assert_eq!(route.then.target_model.as_deref(), Some("gpt-4o-mini"));
        // The pricing table carries the TARGET model so the plan is runnable.
        assert!(
            json.contains("\"openai:gpt-4o-mini\""),
            "the target's rate table must be included: {json}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // (h) G06: dedup — when the pricing catalog ALREADY suggested the same
    //     (source, target) pair, the finding does not duplicate it.
    #[test]
    fn finding_scan_dedups_against_the_catalog_routes() {
        let dir = std::env::temp_dir().join(format!("tt-g06-dedup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        // gpt-4o on classification work: flagged by the rule AND priced by
        // the preview catalog (which will suggest gpt-4o-mini among others).
        let file = dir.join("classifier.py");
        std::fs::write(
            &file,
            "def f(text):
    return client.chat.completions.create(
        model=\"gpt-4o\",
        messages=[{\"role\": \"user\", \"content\": f\"Classify the intent of: {text}\"}],
    )
",
        )
        .expect("fixture");

        let json = build_plan_input_json(dir.to_str().unwrap()).expect("plan input");
        let parsed: PlanInput = serde_json::from_str(&json).expect("deserialize");
        // Exactly ONE route rewrites gpt-4o to gpt-4o-mini (the preview
        // engine's suggestion at prio 90; the finding route is deduped).
        let pair_routes: Vec<_> = parsed
            .proposed_routes
            .iter()
            .filter(|r| {
                r.when.model_in.first().map(String::as_str) == Some("gpt-4o")
                    && r.then.target_model.as_deref() == Some("gpt-4o-mini")
            })
            .collect();
        assert_eq!(
            pair_routes.len(),
            1,
            "the (gpt-4o -> gpt-4o-mini) pair must be suggested exactly once: {:?}",
            pair_routes
                .iter()
                .map(|r| r.name.clone())
                .collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
