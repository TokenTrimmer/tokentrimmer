//! `tt route` — manage routing rules via the gateway's user-facing /v1/routes
//! API, authenticated with the V0-resolved key.

use anyhow::Context as _;
use serde_json::{json, Value};

use crate::context::ResolvedContext;

/// Flags for `tt route add`. Mirrors the clap args in `main.rs`.
pub struct AddArgs {
    pub always: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub when_has_images: bool,
    pub when_has_audio: bool,
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
    let mut then = serde_json::Map::new();
    then.insert("target_model".into(), json!(target));
    if !args.fallback.is_empty() {
        then.insert("fallbacks".into(), json!(args.fallback));
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
            let routes: Value =
                send(http.get(format!("{base}/v1/routes")).bearer_auth(&key)).await?;
            print_routes(&routes);
        }
        RouteCmd::Show(id) => {
            let route: Value =
                send(http.get(format!("{base}/v1/routes/{id}")).bearer_auth(&key)).await?;
            println!("{}", serde_json::to_string_pretty(&route)?);
        }
        RouteCmd::Rm(id) => {
            let _: Value = send(
                http.delete(format!("{base}/v1/routes/{id}"))
                    .bearer_auth(&key),
            )
            .await?;
            println!("Removed route {id}.");
        }
        RouteCmd::Add(args) => {
            let body = build_new_route(&args)?;
            let route: Value = send(
                http.post(format!("{base}/v1/routes"))
                    .bearer_auth(&key)
                    .json(&body),
            )
            .await?;
            println!(
                "Created route {} ({}).",
                route["id"].as_str().unwrap_or("?"),
                route["name"].as_str().unwrap_or("?")
            );
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

fn print_routes(routes: &Value) {
    let Some(arr) = routes.as_array() else {
        println!("(no routes)");
        return;
    };
    if arr.is_empty() {
        println!("No routes. Create one with `tt route add --from <model> --to <model>`.");
        return;
    }
    println!(
        "{:<38}  {:<22}  {:>4}  {:<8}  TARGET",
        "ID", "NAME", "PRIO", "ENABLED"
    );
    for r in arr {
        println!(
            "{:<38}  {:<22}  {:>4}  {:<8}  {}",
            r["id"].as_str().unwrap_or("?"),
            r["name"].as_str().unwrap_or("?"),
            r["priority"].as_u64().unwrap_or(0),
            r["enabled"].as_bool().unwrap_or(false),
            r["then"]["target_model"].as_str().unwrap_or("?"),
        );
    }
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
            priority: 100,
            name: None,
            fallback: vec![],
            disabled: false,
        });
        assert!(err.is_err());
    }
}
