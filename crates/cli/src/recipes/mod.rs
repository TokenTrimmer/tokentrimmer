//! `tt recipes` — curated, ready-to-apply savings route-sets.
//!
//! A *recipe* is a small, named bundle of routing rules that targets one common
//! cost lane (cheap classification, vision gating, cost ceilings, outage
//! failover, long-context downshift). The route bodies are the SAME shape the
//! gateway's `POST /v1/routes` accepts (see `tt_routing::NewRoute`), so applying
//! a recipe is just creating its routes over the user-facing routes API — the
//! exact path `tt route add` uses.
//!
//! The recipe JSON assets live in `crates/cli/recipes/*.json` and are embedded
//! at build time, so the curated set ships with the binary (no runtime fetch).

use anyhow::Context as _;
use serde::Deserialize;
use serde_json::Value;

use crate::context::ResolvedContext;
use crate::ui;

/// One curated recipe: a named bundle of route-set rules plus the human-facing
/// metadata `tt recipes list`/`show` render.
#[derive(Debug, Clone, Deserialize)]
pub struct Recipe {
    /// Stable, CLI-facing identifier (e.g. `cheap-classification`). Used as the
    /// `<name>` argument to `show`/`apply`.
    pub slug: String,
    /// Display title.
    pub name: String,
    /// One-line "what it optimizes" summary for the `list` table.
    pub optimizes: String,
    /// Short tag for the savings lane (downgrade, reroute, cap, failover,
    /// downshift) — surfaced by `show` so the user knows where the savings come
    /// from.
    pub savings_lane: String,
    /// Longer prose description, printed by `show`.
    pub description: String,
    /// The route-set. Each entry is a `tt_routing::NewRoute` body — the exact
    /// JSON the gateway's `POST /v1/routes` accepts.
    pub routes: Vec<Value>,
}

// The curated assets, embedded at build time so the set ships in the binary.
const CHEAP_CLASSIFICATION: &str = include_str!("../../recipes/cheap-classification.json");
const VISION_GATE: &str = include_str!("../../recipes/vision-gate.json");
const COST_CEILING: &str = include_str!("../../recipes/cost-ceiling.json");
const OUTAGE_FALLBACK: &str = include_str!("../../recipes/outage-fallback.json");
const LONG_CONTEXT_DOWNSHIFT: &str = include_str!("../../recipes/long-context-downshift.json");

/// Raw JSON for every curated recipe, in display order.
const RAW_RECIPES: &[&str] = &[
    CHEAP_CLASSIFICATION,
    VISION_GATE,
    COST_CEILING,
    OUTAGE_FALLBACK,
    LONG_CONTEXT_DOWNSHIFT,
];

/// Parse + return the curated recipe set. Panics only on a malformed *embedded*
/// asset, which a unit test guards against — so callers can treat it as
/// infallible at runtime.
#[must_use]
pub fn curated() -> Vec<Recipe> {
    RAW_RECIPES
        .iter()
        .map(|raw| serde_json::from_str(raw).expect("embedded recipe asset is valid JSON"))
        .collect()
}

/// Find a curated recipe by its slug (case-insensitive). Returns `None` when no
/// recipe matches.
#[must_use]
pub fn find(slug: &str) -> Option<Recipe> {
    curated()
        .into_iter()
        .find(|r| r.slug.eq_ignore_ascii_case(slug))
}

/// What `tt recipes` was asked to do.
pub enum RecipesCmd {
    List,
    Show(String),
    Apply(String),
}

// --- pure renderers (unit-tested) -------------------------------------------

/// Render the recipe list as a styled table string.
#[must_use]
pub fn list_table(recipes: &[Recipe]) -> String {
    let mut t = ui::table(&["RECIPE", "OPTIMIZES", "LANE"], console::colors_enabled());
    for r in recipes {
        t.add_row(vec![
            ui::heading_style().apply_to(&r.slug).to_string(),
            r.optimizes.clone(),
            ui::accent().apply_to(&r.savings_lane).to_string(),
        ]);
    }
    format!(
        "{}\n{}",
        ui::format_heading(&format!("RECIPES {} {}", ui::BULLET, recipes.len())),
        t
    )
}

