//! `tt route` — manage routing rules via the gateway's user-facing /v1/routes
//! API, authenticated with the V0-resolved key.

use anyhow::Context as _;
use serde_json::{json, Value};

use crate::context::ResolvedContext;
use crate::ui;

/// Flags for `tt route add`. Mirrors the clap args in `main.rs`.
pub struct AddArgs {
    pub always: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub when_has_images: bool,
    pub when_has_audio: bool,
    pub when_tag: Option<String>,
    pub when_prompt_contains: Vec<String>,
    pub when_cost_gt: Option<f64>,
    pub when_cost_lt: Option<f64>,
    pub max_cost: Option<f64>,
    pub disable_cache: bool,
    pub priority: u32,
    pub name: Option<String>,
    pub fallback: Vec<String>,
    pub disabled: bool,
}

/// Pure: map `add` flags to the `NewRoute` JSON body the API expects.
pub fn build_new_route(args: &AddArgs) -> anyhow::Result<Value> {
    let target = match (&args.always, &args.to) {
        (Some(m), None) => m.clone(),
        (None, Some(m)) => m.clone(),
        (Some(_), Some(_)) => anyhow::bail!("use either --always or --to, not both"),
        (None, None) => {
            anyhow::bail!("a target is required: pass --always <model> or --to <model>")
        }
    };
    let mut when = serde_json::Map::new();
    // --always means match-all: no model_in. --from sets model_in.
    if args.always.is_none() {
        if let Some(from) = &args.from {
            when.insert("model_in".into(), json!([from]));
        }
    }
    if args.when_has_images {
        when.insert("has_images".into(), json!(true));
    }
    if args.when_has_audio {
        when.insert("has_audio".into(), json!(true));
    }
    if let Some(tag) = &args.when_tag {
        when.insert("tag_equals".into(), json!(tag));
    }
    if !args.when_prompt_contains.is_empty() {
        when.insert(
            "prompt_contains_any_of".into(),
            json!(args.when_prompt_contains),
        );
    }
    if let Some(v) = args.when_cost_gt {
        when.insert("estimated_cost_gt".into(), json!(v));
    }
    if let Some(v) = args.when_cost_lt {
        when.insert("estimated_cost_lt".into(), json!(v));
    }
    let mut then = serde_json::Map::new();
    then.insert("target_model".into(), json!(target));
    if !args.fallback.is_empty() {
        then.insert("fallbacks".into(), json!(args.fallback));
    }
    if args.disable_cache {
        then.insert("disable_cache".into(), json!(true));
    }
    if let Some(v) = args.max_cost {
        then.insert("max_cost_usd".into(), json!(v));
    }
    Ok(json!({
        "name": args.name.clone().unwrap_or_else(|| default_name(args, &target)),
        "priority": args.priority,
        "enabled": !args.disabled,
        "when": Value::Object(when),
        "then": Value::Object(then),
    }))
}

fn default_name(args: &AddArgs, target: &str) -> String {
    match &args.from {
        Some(f) => format!("{f}->{target}"),
        None => format!("all->{target}"),
    }
}

/// What `tt route` was asked to do.
pub enum RouteCmd {
    List,
    Show(String),
    Rm(String),
    Add(AddArgs),
}

/// Dispatch a `tt route` subcommand against the gateway.
pub async fn run(
    cmd: RouteCmd,
    flag_key: Option<String>,
    flag_base: Option<String>,
) -> anyhow::Result<()> {
    let ctx = ResolvedContext::load(flag_key, flag_base)?;
    let key = ctx
        .api_key_string()
        .context("no API key — run `tt login --token <KEY>` or set TT_API_KEY")?;
    let base = ctx.base_url.trim_end_matches('/').to_string();
    let http = reqwest::Client::new();

    match cmd {
        RouteCmd::List => {
            let sp = ui::spinner("Loading routes…");
            let routes: Value =
                send(http.get(format!("{base}/v1/routes")).bearer_auth(&key)).await?;
            drop(sp);
            print_routes(&routes);
        }
        RouteCmd::Show(id) => {
            let sp = ui::spinner("Loading route…");
            let route: Value =
                send(http.get(format!("{base}/v1/routes/{id}")).bearer_auth(&key)).await?;
            drop(sp);
            ui::heading(route["name"].as_str().unwrap_or(&id));
            println!("{}", serde_json::to_string_pretty(&route)?);
        }
        RouteCmd::Rm(id) => {
            let sp = ui::spinner("Removing route…");
            let _: Value = send(
                http.delete(format!("{base}/v1/routes/{id}"))
                    .bearer_auth(&key),
            )
            .await?;
            drop(sp);
            ui::success(&format!("Removed route {id}."));
        }
        RouteCmd::Add(args) => {
            let body = build_new_route(&args)?;
            let sp = ui::spinner("Creating route…");
            let route: Value = send(
                http.post(format!("{base}/v1/routes"))
                    .bearer_auth(&key)
                    .json(&body),
            )
            .await?;
            drop(sp);
            ui::success(&format!(
                "Created route {} ({}).",
                route["id"].as_str().unwrap_or("?"),
                route["name"].as_str().unwrap_or("?")
            ));
        }
    }
    Ok(())
}

