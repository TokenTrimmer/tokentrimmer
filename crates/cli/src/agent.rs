//! `tt agent run` — drive the gateway's server-side agent loop
//! (`POST /v1/agent/runs`) from the CLI. The gateway owns the
//! model->tool->model loop (mid-loop down-routing, judge-gated summarize,
//! substep cache); this command kicks off a run, prints the turns + final
//! answer, and reports the aggregate cost.
//!
//! With `--tools`, the four read-only gateway tools (`find_route_for`,
//! `preview_cost`, `inspect_diff`, `batch_savings`) are advertised so the loop
//! can call them — they execute SERVER-SIDE, so the CLI never has to run a tool
//! itself. The client driver still requires a [`tt_client::ToolExecutor`]; the
//! [`DeclineExecutor`] below declines any (unexpected) CLIENT tool pause rather
//! than hanging, since this foundation command ships no real client tools yet.

use anyhow::Context as _;
use serde_json::json;

use crate::context::ResolvedContext;
use crate::ui;

/// The gateway's read-only agent tools (executed server-side). Mirrors
/// `crates/core/src/routes/gateway_tools.rs::GATEWAY_TOOLS`. Shared with the
/// `--server-loop` paths of `tt chat`/`tt advise`, which advertise the same four
/// tools so the gateway runs them server-side (no client `ToolExecutor` work).
pub(crate) fn gateway_tool_defs() -> Vec<tt_client::Tool> {
    vec![
        tt_client::tool(
            "find_route_for",
            "Suggest a cost-effective model/route for a described task.",
            json!({
                "type": "object",
                "properties": { "task": { "type": "string", "description": "What the request does." } },
                "required": ["task"]
            }),
        ),
        tt_client::tool(
            "preview_cost",
            "Estimate the USD cost of a model call before making it.",
            json!({
                "type": "object",
                "properties": {
                    "model": { "type": "string" },
                    "input_tokens": { "type": "integer" },
                    "output_tokens": { "type": "integer" }
                },
                "required": ["model"]
            }),
        ),
        tt_client::tool(
            "inspect_diff",
            "Scan a code diff for cost/quality findings.",
            json!({
                "type": "object",
                "properties": { "diff": { "type": "string" } }
            }),
        ),
        tt_client::tool(
            "batch_savings",
            "Estimate Batch-API savings for latency-insensitive traffic.",
            json!({
                "type": "object",
                "properties": { "tag": { "type": "string" } }
            }),
        ),
    ]
}

/// Declines any CLIENT (non-gateway) tool call. The gateway runs its own tools
/// server-side, so with only `--tools` advertised this is never invoked; it
/// exists to satisfy the driver's `ToolExecutor` and to fail gracefully (rather
/// than hang) if a model somehow requests a tool the CLI can't run. Shared with
/// the `--server-loop` paths of `tt chat`/`tt advise`.
pub(crate) struct DeclineExecutor;

#[tt_client::async_trait]
impl tt_client::ToolExecutor for DeclineExecutor {
    async fn call(
        &self,
        name: &str,
        _arguments: &str,
    ) -> std::result::Result<String, Box<dyn std::error::Error + Send + Sync>> {
        Ok(json!({
            "error": format!("tool '{name}' is not available in `tt agent run`")
        })
        .to_string())
    }
}

/// `tt agent runs` entry point: fetch and print the caller's recent agent runs as
/// a simple table (id · status · model · turns · cost).
pub async fn list_runs(flag_key: Option<String>, flag_base: Option<String>) -> anyhow::Result<()> {
    let ctx = ResolvedContext::load(flag_key, flag_base)?;
    let key = ctx
        .api_key_string()
        .context("no API key — run `tt login` or set TT_API_KEY")?;
    let base = ctx.base_url.trim_end_matches('/').to_string();
    let client = tt_client::Client::new(base, key);

    let _spin = ui::spinner("fetching runs…");
    let runs = client
        .list_runs()
        .await
        .map_err(|e| anyhow::anyhow!("list runs failed: {e}"))?;
    drop(_spin);

    if runs.is_empty() {
        ui::note("no agent runs found");
        return Ok(());
    }

    let color = console::colors_enabled();
    let mut t = ui::table(&["ID", "STATUS", "MODEL", "TURNS", "COST (USD)"], color);
    for r in &runs {
        t.add_row(vec![
            r.id.clone(),
            r.status.clone(),
            r.model.clone(),
            r.turns.to_string(),
            format!("${:.6}", r.cost_usd),
        ]);
    }
    println!(
        "{}\n{}",
        ui::format_heading(&format!("AGENT RUNS {} {}", ui::BULLET, runs.len())),
        t
    );
    Ok(())
}

