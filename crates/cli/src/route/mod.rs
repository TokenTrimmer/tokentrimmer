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
    /// `--when-p95-gt <ms>`: match only when the gateway's live observed p95
    /// upstream latency for the requested model exceeds this many milliseconds.
    pub when_p95_gt: Option<u32>,
    pub max_cost: Option<f64>,
    pub disable_cache: bool,
    /// `--batch`: advisory batch-eligibility marker (Batch Lane forgone-savings
    /// attribution; never applied to streaming/interactive requests).
    pub batch: bool,
    /// `--agentic-budget`: opt the matched (loop) traffic into the route-grained
    /// agentic context budget. Master switch — the other agentic levers below
    /// are inert unless this is set. Off by default; when off, `then` carries no
    /// `agentic_budget` key at all (byte-identical back-compat wire).
    pub agentic_budget: bool,
    /// `--keep-recent <N>`: keep the last N tool-result pairs VERBATIM (caveat
    /// C1 blast-radius bound). Mirrors `AgenticBudget::keep_recent_pairs`
    /// (default 3; server-validated to be >= 1).
    pub keep_recent: u32,
    /// `--elide-stale-tools`: field-drop (lossless, token-true-gated) + summarize
    /// (lossy, judge-gated) stale tool results. Mirrors
    /// `AgenticBudget::elide_stale_tools`.
    pub elide_stale_tools: bool,
    /// `--route-mechanical-to <model>`: down-route mechanical sub-steps to this
    /// model in a cache-isolated subagent lane. Mirrors
    /// `AgenticBudget::route_mechanical_to`.
    pub route_mechanical_to: Option<String>,
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
    if let Some(v) = args.when_p95_gt {
        when.insert("upstream_latency_ms_p95_gt".into(), json!(v));
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
    if args.batch {
        then.insert("batch".into(), json!(true));
    }
    // `--agentic-budget` is the master switch: when off, `then` carries NO
    // `agentic_budget` key at all, so the wire body is byte-identical to the
    // pre-feature shape (off-by-default back-compat). When on, emit the
    // `AgenticBudget` object the gateway's serde mirrors. `cache_prefix` (the
    // lossless FIRST lever) defaults on with the mode; the other levers reflect
    // their flags.
    if args.agentic_budget {
        then.insert(
            "agentic_budget".into(),
            json!({
                "cache_prefix": true,
                "elide_stale_tools": args.elide_stale_tools,
                "keep_recent_pairs": args.keep_recent,
                "route_mechanical_to": args.route_mechanical_to,
            }),
        );
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
    // `AddArgs` is much larger than the other variants (many flags) — box it so
    // the enum doesn't carry that size on every variant (clippy large_enum_variant).
    Add(Box<AddArgs>),
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
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")?;

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
            let route: Value = send(
                http.get(format!("{base}/v1/routes/{}", enc_segment(&id)))
                    .bearer_auth(&key),
            )
            .await?;
            drop(sp);
            ui::heading(route["name"].as_str().unwrap_or(&id));
            println!("{}", serde_json::to_string_pretty(&route)?);
        }
        RouteCmd::Rm(id) => {
            let sp = ui::spinner("Removing route…");
            let _: Value = send(
                http.delete(format!("{base}/v1/routes/{}", enc_segment(&id)))
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

/// Percent-encode a user-supplied path segment (route id) so `/ ? # %`, spaces,
/// etc. can't break or traverse the URL path. Over-encodes harmless chars (e.g.
/// a UUID's `-`); the gateway percent-decodes the path param.
fn enc_segment(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
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

    /// A minimal valid `AddArgs` (a match-all `--always` target, every other
    /// flag off/default). Tests override only the field under test, so adding a
    /// new flag never forces every literal to be rewritten.
    fn base_args() -> AddArgs {
        AddArgs {
            always: Some("gpt-4o-mini".into()),
            from: None,
            to: None,
            when_has_images: false,
            when_has_audio: false,
            when_tag: None,
            when_prompt_contains: vec![],
            when_cost_gt: None,
            when_cost_lt: None,
            when_p95_gt: None,
            max_cost: None,
            disable_cache: false,
            batch: false,
            agentic_budget: false,
            keep_recent: 3,
            elide_stale_tools: false,
            route_mechanical_to: None,
            priority: 100,
            name: None,
            fallback: vec![],
            disabled: false,
        }
    }

    /// `--agentic-budget --keep-recent 3 --elide-stale-tools --route-mechanical-to haiku-4.5`
    /// must produce a `then.agentic_budget` object that deserializes into the
    /// real [`tt_routing::RouteAction`] (the parity contract: the CLI surface is
    /// a true mirror of the server-side knob bag, validated through serde).
    #[test]
    fn agentic_budget_flag_maps_to_route_action() {
        let body = build_new_route(&AddArgs {
            agentic_budget: true,
            keep_recent: 3,
            elide_stale_tools: true,
            route_mechanical_to: Some("haiku-4.5".into()),
            ..base_args()
        })
        .unwrap();
        let then: tt_routing::RouteAction =
            serde_json::from_value(body["then"].clone()).expect("then must parse as RouteAction");
        let ab = then.agentic_budget.expect("agentic_budget must be Some");
        assert!(ab.elide_stale_tools, "--elide-stale-tools sets the flag");
        assert_eq!(
            ab.keep_recent_pairs, 3,
            "--keep-recent maps to keep_recent_pairs"
        );
        assert_eq!(
            ab.route_mechanical_to.as_deref(),
            Some("haiku-4.5"),
            "--route-mechanical-to maps through"
        );
        // The opt-in turns the lossless FIRST lever on by default (the struct
        // documents `cache_prefix` as "default true when the mode is set").
        assert!(
            ab.cache_prefix,
            "opting in enables the lossless cache-prefix lever"
        );
    }

    /// Without `--agentic-budget`, `then.agentic_budget` is absent entirely (so
    /// the deserialized `RouteAction` carries `None`) — the off-by-default,
    /// byte-identical-wire back-compat invariant the whole feature rests on.
    #[test]
    fn agentic_budget_absent_is_none() {
        let body = build_new_route(&base_args()).unwrap();
        assert!(
            body["then"].get("agentic_budget").is_none(),
            "without --agentic-budget the key must be absent from the wire body"
        );
        let then: tt_routing::RouteAction =
            serde_json::from_value(body["then"].clone()).expect("then must parse as RouteAction");
        assert_eq!(then.agentic_budget, None);
    }

    /// `--keep-recent` overrides the default and rides into `keep_recent_pairs`;
    /// the other levers stay at their opt-in defaults.
    #[test]
    fn agentic_budget_keep_recent_overrides_default() {
        let body = build_new_route(&AddArgs {
            agentic_budget: true,
            keep_recent: 5,
            ..base_args()
        })
        .unwrap();
        let then: tt_routing::RouteAction = serde_json::from_value(body["then"].clone()).unwrap();
        let ab = then.agentic_budget.unwrap();
        assert_eq!(ab.keep_recent_pairs, 5);
        assert!(!ab.elide_stale_tools);
        assert_eq!(ab.route_mechanical_to, None);
    }

    /// `--route-mechanical-to` (and the other levers) are inert without the
    /// `--agentic-budget` opt-in: the mode gate is the master switch.
    #[test]
    fn agentic_budget_levers_inert_without_optin() {
        let body = build_new_route(&AddArgs {
            agentic_budget: false,
            elide_stale_tools: true,
            route_mechanical_to: Some("haiku-4.5".into()),
            ..base_args()
        })
        .unwrap();
        assert!(
            body["then"].get("agentic_budget").is_none(),
            "lever flags do nothing unless --agentic-budget is passed"
        );
    }

    #[test]
    fn always_pins_all_traffic() {
        let body = build_new_route(&base_args()).unwrap();
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
            priority: 50,
            name: Some("vis".into()),
            fallback: vec!["gpt-4o".into()],
            disabled: true,
            ..base_args()
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
            ..base_args()
        });
        assert!(err.is_err());
    }

    #[test]
    fn disable_cache_and_when_tag_map_through() {
        let body = build_new_route(&AddArgs {
            always: Some("gpt-4o".into()),
            when_tag: Some("sensitive".into()),
            disable_cache: true,
            ..base_args()
        })
        .unwrap();
        assert_eq!(body["when"]["tag_equals"], "sensitive");
        assert_eq!(body["then"]["disable_cache"], true);
    }

    /// `--batch` maps to `then.batch = true` (the advisory batch-eligibility
    /// route action); without the flag the key is absent entirely — the wire
    /// shape stays back-compat (`batch` is skip-serialized when false).
    #[test]
    fn route_add_batch_flag_maps_to_then_batch() {
        let args = |batch: bool| AddArgs {
            batch,
            ..base_args()
        };
        let body = build_new_route(&args(true)).unwrap();
        assert_eq!(body["then"]["batch"], true, "--batch must set then.batch");
        let body = build_new_route(&args(false)).unwrap();
        assert!(
            body["then"].get("batch").is_none(),
            "without --batch the key must be absent"
        );
    }

    #[test]
    fn disable_cache_omitted_when_false() {
        let body = build_new_route(&AddArgs {
            always: Some("gpt-4o".into()),
            ..base_args()
        })
        .unwrap();
        assert!(body["then"].get("disable_cache").is_none());
        assert!(body["when"].get("tag_equals").is_none());
    }

    #[test]
    fn when_prompt_contains_maps_to_condition() {
        let body = build_new_route(&AddArgs {
            always: Some("ollama/llama3".into()),
            when_prompt_contains: vec!["confidential".into(), "salary".into()],
            ..base_args()
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
            ..base_args()
        })
        .unwrap();
        assert!(body["when"].get("prompt_contains_any_of").is_none());
    }

    #[test]
    fn cost_conditions_map_through() {
        let body = build_new_route(&AddArgs {
            when_cost_gt: Some(0.05),
            ..base_args()
        })
        .unwrap();
        assert_eq!(body["when"]["estimated_cost_gt"], 0.05);
        assert!(body["when"].get("estimated_cost_lt").is_none());
    }

    #[test]
    fn when_p95_gt_serializes_into_when_clause() {
        let body = build_new_route(&AddArgs {
            when_p95_gt: Some(1500),
            ..base_args()
        })
        .unwrap();
        assert_eq!(body["when"]["upstream_latency_ms_p95_gt"], 1500);
    }

    #[test]
    fn when_p95_gt_omitted_when_absent() {
        let body = build_new_route(&base_args()).unwrap();
        assert!(body["when"].get("upstream_latency_ms_p95_gt").is_none());
    }

    #[test]
    fn max_cost_maps_to_then() {
        let body = build_new_route(&AddArgs {
            max_cost: Some(0.1),
            ..base_args()
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

    #[test]
    fn enc_segment_encodes_path_breakers() {
        let e = super::enc_segment("a/b?c#d e");
        assert!(!e.contains('/') && !e.contains('?') && !e.contains('#') && !e.contains(' '));
        assert!(e.contains("%2F") && e.contains("%3F") && e.contains("%23") && e.contains("%20"));
    }

    #[test]
    fn enc_segment_uuid_round_trips() {
        let id = "550e8400-e29b-41d4-a716-446655440000";
        let e = super::enc_segment(id);
        assert!(!e.contains('/'));
        let decoded = percent_encoding::percent_decode_str(&e)
            .decode_utf8()
            .unwrap();
        assert_eq!(decoded, id);
    }
}