/// Send a request; map non-2xx to an error carrying the response body.
async fn send(req: reqwest::RequestBuilder) -> anyhow::Result<Value> {
    let resp = req.send().await.context("request to gateway failed")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("gateway returned {status}: {text}");
    }
    if text.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).context("decode gateway response")
}

/// Pure: render the routes list as a styled table string.
fn routes_table(routes: &Value) -> String {
    let Some(arr) = routes.as_array() else {
        return ui::muted().apply_to("(unexpected response)").to_string();
    };
    if arr.is_empty() {
        return ui::muted()
            .apply_to("No routes. Create one with `tt route add --from <model> --to <model>`.")
            .to_string();
    }
    let mut t = ui::table(
        &["NAME", "ROUTE", "PRIO", "STATUS"],
        console::colors_enabled(),
    );
    for r in arr {
        let name = r["name"].as_str().unwrap_or("?");
        let from = r["when"]["model_in"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|v| v.as_str())
            .unwrap_or("*");
        let target = r["then"]["target_model"].as_str().unwrap_or("?");
        let route_cell = format!(
            "{} {} {}",
            ui::accent().apply_to(from),
            ui::ARROW,
            ui::accent().apply_to(target)
        );
        let status = if r["enabled"].as_bool().unwrap_or(false) {
            format!("{} on", ui::success_style().apply_to(ui::OK))
        } else {
            format!("{} off", ui::muted().apply_to(ui::NO))
        };
        t.add_row(vec![
            ui::heading_style().apply_to(name).to_string(),
            route_cell,
            r["priority"].as_u64().unwrap_or(0).to_string(),
            status,
        ]);
    }
    format!(
        "{}\n{}",
        ui::format_heading(&format!("ROUTES {} {}", ui::BULLET, arr.len())),
        t
    )
}