/// Humanize one route body (`NewRoute` JSON) into a single `when → then` line.
#[must_use]
pub fn humanize_route(route: &Value) -> String {
    let mut conds: Vec<String> = Vec::new();
    let when = &route["when"];
    if let Some(models) = when["model_in"].as_array().filter(|a| !a.is_empty()) {
        let list: Vec<&str> = models.iter().filter_map(Value::as_str).collect();
        conds.push(format!("model in [{}]", list.join(", ")));
    }
    if let Some(v) = when["input_tokens_lt"].as_u64() {
        conds.push(format!("input < {v} tok"));
    }
    if let Some(v) = when["input_tokens_gt"].as_u64() {
        conds.push(format!("input > {v} tok"));
    }
    if when["has_images"].as_bool() == Some(true) {
        conds.push("has image".to_string());
    }
    if when["has_audio"].as_bool() == Some(true) {
        conds.push("has audio".to_string());
    }
    if let Some(tag) = when["tag_equals"].as_str() {
        conds.push(format!("tag = {tag}"));
    }
    if let Some(kw) = when["prompt_contains_any_of"]
        .as_array()
        .filter(|a| !a.is_empty())
    {
        let list: Vec<&str> = kw.iter().filter_map(Value::as_str).collect();
        conds.push(format!("prompt contains any of [{}]", list.join(", ")));
    }
    if let Some(v) = when["estimated_cost_gt"].as_f64() {
        conds.push(format!("est. cost > ${v}"));
    }
    if let Some(v) = when["estimated_cost_lt"].as_f64() {
        conds.push(format!("est. cost < ${v}"));
    }
    let when_str = if conds.is_empty() {
        "any request".to_string()
    } else {
        conds.join(" AND ")
    };

    let then = &route["then"];
    let target = then["target_model"].as_str().unwrap_or("?");
    let mut actions = vec![format!("use {target}")];
    if let Some(fb) = then["fallbacks"].as_array().filter(|a| !a.is_empty()) {
        let list: Vec<&str> = fb.iter().filter_map(Value::as_str).collect();
        actions.push(format!("fallbacks → {}", list.join(" → ")));
    }
    if let Some(v) = then["max_cost_usd"].as_f64() {
        actions.push(format!("cap ${v}/call"));
    }
    if then["disable_cache"].as_bool() == Some(true) {
        actions.push("skip cache".to_string());
    }
    if then["flex"].as_bool() == Some(true) {
        actions.push("flex tier".to_string());
    }

    format!("{} {} {}", when_str, ui::ARROW, actions.join(", "))
}

/// Render the full `show <recipe>` view: title, lane, description, and the
/// humanized route-set.
#[must_use]
pub fn show_text(recipe: &Recipe) -> String {
    let mut out = String::new();
    out.push_str(&ui::format_heading(&recipe.name));
    out.push('\n');
    out.push_str(&format!("slug    : {}\n", recipe.slug));
    out.push_str(&format!("lane    : {}\n", recipe.savings_lane));
    out.push('\n');
    out.push_str(&recipe.description);
    out.push_str("\n\n");
    out.push_str(&ui::format_heading(&format!(
        "ROUTES {} {}",
        ui::BULLET,
        recipe.routes.len()
    )));
    out.push('\n');
    for route in &recipe.routes {
        let name = route["name"].as_str().unwrap_or("(unnamed)");
        let prio = route["priority"].as_u64().unwrap_or(100);
        out.push_str(&format!(
            "  {} [prio {}]  {}\n",
            ui::accent().apply_to(name),
            prio,
            humanize_route(route)
        ));
    }
    out
}

// --- apply (network) --------------------------------------------------------

/// Apply a recipe by creating each of its routes over the gateway's
/// `POST /v1/routes` — the same endpoint `tt route add` uses. Returns the number
/// of routes created. Pure w.r.t. the recipe; the caller supplies the HTTP
/// client + resolved base/key so this is mockable in tests.
pub async fn apply_recipe(
    http: &reqwest::Client,
    base: &str,
    key: &str,
    recipe: &Recipe,
) -> anyhow::Result<usize> {
    let base = base.trim_end_matches('/');
    for route in &recipe.routes {
        let _: Value = post_route(http, base, key, route).await?;
    }
    Ok(recipe.routes.len())
}