/// Options for [`run`], mirroring the `Command::Agent { Run { .. } }` clap args.
pub struct RunOpts {
    pub prompt: String,
    pub model: Option<String>,
    pub system: Option<String>,
    pub tools: bool,
    pub max_turns: Option<u32>,
    pub max_cost: Option<f64>,
    pub tag: Option<String>,
    pub flag_key: Option<String>,
    pub flag_base: Option<String>,
}

const DEFAULT_MODEL: &str = "gpt-4o-mini";

/// `tt agent run` entry point: build the run, drive it to a terminal answer,
/// and print the transcript turns + aggregate cost.
pub async fn run(opts: RunOpts) -> anyhow::Result<()> {
    let ctx = ResolvedContext::load(opts.flag_key, opts.flag_base)?;
    let key = ctx
        .api_key_string()
        .context("no API key — run `tt login` or set TT_API_KEY")?;
    let base = ctx.base_url.trim_end_matches('/').to_string();
    let client = tt_client::Client::new(base, key);

    let model = opts.model.unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let mut builder = client.agent().model(&model).interactive();
    if let Some(sys) = opts.system {
        builder = builder.message(tt_client::system(sys));
    }
    builder = builder.message(tt_client::user(opts.prompt));
    if opts.tools {
        builder = builder.tools(gateway_tool_defs());
    }
    if let Some(mt) = opts.max_turns {
        builder = builder.max_turns(mt);
    }
    if let Some(c) = opts.max_cost {
        builder = builder.max_cost_usd(c);
    }
    if let Some(tag) = opts.tag {
        builder = builder.tag(tag);
    }

    ui::heading("TokenTrimmer agent run");
    let outcome = builder
        .run(&DeclineExecutor)
        .await
        .map_err(|e| anyhow::anyhow!("agent run failed: {e}"))?;

    print_outcome(&outcome);
    Ok(())
}

/// Print the run's turns, final answer, and aggregate cost. Transcript/cost
/// detail goes to stderr (status); the final answer to stdout (the result).
/// Shared with `tt advise --server-loop`.
pub(crate) fn print_outcome(outcome: &tt_client::AgentOutcome) {
    let run = &outcome.run;
    ui::note(&format!(
        "status: {:?} · server turns: {} · client resumes: {}",
        run.status, run.turns, outcome.resume_rounds
    ));
    if let Some(note) = &run.note {
        ui::note(&format!("note: {note}"));
    }
    match outcome.text() {
        Some(answer) => println!("{answer}"),
        None => ui::warn("the run produced no final text answer"),
    }
    let u = &run.usage;
    ui::ok(&format!(
        "cost: ${:.6} · prompt {} + completion {} tokens",
        u.cost_usd, u.prompt_tokens, u.completion_tokens
    ));
    if let Some(tax) = run.summarizer_tax_usd {
        if tax > 0.0 {
            ui::note(&format!("summarizer measurement tax: ${tax:.6}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_client::ToolExecutor as _;

    #[test]
    fn gateway_tool_defs_advertise_the_four_server_side_tools() {
        let defs = gateway_tool_defs();
        let names: Vec<&str> = defs.iter().map(|t| t.function.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "find_route_for",
                "preview_cost",
                "inspect_diff",
                "batch_savings"
            ]
        );
    }

    #[tokio::test]
    async fn decline_executor_declines_gracefully() {
        let out = DeclineExecutor.call("write_file", "{}").await.unwrap();
        assert!(out.contains("not available"), "{out}");
    }
}