fn print_routes(routes: &Value) {
    println!("{}", routes_table(routes));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn always_pins_all_traffic() {
        let body = build_new_route(&AddArgs {
            always: Some("gpt-4o-mini".into()),
            from: None,
            to: None,
            when_has_images: false,
            when_has_audio: false,
            when_tag: None,
            when_prompt_contains: vec![],
            when_cost_gt: None,
            when_cost_lt: None,
            max_cost: None,
            disable_cache: false,
            priority: 100,
            name: None,
            fallback: vec![],
            disabled: false,
        })
        .unwrap();
        assert_eq!(body["then"]["target_model"], "gpt-4o-mini");
        assert_eq!(body["when"], json!({}));
        assert_eq!(body["priority"], 100);
        assert_eq!(body["enabled"], true);
    }

    #[test]
    fn from_to_with_modality() {
        let body = build_new_route(&AddArgs {
            always: None,
            from: Some("gpt-4o".into()),
            to: Some("gpt-4o-mini".into()),
            when_has_images: true,
            when_has_audio: false,
            when_tag: None,
            when_prompt_contains: vec![],
            when_cost_gt: None,
            when_cost_lt: None,
            max_cost: None,
            disable_cache: false,
            priority: 50,
            name: Some("vis".into()),
            fallback: vec!["gpt-4o".into()],
            disabled: true,
        })
        .unwrap();
        assert_eq!(body["when"]["model_in"], json!(["gpt-4o"]));
        assert_eq!(body["when"]["has_images"], true);
        assert_eq!(body["then"]["target_model"], "gpt-4o-mini");
        assert_eq!(body["then"]["fallbacks"], json!(["gpt-4o"]));
        assert_eq!(body["name"], "vis");
        assert_eq!(body["enabled"], false);
    }

    #[test]
    fn requires_a_target() {
        let err = build_new_route(&AddArgs {
            always: None,
            from: Some("gpt-4o".into()),
            to: None,
            when_has_images: false,
            when_has_audio: false,
            when_tag: None,
            when_prompt_contains: vec![],
            when_cost_gt: None,
            when_cost_lt: None,
            max_cost: None,
            disable_cache: false,
            priority: 100,
            name: None,
            fallback: vec![],
            disabled: false,
        });
        assert!(err.is_err());
    }

    #[test]
    fn disable_cache_and_when_tag_map_through() {
        let body = build_new_route(&AddArgs {
            always: Some("gpt-4o".into()),
            from: None,
            to: None,
            when_has_images: false,
            when_has_audio: false,
            when_tag: Some("sensitive".into()),
            when_prompt_contains: vec![],
            when_cost_gt: None,
            when_cost_lt: None,
            max_cost: None,
            disable_cache: true,
            priority: 100,
            name: None,
            fallback: vec![],
            disabled: false,
        })
        .unwrap();
        assert_eq!(body["when"]["tag_equals"], "sensitive");
        assert_eq!(body["then"]["disable_cache"], true);
    }

    #[test]
    fn disable_cache_omitted_when_false() {
        let body = build_new_route(&AddArgs {
            always: Some("gpt-4o".into()),
            from: None,
            to: None,
            when_has_images: false,
            when_has_audio: false,
            when_tag: None,
            when_prompt_contains: vec![],
            when_cost_gt: None,
            when_cost_lt: None,
            max_cost: None,
            disable_cache: false,
            priority: 100,
            name: None,
            fallback: vec![],
            disabled: false,
        })
        .unwrap();
        assert!(body["then"].get("disable_cache").is_none());
        assert!(body["when"].get("tag_equals").is_none());
    }

    #[test]
    fn when_prompt_contains_maps_to_condition() {
        let body = build_new_route(&AddArgs {
            always: Some("ollama/llama3".into()),
            from: None,
            to: None,
            when_has_images: false,
            when_has_audio: false,
            when_tag: None,
            when_prompt_contains: vec!["confidential".into(), "salary".into()],
            when_cost_gt: None,
            when_cost_lt: None,
            max_cost: None,
            disable_cache: false,
            priority: 100,
            name: None,
            fallback: vec![],
            disabled: false,
        })
        .unwrap();
        assert_eq!(
            body["when"]["prompt_contains_any_of"],
            json!(["confidential", "salary"])
        );
    }

    #[test]
    fn when_prompt_contains_omitted_when_empty() {
        let body = build_new_route(&AddArgs {
            always: Some("gpt-4o".into()),
            from: None,
            to: None,
            when_has_images: false,
            when_has_audio: false,
            when_tag: None,
            when_prompt_contains: vec![],
            when_cost_gt: None,
            when_cost_lt: None,
            max_cost: None,
            disable_cache: false,
            priority: 100,
            name: None,
            fallback: vec![],
            disabled: false,
        })
        .unwrap();
        assert!(body["when"].get("prompt_contains_any_of").is_none());
    }

    #[test]
    fn cost_conditions_map_through() {
        let body = build_new_route(&AddArgs {
            always: Some("gpt-4o-mini".into()),
            from: None,
            to: None,
            when_has_images: false,
            when_has_audio: false,
            when_tag: None,
            when_prompt_contains: vec![],
            when_cost_gt: Some(0.05),
            when_cost_lt: None,
            max_cost: None,
            disable_cache: false,
            priority: 100,
            name: None,
            fallback: vec![],
            disabled: false,
        })
        .unwrap();
        assert_eq!(body["when"]["estimated_cost_gt"], 0.05);
        assert!(body["when"].get("estimated_cost_lt").is_none());
    }

    #[test]
    fn max_cost_maps_to_then() {
        let body = build_new_route(&AddArgs {
            always: Some("gpt-4o-mini".into()),
            from: None,
            to: None,
            when_has_images: false,
            when_has_audio: false,
            when_tag: None,
            when_prompt_contains: vec![],
            when_cost_gt: None,
            when_cost_lt: None,
            max_cost: Some(0.1),
            disable_cache: false,
            priority: 100,
            name: None,
            fallback: vec![],
            disabled: false,
        })
        .unwrap();
        assert_eq!(body["then"]["max_cost_usd"], 0.1);
    }

    #[test]
    fn routes_table_renders_names_targets_and_status() {
        console::set_colors_enabled(false);
        let routes = json!([
            { "id": "a", "name": "vis", "priority": 100, "enabled": true,
              "when": {"model_in":["gpt-4o"]}, "then": {"target_model":"gpt-4o-mini"} },
            { "id": "b", "name": "capped", "priority": 50, "enabled": false,
              "when": {}, "then": {"target_model":"claude-haiku"} },
        ]);
        let out = routes_table(&routes);
        assert!(out.contains("vis"));
        assert!(out.contains("gpt-4o-mini"));
        assert!(out.contains("on"));
        assert!(out.contains("off"));
        assert!(out.contains('→')); // from → target
    }

    #[test]
    fn routes_table_empty_state() {
        console::set_colors_enabled(false);
        let out = routes_table(&json!([]));
        assert!(out.contains("No routes"));
    }
}