/// POST one route body and decode the gateway response. Mirrors `route::send`.
async fn post_route(
    http: &reqwest::Client,
    base: &str,
    key: &str,
    body: &Value,
) -> anyhow::Result<Value> {
    let resp = http
        .post(format!("{base}/v1/routes"))
        .bearer_auth(key)
        .json(body)
        .send()
        .await
        .context("request to gateway failed")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let name = body["name"].as_str().unwrap_or("?");
        anyhow::bail!("gateway returned {status} creating route `{name}`: {text}");
    }
    if text.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).context("decode gateway response")
}

// --- dispatch ---------------------------------------------------------------

/// Dispatch a `tt recipes` subcommand.
pub async fn run(
    cmd: RecipesCmd,
    flag_key: Option<String>,
    flag_base: Option<String>,
) -> anyhow::Result<()> {
    match cmd {
        RecipesCmd::List => {
            println!("{}", list_table(&curated()));
            ui::note("Inspect one with `tt recipes show <recipe>`, apply with `tt recipes apply <recipe>`.");
            Ok(())
        }
        RecipesCmd::Show(slug) => {
            let recipe = find(&slug).with_context(|| unknown_recipe_msg(&slug))?;
            print!("{}", show_text(&recipe));
            Ok(())
        }
        RecipesCmd::Apply(slug) => {
            let recipe = find(&slug).with_context(|| unknown_recipe_msg(&slug))?;
            // Resolve the key/base the same way `tt route` does. A missing key is
            // a hard, actionable error — applying writes routes to the hosted
            // gateway, so we never fake success.
            let ctx = ResolvedContext::load(flag_key, flag_base)?;
            let key = ctx.api_key_string().context(
                "no API key — `tt recipes apply` writes routes to the hosted gateway. \
                 Run `tt login --token <KEY>` or set TT_API_KEY first.",
            )?;
            let base = ctx.base_url.trim_end_matches('/').to_string();
            let http = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .context("failed to build HTTP client")?;

            let sp = ui::spinner(&format!("Applying recipe `{}`…", recipe.slug));
            let n = apply_recipe(&http, &base, &key, &recipe).await?;
            drop(sp);
            ui::success(&format!(
                "Applied recipe `{}` — created {n} route{}.",
                recipe.slug,
                if n == 1 { "" } else { "s" }
            ));
            Ok(())
        }
    }
}

/// Actionable "no such recipe" message that lists the valid slugs.
fn unknown_recipe_msg(slug: &str) -> String {
    let slugs: Vec<String> = curated().into_iter().map(|r| r.slug).collect();
    format!(
        "no recipe named `{slug}`. Available: {}. Run `tt recipes list`.",
        slugs.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_recipes_parse_into_valid_route_sets() {
        let recipes = curated();
        assert_eq!(recipes.len(), 5, "five curated recipes");
        for r in &recipes {
            assert!(!r.slug.is_empty());
            assert!(!r.name.is_empty());
            assert!(!r.optimizes.is_empty());
            assert!(!r.savings_lane.is_empty());
            assert!(!r.description.is_empty());
            assert!(!r.routes.is_empty(), "{} has no routes", r.slug);
            // Each route body must deserialize into the REAL gateway type so it
            // applies cleanly — this is the load-bearing schema assertion.
            for route in &r.routes {
                let parsed: tt_routing::NewRoute = serde_json::from_value(route.clone())
                    .unwrap_or_else(|e| panic!("recipe {} route is not a NewRoute: {e}", r.slug));
                assert!(
                    !parsed.then.target_model.is_empty(),
                    "{} route has empty target",
                    r.slug
                );
            }
        }
    }

    #[test]
    fn curated_set_covers_the_five_lanes() {
        let slugs: Vec<String> = curated().into_iter().map(|r| r.slug).collect();
        for expected in [
            "cheap-classification",
            "vision-gate",
            "cost-ceiling",
            "outage-fallback",
            "long-context-downshift",
        ] {
            assert!(slugs.iter().any(|s| s == expected), "missing {expected}");
        }
    }

    #[test]
    fn vision_gate_targets_a_vision_capable_model() {
        // Capability check mirrors the gateway: a has_images route must target a
        // vision model. Validate against the real validator + embedded catalog
        // (look the target up by model id across providers, as the gateway
        // registry does).
        let catalog = tt_shared::model_catalog::model_catalog();
        let lookup = |model: &str| catalog.all().iter().find(|m| m.id == model).cloned();
        let recipe = find("vision-gate").unwrap();
        for route in &recipe.routes {
            let nr: tt_routing::NewRoute = serde_json::from_value(route.clone()).unwrap();
            tt_routing::validate_capability(&nr.when, &nr.then, lookup)
                .expect("vision-gate target must satisfy the capability check");
        }
    }

    #[test]
    fn find_is_case_insensitive_and_misses_cleanly() {
        assert!(find("Cost-Ceiling").is_some());
        assert!(find("nope").is_none());
    }

    #[test]
    fn list_table_shows_every_slug_and_lane() {
        console::set_colors_enabled(false);
        let out = list_table(&curated());
        assert!(out.contains("cheap-classification"));
        assert!(out.contains("vision-gate"));
        assert!(out.contains("cost-ceiling"));
        assert!(out.contains("outage-fallback"));
        assert!(out.contains("long-context-downshift"));
        assert!(out.contains("RECIPES"));
    }

    #[test]
    fn show_text_renders_humanized_routes_and_lane() {
        console::set_colors_enabled(false);
        let recipe = find("cost-ceiling").unwrap();
        let out = show_text(&recipe);
        assert!(out.contains("Cost ceiling"));
        assert!(out.contains("lane    : cap"));
        assert!(out.contains("est. cost > $0.05"));
        assert!(out.contains("use gpt-4o-mini"));
        assert!(out.contains("cap $0.05/call"));
    }

    #[test]
    fn humanize_route_describes_conditions_and_actions() {
        let route = serde_json::json!({
            "name": "x",
            "when": { "has_images": true, "input_tokens_gt": 32000 },
            "then": { "target_model": "gpt-4o", "fallbacks": ["claude-sonnet-4-6"] }
        });
        let s = humanize_route(&route);
        assert!(s.contains("has image"));
        assert!(s.contains("input > 32000 tok"));
        assert!(s.contains("use gpt-4o"));
        assert!(s.contains("fallbacks → claude-sonnet-4-6"));
        assert!(s.contains('→'));
    }

    #[test]
    fn humanize_route_handles_match_all() {
        let route = serde_json::json!({
            "name": "all",
            "when": {},
            "then": { "target_model": "gpt-4o-mini" }
        });
        assert!(humanize_route(&route).starts_with("any request"));
    }

    use httpmock::prelude::*;

    #[tokio::test]
    async fn apply_recipe_posts_each_route_to_v1_routes() {
        // The apply path must dispatch to the REAL routes API — one POST per
        // route — exactly as `tt route add` does. Assert via the mock seam.
        let server = MockServer::start_async().await;
        let m = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/routes")
                .header("authorization", "Bearer tt_live_test");
            then.status(201)
                .header("content-type", "application/json")
                .body(
                    r#"{"id":"00000000-0000-0000-0000-000000000001","name":"recipe:cost-ceiling"}"#,
                );
        });
        let http = reqwest::Client::new();
        let recipe = find("cost-ceiling").unwrap();
        let n = apply_recipe(&http, &server.base_url(), "tt_live_test", &recipe)
            .await
            .unwrap();
        assert_eq!(n, recipe.routes.len());
        m.assert_hits(recipe.routes.len());
    }

    #[tokio::test]
    async fn apply_recipe_sends_the_real_route_body() {
        // The POSTed body must be the recipe's NewRoute, unmodified — proves we
        // apply the curated route-set, not a placeholder.
        let server = MockServer::start_async().await;
        let m = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/routes")
                .json_body_partial(
                    r#"{ "name": "recipe:vision-gate", "when": { "has_images": true }, "then": { "target_model": "gpt-4o" } }"#,
                );
            then.status(201).body("{}");
        });
        let http = reqwest::Client::new();
        let recipe = find("vision-gate").unwrap();
        apply_recipe(&http, &server.base_url(), "tt_live_test", &recipe)
            .await
            .unwrap();
        m.assert();
    }

    #[tokio::test]
    async fn apply_recipe_surfaces_gateway_errors() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST).path("/v1/routes");
            then.status(400).body("bad route");
        });
        let http = reqwest::Client::new();
        let recipe = find("vision-gate").unwrap();
        let err = apply_recipe(&http, &server.base_url(), "k", &recipe)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("400"), "error surfaces status: {msg}");
        assert!(msg.contains("bad route"), "error surfaces body: {msg}");
    }
}
