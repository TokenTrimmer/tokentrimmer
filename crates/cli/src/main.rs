//! `tt` — TokenTrimmer CLI.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};

mod audit;
use audit::AuditAction;
mod repo_context;

#[derive(Parser)]
#[command(name = "tt")]
#[command(about = "TokenTrimmer CLI — gateway, inspect, plan, audit", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Disable colored output.
    #[arg(long, global = true)]
    no_color: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run the Gateway proxy server.
    Gateway {
        /// Apply DB migrations and exit (no server). Explicit, gated migration
        /// step for the deploy pipeline; exits non-zero on failure.
        #[arg(long)]
        migrate_only: bool,
    },
    /// Scan a codebase for token-waste patterns.
    Inspect {
        /// Path to scan (rule mode) or to scope the diff to (`--cost-diff` mode).
        path: String,
        /// Fail the process if any finding meets or exceeds this severity.
        #[arg(long, default_value = "high")]
        fail_on: String,
        /// Output destination. Omitted or "-" writes to stdout.
        /// A path ending in ".json" writes JSON, ".sarif" writes SARIF; any
        /// other path writes markdown. An explicit `--format` overrides this.
        #[arg(long)]
        output: Option<String>,
        /// Output format for rule findings: `md` (default), `json`, or `sarif`
        /// (SARIF 2.1.0, for the GitHub Code Scanning / Security tab + inline PR
        /// annotations). When set, overrides the format inferred from `--output`.
        /// Ignored in `--cost-diff` / `--suggest-plan` mode.
        #[arg(long)]
        format: Option<String>,
        /// Cost-diff mode: instead of running rules, estimate the projected
        /// per-call cost change of LLM model ids added/removed in `git diff
        /// <base> -- <path>`. Reuses the pricing catalog; no cloud dependency.
        #[arg(long)]
        cost_diff: bool,
        /// Base git ref to diff against in `--cost-diff` mode.
        #[arg(long, default_value = "HEAD")]
        base: String,
        /// In `--cost-diff` mode, exit non-zero when a net cost increase is projected.
        #[arg(long)]
        fail_on_cost_increase: bool,
        /// Suggest-plan mode: scan `path` for model strings, generate cheaper-model
        /// route suggestions via the preview engine, and emit a skeleton
        /// `PlanInput` JSON with `proposed_routes` pre-filled.
        ///
        /// The output can be fed straight into `tt plan --input <file>` after
        /// filling in `org_id`, `requests`, and the replay window.
        /// Pairs with `--output` to write to a file instead of stdout.
        #[arg(long, conflicts_with_all = ["cost_diff"])]
        suggest_plan: bool,
        /// With --suggest-plan: pull a real `request_logs` telemetry window
        /// from the gateway's Postgres (DATABASE_URL) into the emitted
        /// PlanInput's `requests`, making the file immediately runnable.
        #[arg(long, requires = "suggest_plan")]
        from_db: bool,
        /// With --from-db: the org UUID to pull. If omitted, auto-detected when
        /// the window has exactly one org (errors if ambiguous).
        #[arg(long, requires = "from_db")]
        org: Option<String>,
        /// With --from-db: telemetry window size in days.
        #[arg(long, default_value_t = 7)]
        window_days: i64,
    },
    /// Replay historical telemetry against a proposed config and project
    /// cost/savings/cache-hit-rate impact with bootstrap confidence intervals.
    ///
    /// v1 reads a serialized [`tt_plan_core::PlanInput`] from a JSON file —
    /// in production the request log + proposed config come from Postgres,
    /// but for offline analysis and CI gates the JSON file path is the
    /// universal interface.
    Plan {
        /// Path to a JSON file containing a serialized PlanInput.
        ///
        /// Use `--example` to dump a minimal example to stdout for editing.
        #[arg(long, conflicts_with = "example")]
        input: Option<String>,

        /// Output destination. Omitted or "-" writes a text summary to stdout.
        /// A path ending in ".json" writes the full PlanResult as JSON.
        #[arg(long)]
        output: Option<String>,

        /// Print an example PlanInput skeleton to stdout (no replay performed).
        #[arg(long)]
        example: bool,

        /// Apply the projected routes to the gateway's Postgres `routes` table
        /// (requires DATABASE_URL) and record a signed `plan.applied` entry to
        /// `.claude/AUDIT-CHAIN.jsonl`. Dry-runs + prompts for confirmation
        /// unless `--yes`. The gateway picks the routes up on its next refresh.
        #[arg(long, conflicts_with = "example")]
        apply: bool,

        /// With --apply: skip the interactive confirmation prompt (for CI /
        /// automation). Ignored without --apply.
        #[arg(long)]
        yes: bool,
    },
    /// Audit log helpers.
    Audit {
        #[command(subcommand)]
        action: AuditAction,
    },
    /// Run the MCP server (stdio transport by default).
    ///
    /// `--transport http` serves the current MCP Streamable HTTP transport on a
    /// single `/mcp` endpoint; `--transport sse` is the deprecated HTTP+SSE
    /// transport, retained only for older clients.
    Mcp {
        /// Transport: `stdio` (default), `http` (Streamable HTTP), or `sse` (deprecated).
        #[arg(long, default_value = "stdio")]
        transport: String,
        #[arg(long)]
        tt_api_key: Option<String>,
        #[arg(long)]
        tt_api_base: Option<String>,
        /// Port to bind when using --transport http or sse (default 31416).
        #[arg(long, default_value_t = 31416)]
        sse_port: u16,
        /// Enable the mutating write tools (`add_route`, `apply_plan`). OFF by
        /// default — without this flag the server is read-only and the write
        /// tools are absent from `tools/list`. Requires DATABASE_URL so the
        /// operator key can be verified and org-bound at boot; refuses to
        /// start otherwise (fail closed).
        #[arg(long)]
        allow_write: bool,
        /// Operator query-offload config (TOML registering dataset aliases
        /// for `run_query`/`list_datasets`). The config file IS the opt-in:
        /// without it the query tools are never registered; a supplied but
        /// unreadable/invalid config refuses to start (fail closed).
        #[arg(long, env = "TT_MCP_QUERY_CONFIG")]
        query_config: Option<PathBuf>,
    },
    /// Log in: opens the dashboard to create an API key (paste it back), or pass --token <KEY>.
    Login {
        /// The tt_live_/tt_test_ key. Use `-` to read it from stdin.
        #[arg(long)]
        token: Option<String>,
        /// Persist a gateway base URL alongside the key.
        #[arg(long)]
        base_url: Option<String>,
        /// Don't open a browser; just print the URL to visit (headless/SSH).
        #[arg(long)]
        no_browser: bool,
    },
    /// Remove the locally stored API key (does not revoke it server-side).
    Logout,
    /// Show the resolved API key (masked), its source, and the gateway base URL.
    Whoami,
    /// Interactive chat through the gateway (streams responses + shows savings).
    Chat {
        /// Model to request (the gateway may route it). Default: gpt-4o-mini.
        #[arg(long)]
        model: Option<String>,
        /// Optional system prompt for the conversation.
        #[arg(long)]
        system: Option<String>,
        /// Resume a saved session by name (see /sessions).
        #[arg(long)]
        resume: Option<String>,
        /// Enable tool-calling from the start (find_route_for, preview_cost, inspect_diff).
        #[arg(long)]
        tools: bool,
        /// Token budget for context management (default: the per-model window).
        #[arg(long)]
        max_context: Option<u32>,
        /// Disable lossless tool-result/arg trimming in the /tools loop.
        #[arg(long)]
        no_tool_trim: bool,
        /// Enable cache-aware compaction: fold old turns into a frozen summary
        /// every K turns. Off by default — the summary is a paid model call.
        #[arg(long)]
        compact: bool,
        /// Compact every K successful turns (min 2 — K=1 would bust the
        /// provider cache every turn; default 8).
        #[arg(long, default_value_t = 8)]
        compact_every: u32,
        /// Model for the compaction summary call (default: gpt-4o-mini).
        #[arg(long)]
        compact_model: Option<String>,
        #[arg(long, global = true)]
        tt_api_key: Option<String>,
        #[arg(long, global = true)]
        tt_api_base: Option<String>,
    },
    /// List the gateway's model catalog (context windows, capabilities, pricing).
    Models {
        #[arg(long)]
        tt_api_key: Option<String>,
        #[arg(long)]
        tt_api_base: Option<String>,
    },
    /// Embed text via the gateway and print a cost summary (or --json vectors).
    Embed {
        /// Text to embed. One arg → single; many → a batch. Omit to read stdin.
        input: Vec<String>,
        /// Embedding model (default: text-embedding-3-small).
        #[arg(long)]
        model: Option<String>,
        /// Reduce output dimensions (Matryoshka models).
        #[arg(long)]
        dimensions: Option<u32>,
        /// Wire encoding format (e.g. "float" or "base64").
        #[arg(long)]
        encoding_format: Option<String>,
        /// Reject (402) if the estimated cost exceeds this many USD.
        #[arg(long)]
        cost_limit: Option<f64>,
        /// Print the full EmbeddingsResponse JSON to stdout (summary → stderr).
        #[arg(long)]
        json: bool,
        #[arg(long)]
        tt_api_key: Option<String>,
        #[arg(long)]
        tt_api_base: Option<String>,
    },
    /// AI cost/routing advisor: scan a repo + recommend optimizations (read-only).
    Advise {
        /// Repo path to scan (default: current directory).
        path: Option<String>,
        /// Describe what the app does (adds context for the advisor).
        #[arg(long)]
        describe: Option<String>,
        /// Advisor model (default: gpt-4o-mini).
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        tt_api_key: Option<String>,
        #[arg(long)]
        tt_api_base: Option<String>,
    },
    /// Install TokenTrimmer best-practices into the current repo.
    Init {
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        framework: Option<String>,
        #[arg(long)]
        interactive: bool,
        #[arg(long)]
        upgrade: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        diff: bool,
        #[arg(long)]
        skip_baseline: bool,
        #[arg(long)]
        skip_hooks: bool,
        #[arg(long)]
        skip_workflows: bool,
        #[arg(long)]
        dry_run: bool,
        /// Tailor the generated config with an AI pass over the repo (needs an API key).
        #[arg(long)]
        ai: bool,
        /// Model for the --ai pass (default: gpt-4o-mini).
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        tt_api_key: Option<String>,
        #[arg(long)]
        tt_api_base: Option<String>,
    },
    /// RAG corpus management. EXPERIMENTAL: in-process only, not persisted.
    Retrieval {
        #[command(subcommand)]
        action: RetrievalAction,
    },
    /// Run a local OpenAI/Anthropic-compatible proxy on port 31415.
    Proxy {
        #[arg(long, default_value_t = 31415)]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value = "gateway")]
        mode: String,
        #[arg(long)]
        tt_api_key: Option<String>,
        #[arg(long)]
        tt_api_base: Option<String>,
        #[arg(long)]
        no_tui: bool,
        #[arg(long)]
        no_preview: bool,
        #[arg(long)]
        session_log: Option<String>,
    },
    /// Manage routing rules via the hosted gateway (requires `tt login`).
    Route {
        #[command(subcommand)]
        action: RouteAction,
        /// Override the API key (else V0 resolution: env / ~/.tokentrimmer).
        #[arg(long, global = true)]
        tt_api_key: Option<String>,
        /// Override the gateway base URL.
        #[arg(long, global = true)]
        tt_api_base: Option<String>,
    },
    /// Curated savings recipes — list, inspect, and apply ready-made route-sets.
    Recipes {
        #[command(subcommand)]
        action: RecipesAction,
        /// Override the API key (else V0 resolution: env / ~/.tokentrimmer).
        #[arg(long, global = true)]
        tt_api_key: Option<String>,
        /// Override the gateway base URL.
        #[arg(long, global = true)]
        tt_api_base: Option<String>,
    },
    /// Preload the most relevant repo files for a coding task.
    Context {
        /// Task description in plain English.
        #[arg(long)]
        task: String,
        /// Repo path to index (default: current dir).
        #[arg(default_value = ".")]
        path: String,
        /// Output format: json | md.
        #[arg(long, default_value = "md")]
        format: String,
        /// Max files to describe.
        #[arg(long, default_value_t = 12)]
        max_files: usize,
        /// Token cap for inlined file content.
        #[arg(long, default_value_t = 6000)]
        token_budget: u32,
    },
    /// Drive the gateway's server-side agent loop (POST /v1/agent/runs).
    Agent {
        #[command(subcommand)]
        action: AgentAction,
        /// Override the API key (else V0 resolution: env / ~/.tokentrimmer).
        #[arg(long, global = true)]
        tt_api_key: Option<String>,
        /// Override the gateway base URL.
        #[arg(long, global = true)]
        tt_api_base: Option<String>,
    },
    /// Async Batch Lane: submit a JSONL file, list/get/cancel batches, download results.
    Batch {
        #[command(subcommand)]
        action: tt_cli::batch::BatchAction,
        /// Override the API key (else V0 resolution: env / ~/.tokentrimmer).
        #[arg(long, global = true)]
        tt_api_key: Option<String>,
        /// Override the gateway base URL.
        #[arg(long, global = true)]
        tt_api_base: Option<String>,
    },
}

#[derive(Subcommand)]
enum AgentAction {
    /// Run a server-side agent loop against a prompt and print the result.
    Run {
        /// The user prompt to run the agent on.
        prompt: String,
        /// Model to request (the gateway may route it). Default: gpt-4o-mini.
        #[arg(long)]
        model: Option<String>,
        /// Optional system prompt.
        #[arg(long)]
        system: Option<String>,
        /// Advertise the read-only gateway tools (find_route_for, preview_cost,
        /// inspect_diff, batch_savings) so the loop can call them server-side.
        #[arg(long)]
        tools: bool,
        /// Server-side per-run turn cap (the gateway clamps to 1..=32).
        #[arg(long)]
        max_turns: Option<u32>,
        /// X-TokenTrimmer-Tag cost-attribution tag.
        #[arg(long)]
        tag: Option<String>,
    },
}

#[derive(Subcommand)]
enum RetrievalAction {
    /// EXPERIMENTAL: add a doc to a corpus. In-process only — the corpus lives
    /// in this process's memory and is discarded on exit; nothing is persisted.
    DocAdd {
        corpus: String,
        path: String,
        #[arg(long, env = "OPENAI_API_KEY")]
        openai_key: String,
    },
    /// EXPERIMENTAL: ad-hoc search over an in-process corpus. Each invocation
    /// starts empty, so a separate `doc-add` run is never visible here.
    Search {
        corpus: String,
        query: String,
        #[arg(long, default_value_t = 5)]
        k: usize,
        #[arg(long, env = "OPENAI_API_KEY")]
        openai_key: String,
    },
}

/// The action to take on the curated down-route catalog.
#[derive(clap::ValueEnum, Clone, Debug, Copy)]
enum CatalogAction {
    /// Create all curated down-routes (idempotent — skips already-existing routes by name).
    Enable,
    /// Remove all catalog-managed routes (never touches user-defined routes).
    Disable,
    /// Show which catalog routes are active or paused.
    Status,
}

// The `Add` variant carries every `route add` flag inline (clap can't box an
// inline struct variant's fields). This enum is parsed once at startup and never
// stored in a hot collection, so its size is immaterial — suppress the lint here
// rather than restructure the clap surface.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum RouteAction {
    /// List your routes.
    List,
    /// Show one route by id.
    Show { id: String },
    /// Delete one route by id.
    Rm { id: String },
    /// Manage the curated flagship → mini down-route catalog (opt-in, idempotent).
    Catalog {
        /// Action to perform: enable, disable, or status.
        action: CatalogAction,
    },
    /// Add a route. Use --always <model>, or --from <m> --to <m>, or
    /// --agentic-budget alone for a modifier-only route (keeps the caller's model).
    Add {
        #[arg(long)]
        always: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        when_has_images: bool,
        #[arg(long)]
        when_has_audio: bool,
        /// Match only requests tagged with this value (X-TokenTrimmer-Tag header).
        #[arg(long)]
        when_tag: Option<String>,
        /// Match only requests whose prompt contains this keyword (repeatable).
        #[arg(long)]
        when_prompt_contains: Vec<String>,
        /// Match only requests whose estimated cost (USD) exceeds this.
        #[arg(long)]
        when_cost_gt: Option<f64>,
        /// Match only requests whose estimated cost (USD) is below this.
        #[arg(long)]
        when_cost_lt: Option<f64>,
        /// Match only when the gateway's live observed p95 upstream latency for
        /// the requested model exceeds this many milliseconds. Backed by the
        /// gateway's own rolling window — does NOT fire until enough recent
        /// samples exist (cold start), so a fresh/unknown primary is never gated.
        #[arg(long)]
        when_p95_gt: Option<u32>,
        /// Reject (402) any matched request whose estimated cost exceeds this (USD).
        #[arg(long)]
        max_cost: Option<f64>,
        /// Make matched requests skip TokenTrimmer's cache entirely (privacy).
        #[arg(long)]
        disable_cache: bool,
        /// Advisory batch-eligibility marker (Batch Lane forgone-savings
        /// attribution; never applied to streaming/interactive requests).
        #[arg(long)]
        batch: bool,
        /// Opt matched (loop) traffic into the agentic context budget — the
        /// route-grained mode that brings the CLI's loop-aware levers
        /// server-side. Master switch: the --keep-recent / --elide-stale-tools
        /// / --route-mechanical-to flags below are inert unless this is set.
        /// Off by default (no-op for non-opted traffic).
        #[arg(long)]
        agentic_budget: bool,
        /// With --agentic-budget: keep the last N tool-result pairs VERBATIM
        /// (caveat C1 blast-radius bound). Default 3; must be >= 1.
        #[arg(long, default_value_t = 3)]
        keep_recent: u32,
        /// With --agentic-budget: field-drop (lossless) + summarize (lossy,
        /// judge-gated) stale tool results.
        #[arg(long)]
        elide_stale_tools: bool,
        /// With --agentic-budget: down-route mechanical sub-steps to this model
        /// in a cache-isolated subagent lane (must resolve to a registered model).
        #[arg(long)]
        route_mechanical_to: Option<String>,
        #[arg(long, default_value_t = 100)]
        priority: u32,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        fallback: Vec<String>,
        #[arg(long)]
        disabled: bool,
    },
}

#[derive(Subcommand)]
enum RecipesAction {
    /// List the curated savings recipes (name, what it optimizes, lane).
    List,
    /// Show one recipe's route-set (humanized) and its savings lane.
    Show {
        /// Recipe slug (see `tt recipes list`).
        name: String,
    },
    /// Apply a recipe — create its routes via the hosted gateway (requires a key).
    Apply {
        /// Recipe slug (see `tt recipes list`).
        name: String,
    },
}

/// Install ring as the process-default rustls `CryptoProvider`.
///
/// The dependency tree links BOTH providers (ring via reqwest 0.12 and
/// friends; aws-lc-rs via redis 1.x and opentelemetry-otlp 0.32's reqwest
/// 0.13), and rustls 0.23 PANICS at the first default-provider use when more
/// than one is linked and none was installed. That panic took the prod
/// gateway down at boot (OTLP exporter init) on the first deploy after the
/// #158 dependency bumps — staging missed it because only prod sets the OTLP
/// env. Must run before ANY TLS-touching init (sentry, tracing/OTLP, sqlx,
/// provider clients). Idempotent: a second call returns Err (a default
/// already exists), which is fine.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod crypto_provider_tests {
    /// Regression guard for the 2026-06-12 prod boot panic: with ring and
    /// aws-lc-rs both linked, rustls has NO process default until one is
    /// installed. This asserts our install runs, wins, and is idempotent.
    #[test]
    fn install_is_idempotent_and_sets_a_default() {
        super::install_crypto_provider();
        super::install_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    install_crypto_provider();
    let config = tt_config::Config::from_env().map_err(|e| anyhow::anyhow!("config: {e}"))?;

    // Init Sentry before tracing. Guard is bound to the function's lifetime;
    // dropping it on shutdown flushes pending events.
    let _sentry_guard = config.sentry_dsn.as_deref().map(|dsn| {
        sentry::init((
            dsn,
            sentry::ClientOptions {
                release: sentry::release_name!(),
                send_default_pii: false,
                before_send: Some(std::sync::Arc::new(scrub_sensitive_event)),
                ..Default::default()
            },
        ))
    });

    // The telemetry tracing layer emits a JSON line (e.g. "tracing initialized")
    // to STDOUT on startup. That's fine for human commands, but it corrupts a
    // machine-readable `tt inspect --format json|sarif` report when that report
    // is printed to stdout (the GitHub Action does `... > results.sarif`, which
    // would otherwise prepend a log line and break SARIF parsing). When this
    // invocation is exactly that, silence the stdout log layer before init by
    // forcing the env filter off — unless the operator has set RUST_LOG, in
    // which case we honor their explicit choice.
    if inspect_emits_machine_output_to_stdout(std::env::args())
        && std::env::var_os("RUST_LOG").is_none()
    {
        // SAFETY: set before any threads are spawned that read RUST_LOG; tracing
        // init below reads it synchronously on this thread.
        std::env::set_var("RUST_LOG", "off");
    }

    // Initialize tracing via the telemetry crate so the OTLP span exporter
    // activates when `OTEL_EXPORTER_OTLP_ENDPOINT` is set (no-op JSON-stdout
    // otherwise). The guard is bound to `main`'s lifetime; dropping it on
    // shutdown flushes any buffered spans.
    let _tracing_guard = tt_telemetry::tracing::init("tokentrimmer")
        .map_err(|e| anyhow::anyhow!("tracing init: {e}"))?;

    let cli = Cli::parse();
    tt_cli::ui::init(cli.no_color);
    match cli.command {
        Command::Gateway { migrate_only } => {
            if migrate_only {
                let url = config
                    .database_url
                    .as_deref()
                    .context("--migrate-only requires DATABASE_URL")?;
                tt_core::db::migrate_only(url).await?;
                tt_cli::ui::success("migrations applied");
                return Ok(());
            }
            run_gateway(config).await?;
        }
        Command::Inspect {
            path,
            fail_on,
            output,
            format,
            cost_diff,
            base,
            fail_on_cost_increase,
            suggest_plan,
            from_db,
            org,
            window_days,
        } => {
            if cost_diff {
                run_cost_diff(&path, &base, output.as_deref(), fail_on_cost_increase)?;
            } else if suggest_plan {
                run_suggest_plan(
                    &path,
                    output.as_deref(),
                    from_db,
                    org.as_deref(),
                    window_days,
                )
                .await?;
            } else {
                run_inspect(&path, &fail_on, output.as_deref(), format.as_deref())?;
            }
        }
        Command::Plan {
            input,
            output,
            example,
            apply,
            yes,
        } => {
            run_plan(input.as_deref(), output.as_deref(), example, apply, yes).await?;
        }
        Command::Audit {
            action:
                AuditAction::Verify {
                    path,
                    org,
                    key,
                    key_hex,
                    expected_tip,
                },
        } => {
            audit::run_audit_verify(
                path.as_deref(),
                org.as_deref(),
                key.as_deref(),
                key_hex.as_deref(),
                expected_tip.as_deref(),
            )?;
        }
        Command::Mcp {
            transport,
            tt_api_key,
            tt_api_base,
            sse_port,
            allow_write,
            query_config,
        } => {
            use tt_mcp::{
                auth,
                cost::{CostControlBackend, UnconfiguredBackend},
                resources::{cost_ledger, inspect_baseline},
                tools::{
                    cost_control, find_route_for, inspect_diff, lookup_semantic_cache, preview_cost,
                },
                Server,
            };
            let ctx = tt_cli::context::ResolvedContext::load(tt_api_key, tt_api_base)?;
            let api_key = auth::validate_api_key(ctx.api_key_string())?;
            let tt_api_base = ctx.base_url;
            let mut server = Server::new().with_write_enabled(allow_write);
            server
                .tools
                .register(Box::new(preview_cost::PreviewCostTool));
            server
                .tools
                .register(Box::new(find_route_for::FindRouteForTool));
            server
                .tools
                .register(Box::new(inspect_diff::InspectDiffTool));
            server.tools.register(Box::new(
                tt_mcp::tools::get_repo_context::GetRepoContextTool,
            ));
            server
                .tools
                .register(Box::new(lookup_semantic_cache::LookupSemanticCacheTool {
                    base_url: tt_api_base.clone(),
                    api_key: api_key.clone(),
                    http: reqwest::Client::new(),
                }));
            server
                .tools
                .register(Box::new(tt_mcp::tools::simulate_plan::SimulatePlanTool {
                    base_url: tt_api_base.clone(),
                    api_key: api_key.clone(),
                    http: reqwest::Client::new(),
                }));
            server
                .resources
                .register(Box::new(cost_ledger::CostLedgerResource));
            server
                .resources
                .register(Box::new(inspect_baseline::InspectBaselineResource));
            server.resources.register(Box::new(
                tt_mcp::resources::plan_history::PlanHistoryResource {
                    base_url: tt_api_base.clone(),
                    api_key: api_key.clone(),
                    http: reqwest::Client::new(),
                },
            ));

            // Real, store-backed key verification (design §8): when a database is
            // configured, wire a Postgres key store and have the server verify the
            // operator's own key against it on the first tool/resource call,
            // caching the bound org for the process lifetime so tools act on the
            // right tenant. Invalid/absent/revoked → the call fails closed with
            // `unauthorized` (-32001) via `tt_auth::verify` (no reimplemented
            // crypto, no timing oracle). Without a DB there is no key store to
            // verify against, so we fall back to the transport's loopback bearer
            // guard alone — mirroring the gateway's documented dev-mode posture.
            if let Some(db_url) = config.database_url.as_deref() {
                match tt_core::connect(db_url, 5).await {
                    Ok(pool) => {
                        let store: std::sync::Arc<dyn tt_auth::KeyStore> =
                            std::sync::Arc::new(tt_auth::postgres::PostgresKeyStore::new(pool));
                        let authenticator = auth::Authenticator::new(store, api_key.clone());

                        // Eagerly resolve the bound org so the cost-control tools
                        // can be scoped to the verified tenant (set_cost_limit
                        // must only ever touch this org). This runs the same
                        // store-backed verify path the dispatcher uses; the
                        // OnceCell caches it so the first dispatch reuses it.
                        match authenticator.context().await {
                            Ok(ctx) => {
                                let org_id = ctx.org_id;
                                // PUBLIC-repo MVP: no per-org-key cost endpoint
                                // exists in the hosted API yet, so the cost tools
                                // run against the documented `UnconfiguredBackend`
                                // seam (clearly-marked responses, no fabricated
                                // numbers). A hosted deployment swaps in a real
                                // `CostControlBackend` here without changing the
                                // tool surface.
                                let backend: std::sync::Arc<dyn CostControlBackend> =
                                    std::sync::Arc::new(UnconfiguredBackend);
                                server
                                    .tools
                                    .register(Box::new(cost_control::GetSpendTodayTool {
                                        backend: backend.clone(),
                                        org_id,
                                    }));
                                server.tools.register(Box::new(
                                    cost_control::CheckBudgetRemainingTool {
                                        backend: backend.clone(),
                                        org_id,
                                    },
                                ));
                                server
                                    .tools
                                    .register(Box::new(cost_control::SetCostLimitTool {
                                        backend,
                                        org_id,
                                    }));
                                tracing::info!(
                                    "MCP cost-control tools registered (org-scoped); backend: unconfigured seam"
                                );
                                if allow_write {
                                    // The CLI has a single configured base; both
                                    // write targets (`POST /v1/routes` on the
                                    // gateway, `POST /v1/admin/plans/:id/apply`
                                    // on the plan surface) resolve against it —
                                    // the same convention the read tools
                                    // (simulate_plan, plan_history) already use.
                                    server.register_write_tools(
                                        org_id,
                                        tt_api_base.clone(),
                                        tt_api_base.clone(),
                                        api_key.clone(),
                                        reqwest::Client::new(),
                                    );
                                    tracing::info!(
                                        "MCP write tools registered (add_route, apply_plan; org-scoped)"
                                    );
                                }
                            }
                            Err(e) => {
                                if allow_write {
                                    anyhow::bail!(
                                        "--allow-write requires a verified operator key, but verification failed: {e}"
                                    );
                                }
                                tracing::error!(error = %e, "MCP operator key verification failed; cost-control tools not registered");
                            }
                        }

                        server = server.with_authenticator(authenticator);
                        tracing::info!(
                            "MCP key store: Postgres-backed (operator key verified on first call)"
                        );
                    }
                    Err(e) => {
                        if allow_write {
                            anyhow::bail!(
                                "--allow-write requires store-backed key verification, but the database connection failed: {e}"
                            );
                        }
                        tracing::error!(error = %e, "MCP db connect failed; serving with loopback bearer guard only (no store-backed verification)");
                    }
                }
            } else {
                if allow_write {
                    anyhow::bail!(
                        "--allow-write requires DATABASE_URL: write tools are org-scoped and need store-backed key verification; refusing to start a writable MCP server without it"
                    );
                }
                tracing::warn!("DATABASE_URL not set; MCP serving with loopback bearer guard only (no store-backed key verification); cost-control tools require a verified org and are not registered");
            }

            // Query-offload tools (`run_query`, `list_datasets`): the
            // operator config file is the explicit opt-in — no config means
            // the tools are never registered (`tools/list` omits them,
            // `tools/call` returns MethodNotFound). A supplied-but-invalid
            // config FAILS BOOT (fail closed, the --allow-write posture);
            // building the registry connects any postgres pools, so a bad
            // DSN also refuses to start. Query tools are local-data only and
            // need no DATABASE_URL/org binding.
            if let Some(cfg_path) = query_config {
                let qcfg = tt_mcp::query::QueryConfig::load(&cfg_path).map_err(|e| {
                    anyhow::anyhow!(
                        "--query-config {}: refusing to start: {e}",
                        cfg_path.display()
                    )
                })?;
                let registry = qcfg.build_registry().await.map_err(|e| {
                    anyhow::anyhow!(
                        "--query-config {}: refusing to start: {e}",
                        cfg_path.display()
                    )
                })?;
                let ledger_path = tt_mcp::query::ledger::ExecutionLedger::default_path();
                server.register_query_tools(
                    Arc::new(registry),
                    Arc::new(tt_mcp::query::cache::QueryCache::default()),
                    Arc::new(tt_mcp::query::ledger::ExecutionLedger::new(ledger_path)),
                    qcfg.limits(),
                );
                tracing::info!(
                    "MCP query tools registered (run_query, list_datasets; gated by the operator query config)"
                );
            }

            match transport.as_str() {
                "stdio" => {
                    server.run_stdio().await?;
                }
                "http" => {
                    let addr: std::net::SocketAddr = format!("127.0.0.1:{sse_port}")
                        .parse()
                        .context("invalid HTTP bind address")?;
                    server.run_http(addr, api_key).await?;
                }
                "sse" => {
                    // Deprecated HTTP+SSE transport (MCP 2024-11-05); prefer `http`.
                    let addr: std::net::SocketAddr = format!("127.0.0.1:{sse_port}")
                        .parse()
                        .context("invalid SSE bind address")?;
                    server.run_sse(addr, api_key).await?;
                }
                other => {
                    anyhow::bail!(
                        "unsupported MCP transport `{other}` (supported: stdio, http, sse[deprecated])"
                    )
                }
            }
        }
        Command::Login {
            token,
            base_url,
            no_browser,
        } => {
            tt_cli::account::login(token, base_url, no_browser)?;
        }
        Command::Logout => {
            tt_cli::account::logout()?;
        }
        Command::Whoami => {
            tt_cli::account::whoami()?;
        }
        Command::Chat {
            model,
            system,
            resume,
            tools,
            max_context,
            no_tool_trim,
            compact,
            compact_every,
            compact_model,
            tt_api_key,
            tt_api_base,
        } => {
            tt_cli::chat::run(tt_cli::chat::RunOpts {
                model,
                system,
                resume,
                tools,
                max_context,
                no_tool_trim,
                compact,
                compact_every,
                compact_model,
                flag_key: tt_api_key,
                flag_base: tt_api_base,
            })
            .await?;
        }
        Command::Models {
            tt_api_key,
            tt_api_base,
        } => {
            tt_cli::catalog::run(tt_api_key, tt_api_base).await?;
        }
        Command::Advise {
            path,
            describe,
            model,
            tt_api_key,
            tt_api_base,
        } => {
            tt_cli::advise::run(path, describe, model, tt_api_key, tt_api_base).await?;
        }
        Command::Embed {
            input,
            model,
            dimensions,
            encoding_format,
            cost_limit,
            json,
            tt_api_key,
            tt_api_base,
        } => {
            tt_cli::embed::run(
                input,
                model,
                dimensions,
                encoding_format,
                cost_limit,
                json,
                tt_api_key,
                tt_api_base,
            )
            .await?;
        }
        Command::Init {
            path,
            language,
            framework,
            interactive,
            upgrade,
            force,
            diff,
            skip_baseline,
            skip_hooks,
            skip_workflows,
            dry_run,
            ai,
            model,
            tt_api_key,
            tt_api_base,
        } => {
            use tt_cli::init::{run, RunOptions};
            let root = path
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap());
            let opts = RunOptions {
                root: root.clone(),
                language_override: language,
                framework_override: framework,
                interactive,
                upgrade,
                force,
                diff_only: diff,
                skip_baseline,
                skip_hooks,
                skip_workflows,
                dry_run,
                tt_cli_version: env!("CARGO_PKG_VERSION").to_string(),
            };
            // `init::run` already prints a styled summary (V1b); no second one.
            run(opts).context("tt init failed")?;
            // Opt-in AI pass: tailors the just-written artifacts. Skipped on a dry run.
            if ai && !dry_run {
                tt_cli::init::ai_tailor(&root, model, tt_api_key, tt_api_base)
                    .await
                    .context("tt init --ai pass failed")?;
            }
        }
        Command::Retrieval { action } => {
            use tt_cli::retrieval as cli_retrieval;
            match action {
                RetrievalAction::DocAdd {
                    corpus,
                    path,
                    openai_key,
                } => {
                    cli_retrieval::add_doc(&corpus, std::path::Path::new(&path), &openai_key)
                        .await?;
                }
                RetrievalAction::Search {
                    corpus,
                    query,
                    k,
                    openai_key,
                } => {
                    cli_retrieval::search(&corpus, &query, k, &openai_key).await?;
                }
            }
        }
        Command::Route {
            action,
            tt_api_key,
            tt_api_base,
        } => {
            use tt_cli::route::{AddArgs, CatalogCmd, RouteCmd};
            let cmd = match action {
                RouteAction::List => RouteCmd::List,
                RouteAction::Show { id } => RouteCmd::Show(id),
                RouteAction::Rm { id } => RouteCmd::Rm(id),
                RouteAction::Catalog { action } => RouteCmd::Catalog(match action {
                    CatalogAction::Enable => CatalogCmd::Enable,
                    CatalogAction::Disable => CatalogCmd::Disable,
                    CatalogAction::Status => CatalogCmd::Status,
                }),
                RouteAction::Add {
                    always,
                    from,
                    to,
                    when_has_images,
                    when_has_audio,
                    when_tag,
                    when_prompt_contains,
                    when_cost_gt,
                    when_cost_lt,
                    when_p95_gt,
                    max_cost,
                    disable_cache,
                    batch,
                    agentic_budget,
                    keep_recent,
                    elide_stale_tools,
                    route_mechanical_to,
                    priority,
                    name,
                    fallback,
                    disabled,
                } => RouteCmd::Add(Box::new(AddArgs {
                    always,
                    from,
                    to,
                    when_has_images,
                    when_has_audio,
                    when_tag,
                    when_prompt_contains,
                    when_cost_gt,
                    when_cost_lt,
                    when_p95_gt,
                    max_cost,
                    disable_cache,
                    batch,
                    agentic_budget,
                    keep_recent,
                    elide_stale_tools,
                    route_mechanical_to,
                    priority,
                    name,
                    fallback,
                    disabled,
                })),
            };
            if let Err(e) = tt_cli::route::run(cmd, tt_api_key, tt_api_base).await {
                tt_cli::ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
        Command::Recipes {
            action,
            tt_api_key,
            tt_api_base,
        } => {
            use tt_cli::recipes::RecipesCmd;
            let cmd = match action {
                RecipesAction::List => RecipesCmd::List,
                RecipesAction::Show { name } => RecipesCmd::Show(name),
                RecipesAction::Apply { name } => RecipesCmd::Apply(name),
            };
            if let Err(e) = tt_cli::recipes::run(cmd, tt_api_key, tt_api_base).await {
                tt_cli::ui::error(&format!("{e:#}"));
                std::process::exit(1);
            }
        }
        Command::Proxy {
            port,
            bind,
            mode,
            tt_api_key,
            tt_api_base,
            no_tui,
            no_preview,
            session_log,
        } => {
            use tt_cli::proxy::{
                config::{Config, Mode},
                listener::run as run_listener,
            };
            let bind_addr: std::net::IpAddr = bind.parse().context("invalid --bind address")?;
            let mode = Mode::parse(&mode).context("invalid --mode (gateway|bypass|hybrid)")?;
            let ctx = tt_cli::context::ResolvedContext::load(tt_api_key, tt_api_base)?;
            let api_key = ctx.api_key_string();
            if mode == Mode::Gateway && api_key.is_none() {
                anyhow::bail!(
                    "--mode gateway requires a key — run `tt login --token <KEY>`, \
                     pass --tt-api-key, or set TT_API_KEY"
                );
            }
            let mut cfg = Config::build(
                port,
                bind_addr,
                mode,
                api_key,
                no_tui,
                no_preview,
                session_log.map(std::path::PathBuf::from),
            );
            cfg.gateway_base_url = ctx.base_url;
            run_listener(cfg).await.context("tt proxy listener")?;
        }
        Command::Context {
            task,
            path,
            format,
            max_files,
            token_budget,
        } => {
            repo_context::run(&path, &task, &format, max_files, token_budget)?;
        }
        Command::Agent {
            action,
            tt_api_key,
            tt_api_base,
        } => match action {
            AgentAction::Run {
                prompt,
                model,
                system,
                tools,
                max_turns,
                tag,
            } => {
                tt_cli::agent::run(tt_cli::agent::RunOpts {
                    prompt,
                    model,
                    system,
                    tools,
                    max_turns,
                    tag,
                    flag_key: tt_api_key,
                    flag_base: tt_api_base,
                })
                .await?;
            }
        },
        Command::Batch {
            action,
            tt_api_key,
            tt_api_base,
        } => {
            tt_cli::batch::run(tt_cli::batch::RunOpts {
                action,
                flag_key: tt_api_key,
                flag_base: tt_api_base,
            })
            .await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Sentry event scrubbing — runs before any event is sent upstream.
// ---------------------------------------------------------------------------

/// Names that may carry secrets when a panic captures locals or request data.
/// Matched case-insensitively against header names, frame variable names, and
/// extra-field keys before the event leaves the process.
const SENSITIVE_KEY_FRAGMENTS: &[&str] = &[
    "authorization",
    "api_key",
    "apikey",
    "api-key",
    "token",
    "secret",
    "password",
    "cookie",
    "tt_master_key",
    "tt_live_",
    "tt_test_",
    "sk_live_",
    "sk_test_",
    "bearer",
    "database_url",
    "redis_url",
    "sentry_dsn",
];

const SCRUB_PLACEHOLDER: &str = "[Filtered]";

fn key_is_sensitive(k: &str) -> bool {
    let lower = k.to_ascii_lowercase();
    SENSITIVE_KEY_FRAGMENTS.iter().any(|f| lower.contains(f))
}

/// Strip sensitive values from an Event before Sentry uploads it.
///
/// Conservative: we redact whole fields whose name *looks* sensitive rather
/// than trying to detect secrets in arbitrary value bodies. False positives
/// (scrubbing something that wasn't actually a secret) are vastly preferable
/// to false negatives.
fn scrub_sensitive_event(
    mut event: sentry::protocol::Event<'static>,
) -> Option<sentry::protocol::Event<'static>> {
    // Request headers / cookies / query string are the most common leak path.
    if let Some(req) = event.request.as_mut() {
        req.cookies = None;
        req.query_string = None;
        for (k, v) in req.headers.iter_mut() {
            if key_is_sensitive(k) {
                *v = SCRUB_PLACEHOLDER.to_string();
            }
        }
        for (k, v) in req.env.iter_mut() {
            if key_is_sensitive(k) {
                *v = SCRUB_PLACEHOLDER.to_string();
            }
        }
    }

    // Exception stacktraces can carry captured locals.
    for ex in event.exception.iter_mut() {
        if let Some(st) = ex.stacktrace.as_mut() {
            for frame in st.frames.iter_mut() {
                for (k, v) in frame.vars.iter_mut() {
                    if key_is_sensitive(k) {
                        *v = serde_json::Value::String(SCRUB_PLACEHOLDER.to_string());
                    }
                }
            }
        }
    }
    if let Some(st) = event.stacktrace.as_mut() {
        for frame in st.frames.iter_mut() {
            for (k, v) in frame.vars.iter_mut() {
                if key_is_sensitive(k) {
                    *v = serde_json::Value::String(SCRUB_PLACEHOLDER.to_string());
                }
            }
        }
    }

    // Extras and tags that downstream code attached.
    for (k, v) in event.extra.iter_mut() {
        if key_is_sensitive(k) {
            *v = serde_json::Value::String(SCRUB_PLACEHOLDER.to_string());
        }
    }
    for (k, v) in event.tags.iter_mut() {
        if key_is_sensitive(k) {
            *v = SCRUB_PLACEHOLDER.to_string();
        }
    }

    Some(event)
}

// ---------------------------------------------------------------------------
// `tt gateway` implementation
// ---------------------------------------------------------------------------

/// Opt-in env var that allows binding a non-loopback address WITHOUT a key
/// store. With it set to `1` the gateway serves unauthenticated traffic as a
/// BYO-key passthrough — callers supply their own upstream provider key as
/// the Bearer token; the operator's env provider keys are still never served.
const ALLOW_UNAUTHENTICATED_PUBLIC_BIND_VAR: &str = "TT_ALLOW_UNAUTHENTICATED_PUBLIC_BIND";

/// Decide which IP the gateway binds, failing closed when an unauthenticated
/// deployment would be exposed beyond loopback.
///
/// Without a persistent key store the auth middleware cannot verify anyone —
/// every caller is anonymous — so a non-loopback bind would serve the open
/// internet (or LAN) as an anonymous proxy. The matrix:
///
/// * key store configured → today's behavior: `TT_BIND_ADDR` or 0.0.0.0.
/// * no key store, no `TT_BIND_ADDR` → loopback (dev mode keeps working).
/// * no key store, loopback `TT_BIND_ADDR` → honored.
/// * no key store, non-loopback `TT_BIND_ADDR` → refused unless
///   `TT_ALLOW_UNAUTHENTICATED_PUBLIC_BIND=1`.
fn resolve_gateway_bind(
    configured: Option<std::net::IpAddr>,
    key_store_configured: bool,
    public_opt_in: bool,
) -> anyhow::Result<std::net::IpAddr> {
    use std::net::{IpAddr, Ipv4Addr};

    if key_store_configured {
        return Ok(configured.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)));
    }
    match configured {
        None => Ok(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        Some(ip) if ip.is_loopback() || public_opt_in => Ok(ip),
        Some(ip) => anyhow::bail!(
            "refusing to bind {ip}: no persistent key store is configured (DATABASE_URL \
             unset), so every caller is anonymous and a non-loopback bind would serve an \
             unauthenticated proxy. Fix one of: set DATABASE_URL to enable tt_live_* key \
             verification; bind loopback (unset TT_BIND_ADDR or set TT_BIND_ADDR=127.0.0.1); \
             or, to knowingly run an unauthenticated BYO-key passthrough, set \
             {ALLOW_UNAUTHENTICATED_PUBLIC_BIND_VAR}=1"
        ),
    }
}

/// Opt-in env var that re-enables the process-env credential fallback behind
/// the Postgres store (see [`tt_auth::ALLOW_ENV_CREDENTIAL_FALLBACK_VAR`]).
const ALLOW_ENV_CREDENTIAL_FALLBACK_VAR: &str = tt_auth::ALLOW_ENV_CREDENTIAL_FALLBACK_VAR;

/// Build the provider credential store for the gateway.
///
/// * No DB pool → there is no key store, nothing can verify callers, and the
///   operator's env provider keys (`OPENAI_API_KEY`, …) must never be served:
///   no credential store is wired at all, which makes the chat handler fall
///   back to forwarding the caller's own Bearer key upstream (dev mode,
///   loopback-guarded at boot).
/// * DB pool + valid `TT_MASTER_KEY` → the per-org Postgres store, **BYO-only
///   by default** (P0 #9): an org with no stored credential for a provider
///   gets an actionable `missing_provider_credential` error — it never
///   silently rides the operator's env keys (provider-ToS / resale and
///   surprise-spend exposure on the hosted gateway). Setting
///   `TT_ALLOW_ENV_CREDENTIAL_FALLBACK=1` chains the env store behind
///   Postgres for single-tenant self-host / dogfood deployments.
/// * DB pool + missing/bad `TT_MASTER_KEY` → fail closed (no credential
///   store; verified orgs cannot resolve upstream credentials) unless the env
///   fallback is explicitly opted in, which restores the env-only dogfood
///   mode.
fn build_credential_store(
    db_pool: Option<&sqlx::PgPool>,
    allow_env_fallback: bool,
) -> Option<Arc<dyn tt_auth::ProviderCredentialStore>> {
    let pool = match db_pool {
        Some(pool) => pool,
        None => {
            tracing::warn!(
                "no DB pool → no key store; provider credential store disabled — operator env \
                 provider keys are never served to unverified callers (requests forward the \
                 caller's own Bearer key upstream)"
            );
            return None;
        }
    };
    match tt_auth::postgres::PostgresProviderCredentialStore::from_env(pool.clone()) {
        Ok(pg) if allow_env_fallback => {
            tracing::warn!(
                "provider credentials: Postgres primary + process-env fallback \
                 ({ALLOW_ENV_CREDENTIAL_FALLBACK_VAR}=1). Single-tenant/self-host only — every \
                 org without a stored credential is served the operator's env provider keys"
            );
            Some(Arc::new(tt_auth::ChainedProviderCredentialStore::new(
                pg,
                tt_auth::EnvProviderCredentialStore::new(),
            )))
        }
        Ok(pg) => {
            tracing::info!(
                "provider credentials: Postgres only (BYO-only). Orgs without a stored \
                 credential get `missing_provider_credential`; set \
                 {ALLOW_ENV_CREDENTIAL_FALLBACK_VAR}=1 to re-enable the process-env fallback \
                 for single-tenant/dogfood deployments"
            );
            Some(Arc::new(pg))
        }
        Err(e) if allow_env_fallback => {
            tracing::warn!(
                error = %e,
                "Postgres credential store unavailable (TT_MASTER_KEY missing / bad); serving \
                 env-only provider credentials ({ALLOW_ENV_CREDENTIAL_FALLBACK_VAR}=1)"
            );
            Some(Arc::new(tt_auth::EnvProviderCredentialStore::new()))
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                "Postgres credential store unavailable (TT_MASTER_KEY missing / bad) and \
                 {ALLOW_ENV_CREDENTIAL_FALLBACK_VAR} is not set — wiring NO credential store. \
                 Operator env provider keys are never served; verified orgs cannot resolve \
                 upstream credentials until TT_MASTER_KEY is fixed (or, for single-tenant \
                 deployments, the env fallback is explicitly opted in)"
            );
            None
        }
    }
}

/// Boot the Gateway HTTP server.
///
/// Reads config from env (see [`tt_config::Config::from_env`]). Every external
/// dependency (DB, Redis) is best-effort at boot: a failure logs + continues
/// rather than crash-looping the process. Bind / serve are fatal — including
/// the fail-closed refusal to bind a non-loopback address without a key store
/// (see [`resolve_gateway_bind`]).
async fn run_gateway(config: tt_config::Config) -> anyhow::Result<()> {
    // Fail-closed bind decision, BEFORE any best-effort dependency connects:
    // a misconfigured public + unauthenticated gateway must not boot at all.
    let key_store_configured = config.database_url.is_some();
    let public_opt_in = std::env::var(ALLOW_UNAUTHENTICATED_PUBLIC_BIND_VAR).as_deref() == Ok("1");
    let bind_ip = resolve_gateway_bind(config.bind_addr, key_store_configured, public_opt_in)?;
    if !key_store_configured {
        if bind_ip.is_loopback() {
            tracing::info!(
                %bind_ip,
                "no key store configured (DATABASE_URL unset); binding loopback. Set \
                 DATABASE_URL for tt_live_* verification, or TT_BIND_ADDR + \
                 {ALLOW_UNAUTHENTICATED_PUBLIC_BIND_VAR}=1 to expose an unauthenticated \
                 BYO-key passthrough anyway"
            );
        } else {
            tracing::warn!(
                %bind_ip,
                "{ALLOW_UNAUTHENTICATED_PUBLIC_BIND_VAR}=1: serving UNAUTHENTICATED traffic \
                 on a non-loopback address. Callers must bring their own upstream provider \
                 key as the Bearer token; operator env provider keys are never served"
            );
        }
    }
    let bind = std::net::SocketAddr::new(bind_ip, config.port);

    // Every external connect is wrapped in a 5s budget so a misconfigured
    // hostname can't hang the process past Fly's health-check grace window.
    let boot_timeout = std::time::Duration::from_secs(5);

    // DB best-effort: keep the pool around for downstream wiring
    // (Postgres credential store, request_logs writer when that lands).
    // Serverless Postgres (Neon scale-to-zero) can exceed sqlx's default
    // acquire timeout on first connect, so the connect is guarded by a
    // boot-time budget.
    let db_pool: Option<sqlx::PgPool> = match config.database_url.as_deref() {
        Some(url) => {
            tracing::info!("connecting to database");
            match tokio::time::timeout(boot_timeout, tt_core::connect(url, 10)).await {
                Ok(Ok(pool)) => {
                    match tt_core::migrate(&pool).await {
                        Ok(()) => tracing::info!("migrations applied"),
                        Err(e) => tracing::error!(error = %e, "migrations failed; continuing"),
                    }
                    Some(pool)
                }
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "db connect failed; continuing without persistence");
                    None
                }
                Err(_) => {
                    tracing::error!(
                        timeout_secs = boot_timeout.as_secs(),
                        "db connect timed out; continuing without persistence"
                    );
                    None
                }
            }
        }
        None => {
            tracing::warn!("DATABASE_URL not set; gateway running without persistence");
            None
        }
    };

    // L1 Redis best-effort. Same timeout budget — a HTTP-REST URL passed
    // where a `rediss://` URL belongs will hang `ConnectionManager::new`.
    let l1_cache: Option<Arc<dyn tt_cache::L1Cache>> = match config.redis_url.as_deref() {
        Some(url) => {
            tracing::info!("connecting to redis (L1 cache)");
            match tokio::time::timeout(
                boot_timeout,
                tt_cache::redis_impl::RedisL1Cache::connect(url, "tt:l1"),
            )
            .await
            {
                Ok(Ok(c)) => {
                    // SEC-2: encrypt L1 (Redis) response payloads at rest when
                    // `TT_MASTER_KEY` is set. Unset → plaintext (back-compat); a
                    // malformed key disables L1 rather than serve plaintext under
                    // a misconfigured key.
                    match tt_cache::ResponseCodec::from_env() {
                        Ok(Some(codec)) => {
                            tracing::info!(
                                "L1 cache enabled (response encryption on — TT_MASTER_KEY)"
                            );
                            Some(Arc::new(c.with_response_codec(codec))
                                as Arc<dyn tt_cache::L1Cache>)
                        }
                        Ok(None) => {
                            tracing::info!("L1 cache enabled (plaintext — TT_MASTER_KEY unset)");
                            Some(Arc::new(c) as Arc<dyn tt_cache::L1Cache>)
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "TT_MASTER_KEY invalid — L1 cache disabled (refusing to serve plaintext under a misconfigured key)");
                            None
                        }
                    }
                }
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "redis connect failed; L1 cache disabled");
                    None
                }
                Err(_) => {
                    tracing::error!(
                        timeout_secs = boot_timeout.as_secs(),
                        "redis connect timed out; L1 cache disabled (check URL format — needs rediss:// native, not https:// REST)"
                    );
                    None
                }
            }
        }
        None => {
            tracing::warn!("REDIS_URL not set; L1 cache disabled");
            None
        }
    };

    // REL-3: create a TaskTracker so detached telemetry writes (request_logs,
    // body capture) can be drained on graceful shutdown instead of abandoned
    // on every rolling deploy / SIGTERM.
    let telemetry_tracker = tokio_util::task::TaskTracker::new();

    let mut state = tt_core::AppState::with_default_providers()
        .with_telemetry_tracker(telemetry_tracker.clone());
    if let Some(l1) = l1_cache {
        state = state.with_l1(l1, None);
    }

    // REL-1: hand the Postgres pool to AppState so the `/ready` probe actually
    // checks the DB (`SELECT 1`) instead of always reporting `not_configured`.
    // Clone — the pool is reused below for the credential/key/tier/routing
    // stores. No-op when there is no DB pool (probe keeps reporting
    // `not_configured`, the honest state).
    if let Some(pool) = db_pool.as_ref() {
        state = state.with_db_pool(pool.clone());
    }

    // Surface a stale embedded pricing catalog (the dormant freshness signal).
    const PRICING_STALE_DAYS: i64 = 90;
    let newest_pricing = tt_shared::pricing::catalog().catalog_max_effective_at();
    if let Some(d) = newest_pricing {
        if tt_shared::pricing::is_stale(Some(d), chrono::Utc::now(), PRICING_STALE_DAYS) {
            tracing::warn!(
                newest_effective_at = %d,
                "pricing catalog is over {PRICING_STALE_DAYS} days old — rates may be stale; refresh data/pricing.toml"
            );
        }
    }

    // Provider credentials: Postgres per-org store (when DB + `TT_MASTER_KEY`
    // are configured), BYO-only by default — an org that hasn't onboarded a
    // provider credential gets `missing_provider_credential`, never the
    // operator's env keys (P0 #9). Setting TT_ALLOW_ENV_CREDENTIAL_FALLBACK=1
    // chains the env-backed fallback (`OPENAI_API_KEY` / `ANTHROPIC_API_KEY` /
    // … in Fly secrets) behind Postgres for single-tenant dogfooding.
    //
    // SECURITY: without a DB pool there is no key store either, so callers
    // can't be verified — `build_credential_store` then wires NO store, so the
    // operator's env keys are unreachable and requests fall back to the
    // caller's own Bearer key (P0 #21 fail-closed).
    if let Some(credential_store) = build_credential_store(
        db_pool.as_ref(),
        tt_auth::env_credential_fallback_opted_in(),
    ) {
        state = state.with_credential_store(credential_store);
    }

    // Key store: Postgres when a DB pool is available. Without a key store
    // the auth middleware passes `tt_live_*` through unchallenged (dev mode);
    // with the Postgres store it does real argon2 verify against the
    // `api_keys` table populated by the cloud-repo dashboard (or the
    // forthcoming hosted issuance endpoint).
    if let Some(pool) = db_pool.as_ref() {
        state = state.with_key_store(Arc::new(tt_auth::postgres::PostgresKeyStore::new(
            pool.clone(),
        )));
        tracing::info!("key store: Postgres-backed (tt_live_* verification enabled)");
    } else {
        tracing::warn!("no DB pool; tt_live_* keys pass through without verification (dev mode)");
    }

    // Tier resolver: Postgres when available. Resolves each org's subscription
    // (tier, status) + self-serve budget cap (`org_budget_caps`) into the
    // per-request `BudgetLimits` the auth middleware enforces via
    // `dynamic_budget`. Without it the middleware enforces NO per-org tier
    // limits or budget caps (rv-tier-limits-enforcement / rv-budget-cap-ui).
    // Wrapped in a 30s-TTL cache so a webhook tier change propagates within a
    // minute without a DB hit per request. Fail-open: a resolver error falls
    // back to Free defaults, never blocking legitimate traffic.
    if let Some(pool) = db_pool.as_ref() {
        let resolver = tt_core::tier_resolver::PostgresTierResolver::new(pool.clone());
        state = state.with_tier_resolver(Arc::new(
            tt_core::tier_resolver::CachedTierResolver::new(resolver),
        ));
        tracing::info!(
            "tier resolver: Postgres-backed (per-org tier limits + budget caps enforced)"
        );
    } else {
        tracing::warn!("no DB pool; per-org tier limits + budget caps NOT enforced");
    }

    // Request-log writer: Postgres when available. The dashboard's
    // `/api/telemetry` endpoint reads from this table for spend / savings
    // / cache hit rate cards.
    if let Some(pool) = db_pool.as_ref() {
        state = state.with_request_log_writer(Arc::new(
            tt_telemetry::request_logs::postgres::PostgresRequestLogWriter::new(pool.clone()),
        ));
        tracing::info!("request_logs writer: Postgres-backed");
    } else {
        tracing::warn!("no DB pool; request_logs writes disabled");
    }

    // Encrypted body capture: DB + TT_MASTER_KEY arms the sink, but writes
    // remain per-org opt-in via request_body_capture_settings.
    if let Some(pool) = db_pool.as_ref() {
        match tt_telemetry::body_capture::postgres::PostgresBodyCaptureWriter::from_env(
            pool.clone(),
        ) {
            Ok(Some(writer)) => {
                state = state.with_body_capture_writer(Arc::new(writer));
                tracing::info!("request body capture writer: Postgres-backed (per-org opt-in)");
            }
            Ok(None) => tracing::warn!(
                "TT_MASTER_KEY unset; encrypted request body capture writes disabled"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                "TT_MASTER_KEY invalid; encrypted request body capture writes disabled"
            ),
        }
    } else {
        tracing::warn!("no DB pool; encrypted request body capture writes disabled");
    }

    // Sampled paired A/B quality judge — opt-in via TT_JUDGE_ENABLED (off by
    // default; request/response semantics change for nobody who hasn't opted
    // in). When enabled, verdicts feed the in-memory band store (live
    // /v1/preview enrichment) and — with a DB pool — land durably in the
    // `quality_verdicts` table (migration 0014, applied by the standard boot
    // migration path above) for Phase 2 attribution netting. Judge + baseline
    // costs are measurement tax recorded only there, never in request_logs.
    let judge_config = tt_core::quality_sample::JudgeConfig::from_env();
    let judge_enabled = judge_config.enabled;
    if judge_config.enabled {
        let band_store = Arc::new(tt_core::quality_sample::InMemoryJudgeBandStore::new());
        state = if let Some(pool) = db_pool.as_ref() {
            tracing::info!(
                "quality judge: paired A/B sampler enabled (verdicts → Postgres quality_verdicts)"
            );
            state.with_quality_judge_persistent(
                band_store,
                Arc::new(tt_core::quality_persist::PostgresJudgeSink::new(
                    pool.clone(),
                )),
                judge_config,
            )
        } else {
            tracing::warn!(
                "quality judge: enabled without DB pool — verdicts in-memory only (not persisted)"
            );
            state.with_quality_judge_band_store(band_store, judge_config)
        };
    }

    // L2 semantic cache — opt-in via TT_L2_SEMANTIC_CACHE=1. Needs a pgvector
    // DB pool and an OpenAI embedding key (TT_OPENAI_EMBED_KEY); the embedder
    // reuses the registered OpenAI provider. A misconfig (flag on, dependency
    // missing) degrades to a warning + L2 off rather than failing boot.
    // NOTE: L2-hit savings currently use a synthetic baseline (chat.rs); an
    // honest per-row baseline needs a `cache_entries.baseline_cost_usd` column
    // (cloud migration) — tracked separately.
    if std::env::var("TT_L2_SEMANTIC_CACHE").as_deref() == Ok("1") {
        match (
            db_pool.as_ref(),
            std::env::var("TT_OPENAI_EMBED_KEY").ok(),
            state.registry.by_id("openai"),
        ) {
            (Some(pool), Some(key), Some(openai)) => {
                let creds = tt_shared::context::ProviderCredentials {
                    api_key: tt_shared::context::SecretString::new(key),
                    base_url: None,
                    extra_headers: Vec::new(),
                };
                // COST-4(I): wrap the OpenAI embedder in the bounded in-process
                // LRU so repeated identical lookup/insert texts (re-asked
                // questions, retried requests) reuse a cached vector instead of
                // re-paying the embedding call. Pure win — `MemoizingEmbedder`
                // is transparent on a miss and exact on a hit.
                let base_embedder: Arc<dyn tt_cache::EmbeddingProvider> = Arc::new(
                    tt_cache::OpenAIEmbedder::new(openai, "text-embedding-3-small", creds),
                );
                let embedder: Arc<dyn tt_cache::EmbeddingProvider> =
                    Arc::new(tt_cache::MemoizingEmbedder::new(base_embedder));
                // SEC-2: install the per-org response codec when `TT_MASTER_KEY`
                // is set — L2 rows are then encrypted at rest. Unset → `None` →
                // today's plaintext behavior (back-compat). A *malformed*
                // master key disables L2 (warn) rather than silently serving
                // plaintext under a misconfigured key.
                let l2_with_codec = match tt_cache::ResponseCodec::from_env() {
                    Ok(Some(codec)) => {
                        tracing::info!(
                            "L2 response encryption enabled (TT_MASTER_KEY — rows encrypted at rest)"
                        );
                        Some(tt_cache::PostgresL2Cache::new(pool.clone()).with_response_codec(codec))
                    }
                    Ok(None) => {
                        tracing::info!(
                            "L2 response encryption disabled (TT_MASTER_KEY unset — plaintext rows, back-compat)"
                        );
                        Some(tt_cache::PostgresL2Cache::new(pool.clone()))
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "TT_MASTER_KEY invalid — L2 disabled (refusing to serve plaintext under a misconfigured key)");
                        None
                    }
                };
                if let Some(l2) = l2_with_codec {
                    state = state.with_l2(Arc::new(l2), embedder, None);
                    tracing::info!(
                        "L2 semantic cache enabled (pgvector + text-embedding-3-small; LRU-memoized embedder)"
                    );
                }
            }
            _ => tracing::warn!(
                "TT_L2_SEMANTIC_CACHE=1 but DATABASE_URL / TT_OPENAI_EMBED_KEY missing — L2 disabled"
            ),
        }
    }

    // L2 false-positive verify gate — opt-in via TT_L2_VERIFY=1 (research
    // Phase 2.2). Ambiguous-band hits are lexically verified before serving,
    // and judged in-band verdicts adapt the per-class threshold upward (never
    // below the 0.92 floor) to hold the FP rate at the route tolerance.
    // No-op (warn) when L2 itself is off. All knobs clamp to their documented
    // safe ranges inside `with_l2_verify`; malformed values fall back to the
    // defaults rather than failing boot (mirrors the TT_L2_SEMANTIC_CACHE
    // degradation pattern).
    if std::env::var("TT_L2_VERIFY").as_deref() == Ok("1") {
        if state.l2.is_some() {
            let parse_f32 = |key: &str, default: f32| {
                std::env::var(key)
                    .ok()
                    .and_then(|v| v.trim().parse::<f32>().ok())
                    .unwrap_or(default)
            };
            let parse_f64 = |key: &str, default: f64| {
                std::env::var(key)
                    .ok()
                    .and_then(|v| v.trim().parse::<f64>().ok())
                    .unwrap_or(default)
            };
            let epsilon = parse_f32("TT_L2_VERIFY_EPSILON", 0.02);
            let min_agreement = parse_f32(
                "TT_L2_VERIFY_MIN_AGREEMENT",
                tt_cache::DEFAULT_LEXICAL_MIN_AGREEMENT,
            );
            let tolerance_pct = parse_f64("TT_L2_FP_TOLERANCE_PCT", 1.0);
            let min_samples = std::env::var("TT_L2_FP_MIN_SAMPLES")
                .ok()
                .and_then(|v| v.trim().parse::<u32>().ok())
                .unwrap_or(20);
            let tuning = tt_cache::FpGateTuning::new(min_samples, 0.005);
            state = state.with_l2_verify(epsilon, min_agreement, tolerance_pct, tuning);
            tracing::info!(
                epsilon,
                min_agreement,
                tolerance_pct,
                min_samples,
                "L2 verify gate enabled (FP gate on ambiguous-band hits)"
            );
        } else {
            tracing::warn!(
                "TT_L2_VERIFY=1 but the L2 semantic cache is off — verify gate disabled"
            );
        }
    }

    // L2 volatility-class TTL — ON BY DEFAULT (P2). Volatile queries
    // (news/realtime/version-ish) get a shortened L2 TTL (shorten-only,
    // floor-bounded; explicit per-request TTLs always win). `with_l2` already
    // seeds the default config, so the work here is: honor the optional
    // multiplier/floor overrides, or disable entirely with
    // TT_L2_VOLATILITY_TTL=0. Default-on is safe because the lane is
    // shorten-only — a misclassification's worst case is an early re-dispatch
    // (a cache miss), never a stale answer.
    if state.l2.is_some() {
        if std::env::var("TT_L2_VOLATILITY_TTL").as_deref() == Ok("0") {
            state = state.without_l2_volatility_ttl();
            tracing::info!("L2 volatility-class TTL disabled (TT_L2_VOLATILITY_TTL=0)");
        } else {
            let default_cfg = tt_core::state::L2VolatilityTtl::default();
            let volatile_multiplier = std::env::var("TT_L2_VOLATILE_TTL_MULTIPLIER")
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .unwrap_or(default_cfg.volatile_multiplier);
            let floor_secs = std::env::var("TT_L2_VOLATILE_TTL_FLOOR_SECS")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(default_cfg.floor_secs);
            state = state.with_l2_volatility_ttl(tt_core::state::L2VolatilityTtl {
                volatile_multiplier,
                floor_secs,
            });
            tracing::info!(
                volatile_multiplier,
                floor_secs,
                "L2 volatility-class TTL enabled (volatile entries expire sooner; default-on)"
            );
        }
    }

    // Routing store: Postgres when available. Reads the `routes` table that
    // the cloud dashboard writes (cloud schema migration 0002). Wrapped in
    // the 60s per-org cache so the chat hot path doesn't hit the DB on
    // every request — the dashboard surfaces this latency budget as
    // "routes refresh every ~60 seconds".
    if let Some(pool) = db_pool.as_ref() {
        let backing: Arc<dyn tt_routing::RoutingStore> =
            Arc::new(tt_routing::PostgresRoutingStore::new(pool.clone()));
        let routing_store = Arc::new(tt_routing::CachingRoutingStore::new(backing));
        state = state.with_routing_store(routing_store.clone());
        tracing::info!("routing store: Postgres-backed (60s per-org cache)");

        // Route-level netted savings read-side (GET /v1/routes/:id/savings).
        // Pure read-side aggregation over request_logs × quality_verdicts —
        // wiring it changes no request-path behavior. Without a DB pool the
        // endpoint keeps answering 503 (aggregation not configured).
        state = state.with_route_savings(Arc::new(
            tt_core::route_savings::PostgresRouteSavingsSource::new(pool.clone()),
        ));
        tracing::info!("route savings: Postgres-backed netting (GET /v1/routes/:id/savings)");

        // Opt-in quality auto-pause evaluator. Doubly gated: the judge must
        // be enabled (TT_JUDGE_ENABLED → a persistent sink was wired above)
        // AND each route must set `then.auto_pause: true`. Appended AFTER the
        // persistent sink so the just-recorded verdict is already in the
        // durable window when the evaluator consults it. Without this wiring
        // a route's `auto_pause` flag validates but never fires.
        if judge_enabled {
            let window = Arc::new(tt_core::route_autopause::PgVerdictWindow::new(pool.clone()));
            state = state.with_route_auto_pause(Arc::new(
                tt_core::route_autopause::AutoPauseJudgeSink::new(routing_store, window),
            ));
            tracing::info!(
                "route auto-pause: evaluator wired (fires only on routes with auto_pause: true)"
            );
        }
    } else if std::env::var("TT_DOGFOOD_GROQ_ROUTING").as_deref() == Ok("1") {
        // Dogfood mode: seed an in-memory route that redirects short flagship
        // model prompts to Groq's llama-3.1-8b-instant for internal testing.
        let backing = Arc::new(tt_routing::InMemoryRoutingStore::new());
        backing.set_routes(
            tt_core::DOGFOOD_ORG_ID,
            vec![tt_routing::Route {
                paused: false,
                id: uuid::Uuid::now_v7(),
                name: "dogfood-short-prompts-to-groq".into(),
                priority: 100,
                enabled: true,
                when: tt_routing::RouteConditions {
                    model_in: vec![
                        "claude-sonnet-4-6".into(),
                        "claude-opus-4-7".into(),
                        "gpt-4o".into(),
                        "gpt-4-turbo".into(),
                    ],
                    input_tokens_lt: Some(200),
                    ..Default::default()
                },
                then: tt_routing::RouteAction {
                    format_switch: None,
                    diff: false,
                    auto_pause: false,
                    pause_floor_pass_rate: None,
                    pause_min_verdicts: None,
                    minify_json: false,
                    reasoning_max_effort: None,
                    reasoning_budget_tokens: None,
                    agentic_budget: None,
                    target_model: Some("llama-3.1-8b-instant".into()),
                    fallbacks: Vec::new(),
                    disable_cache: false,
                    max_cost_usd: None,
                    flex: false,
                    batch: false,
                    compress: false,
                    redact: false,
                    traffic_pct: None,
                    shadow_model: None,
                    panel: None,
                },
            }],
        );
        let caching: Arc<dyn tt_routing::RoutingStore> = backing;
        state = state
            .with_routing_store(Arc::new(tt_routing::CachingRoutingStore::new(caching)))
            // SEC-6: dogfood must fail closed off a loopback bind — the resolved
            // `bind_ip` gates it so an accidentally-public dogfood gateway does
            // not route real traffic to the internal Groq lane.
            .with_dogfood_enabled_for_bind(bind_ip.is_loopback());
        tracing::info!(
            "dogfood routing: short prompts on flagship models → llama-3.1-8b-instant (Groq)"
        );
    } else {
        tracing::warn!("no DB pool; routing disabled (chat requests pass through unrouted)");
    }

    // Deep-research panel kill-switch: off by default; set TT_PANEL_ENABLED=1
    // or TT_PANEL_ENABLED=true to enable. Panel requests are rejected with
    // `panel_disabled` (403) unless this is set — never a silent single-model
    // fallback.
    state = state.with_panel_enabled(tt_core::panel_enabled_from_env());
    state = state.with_panel_min_tier(tt_core::panel_min_tier_from_env());

    // Start background catalogue refreshers (OpenRouter's dynamic `GET /models`
    // fetch). Best-effort + non-blocking: it refreshes once shortly after boot
    // and hourly thereafter, so the live 300+ model catalogue + per-model
    // pricing feed dispatch/cost. A failed fetch is logged and the provider
    // keeps serving its static baseline — this never delays startup or 5xxs a
    // request. No-op when OpenRouter is disabled (`TT_PROVIDERS`).
    state.spawn_background_refreshers();

    let app = tt_core::build_router(state);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind {bind} failed"))?;
    tracing::info!(addr = %bind, "gateway listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    // REL-3: drain detached telemetry writes (request_logs/body capture) so a
    // rolling deploy / SIGTERM doesn't abandon billing rows. Bounded so a stuck
    // write can't hang shutdown.
    telemetry_tracker.close();
    match tokio::time::timeout(std::time::Duration::from_secs(30), telemetry_tracker.wait()).await {
        Ok(()) => tracing::info!("telemetry writes drained on shutdown"),
        Err(_) => tracing::error!("telemetry drain timed out; some writes may be lost"),
    }

    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("shutdown: SIGINT"),
        _ = terminate => tracing::info!("shutdown: SIGTERM"),
    }
}

#[cfg(test)]
mod gateway_fail_closed_tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    const ANY: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
    const PUBLIC: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7));

    // -- startup-config matrix ------------------------------------------------

    #[test]
    fn with_key_store_default_bind_is_unspecified_unchanged() {
        // DB-backed deployments keep today's 0.0.0.0 default.
        assert_eq!(resolve_gateway_bind(None, true, false).unwrap(), ANY);
    }

    #[test]
    fn with_key_store_explicit_non_loopback_allowed_without_opt_in() {
        assert_eq!(resolve_gateway_bind(Some(ANY), true, false).unwrap(), ANY);
        assert_eq!(
            resolve_gateway_bind(Some(PUBLIC), true, false).unwrap(),
            PUBLIC
        );
    }

    #[test]
    fn no_store_default_bind_falls_back_to_loopback() {
        // Dev mode (`tt gateway` with no DATABASE_URL) keeps working — on
        // loopback, where the unauthenticated gateway can't be reached from
        // the network.
        assert_eq!(resolve_gateway_bind(None, false, false).unwrap(), LOOPBACK);
    }

    #[test]
    fn no_store_explicit_loopback_allowed() {
        assert_eq!(
            resolve_gateway_bind(Some(LOOPBACK), false, false).unwrap(),
            LOOPBACK
        );
        let v6_lo = IpAddr::V6(Ipv6Addr::LOCALHOST);
        assert_eq!(
            resolve_gateway_bind(Some(v6_lo), false, false).unwrap(),
            v6_lo
        );
    }

    #[test]
    fn no_store_non_loopback_refused_with_actionable_error() {
        for ip in [ANY, PUBLIC, IpAddr::V6(Ipv6Addr::UNSPECIFIED)] {
            let err = resolve_gateway_bind(Some(ip), false, false)
                .expect_err("non-loopback bind without a key store must be refused");
            let msg = format!("{err:#}");
            assert!(
                msg.contains(ALLOW_UNAUTHENTICATED_PUBLIC_BIND_VAR),
                "error must name the opt-in env var: {msg}"
            );
            assert!(
                msg.contains("DATABASE_URL"),
                "error must point at the key-store fix: {msg}"
            );
            assert!(
                msg.contains("TT_BIND_ADDR"),
                "error must point at the loopback fix: {msg}"
            );
        }
    }

    #[test]
    fn no_store_non_loopback_allowed_with_explicit_opt_in() {
        assert_eq!(resolve_gateway_bind(Some(ANY), false, true).unwrap(), ANY);
        assert_eq!(
            resolve_gateway_bind(Some(PUBLIC), false, true).unwrap(),
            PUBLIC
        );
    }

    // -- credential-store wiring ------------------------------------------------

    /// Process-wide lock serializing the credential-wiring tests that mutate
    /// `TT_MASTER_KEY` / `OPENAI_API_KEY` so they can't race each other in the
    /// multi-threaded test runner. (Same pattern as the `routes::chat`
    /// credential tests in tt-core.)
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn no_db_pool_wires_no_credential_store() {
        // Without a key store nothing can verify callers, so the operator's
        // env provider keys (OPENAI_API_KEY, …) must be unreachable: no
        // credential store at all → the chat handler falls back to forwarding
        // the caller's own Bearer key upstream. The env-fallback opt-in makes
        // no difference without a DB pool — (c) no-DB dev mode is unchanged.
        assert!(build_credential_store(None, false).is_none());
        assert!(build_credential_store(None, true).is_none());
    }

    /// With a DB pool but a missing/bad `TT_MASTER_KEY`, the env-only dogfood
    /// store exists ONLY behind the explicit opt-in; by default the gateway
    /// fails closed with no credential store, so the operator's env keys are
    /// structurally unreachable.
    // The lock is held across awaits on purpose: only other test threads ever
    // contend it, so there is no deadlock risk (same pattern as tt-core's
    // credential tests).
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn bad_master_key_env_only_store_requires_opt_in() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        std::env::remove_var("TT_MASTER_KEY"); // Postgres store init fails
        std::env::set_var("OPENAI_API_KEY", "sk-operator-env");

        // Lazy pool: never connected — the env-only store ignores it and the
        // fail-closed branch returns before any query.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")
            .expect("lazy pool");

        // Default (no opt-in): NO store — env keys unreachable.
        let fail_closed = build_credential_store(Some(&pool), false);

        // Explicit opt-in: env-only store serves the operator's env key.
        let env_only = build_credential_store(Some(&pool), true).expect("env-only store");
        let got = env_only
            .get(uuid::Uuid::nil(), "openai")
            .await
            .expect("env get");

        // Clean up BEFORE asserting so a failed assert can't leak env state.
        std::env::remove_var("OPENAI_API_KEY");

        assert!(
            fail_closed.is_none(),
            "bad TT_MASTER_KEY without the opt-in must wire NO credential store"
        );
        let got = got.expect("opt-in env fallback must serve the env key");
        assert_eq!(got.api_key.expose(), "sk-operator-env");
    }

    /// BYO-only against a REAL Postgres: with a valid `TT_MASTER_KEY`,
    /// (a) the default wiring is Postgres-only — an org with no stored
    /// credential resolves NOTHING even though `OPENAI_API_KEY` is set in the
    /// process env (env never consulted); (b) the explicit opt-in chains the
    /// env fallback behind Postgres and serves it.
    ///
    /// Run with e.g.:
    /// `docker run -d --name tt-byo-pg -e POSTGRES_PASSWORD=tt -p 55432:5432 pgvector/pgvector:pg17`
    /// `TEST_DATABASE_URL=postgres://postgres:tt@localhost:55432/postgres cargo test -p tt-cli -- --include-ignored byo_only`
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL (Postgres; the test creates its own provider_credentials table) — run with --include-ignored"]
    async fn byo_only_db_wiring_env_fallback_gated_by_opt_in() {
        let url = std::env::var("TEST_DATABASE_URL").expect("TEST_DATABASE_URL");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect TEST_DATABASE_URL");
        // The provider_credentials schema ships from the cloud repo's
        // migrations, not this OSS migrator — create the minimal shape the
        // store queries here.
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS provider_credentials (
                 id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
                 org_id uuid NOT NULL,
                 provider text NOT NULL,
                 label text NOT NULL DEFAULT '',
                 secret_enc bytea NOT NULL,
                 base_url text,
                 extra_headers jsonb NOT NULL DEFAULT '[]'::jsonb,
                 created_at timestamptz NOT NULL DEFAULT now(),
                 rotated_at timestamptz,
                 UNIQUE (org_id, provider)
               )"#,
        )
        .execute(&pool)
        .await
        .expect("create provider_credentials");

        let _guard = ENV_LOCK.lock().expect("env lock");
        std::env::set_var("TT_MASTER_KEY", hex::encode([7u8; 32]));
        std::env::set_var("OPENAI_API_KEY", "sk-operator-env");
        let org = uuid::Uuid::now_v7(); // fresh org: no stored credential

        // (a) Default: Postgres-only. Store miss → None; env NOT consulted.
        let byo_only = build_credential_store(Some(&pool), false).expect("postgres-only store");
        let got_default = byo_only.get(org, "openai").await.expect("pg get");

        // (b) Opt-in: env fallback chained behind Postgres serves the env key.
        let chained = build_credential_store(Some(&pool), true).expect("chained store");
        let got_opt_in = chained.get(org, "openai").await.expect("chained get");

        std::env::remove_var("TT_MASTER_KEY");
        std::env::remove_var("OPENAI_API_KEY");

        assert!(
            got_default.is_none(),
            "BYO-only default must NOT serve the operator's env key, got {:?}",
            got_default.map(|c| c.api_key.expose().to_string())
        );
        let got_opt_in = got_opt_in.expect("opt-in env fallback must serve the env key");
        assert_eq!(got_opt_in.api_key.expose(), "sk-operator-env");
    }
}

// ---------------------------------------------------------------------------
// Output format detection
// ---------------------------------------------------------------------------

/// Whether to emit a markdown report, a JSON array, or a SARIF 2.1.0 log.
enum OutputFormat {
    Markdown,
    Json,
    /// SARIF 2.1.0 — only valid for `tt inspect` rule findings.
    Sarif,
}

/// Infer the desired output format from the destination path.
///
/// Used by `--cost-diff` (which only emits Markdown or JSON); a `.sarif` path
/// here falls through to Markdown since SARIF is not a cost-diff format.
fn output_format_for(output: Option<&str>) -> OutputFormat {
    match output {
        Some(p) if p.ends_with(".json") => OutputFormat::Json,
        _ => OutputFormat::Markdown,
    }
}

/// Decide, from the raw process args (before clap parsing), whether this is a
/// `tt inspect` invocation that prints a **machine-readable** report (`json` or
/// `sarif`) to **stdout** — i.e. with no `--output <file>` redirect.
///
/// Used to silence the startup tracing log line so it can't corrupt the
/// machine output. Conservative by construction: a false negative just leaves
/// the (harmless-for-humans) log on stdout; it never suppresses output.
///
/// `--cost-diff` / `--suggest-plan` are excluded — they don't honor `--format`
/// and never emit SARIF.
fn inspect_emits_machine_output_to_stdout<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
    // args[0] is the binary; the subcommand is the first non-flag token.
    let is_inspect = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .is_some_and(|sub| sub == "inspect");
    if !is_inspect {
        return false;
    }

    // These modes ignore --format and never emit SARIF.
    if args
        .iter()
        .any(|a| a == "--cost-diff" || a == "--suggest-plan")
    {
        return false;
    }

    // Pull --format and --output values (supporting both `--flag val` and
    // `--flag=val`). Last occurrence wins, mirroring clap.
    let value_of = |flag: &str| -> Option<String> {
        let mut found = None;
        let mut it = args.iter().peekable();
        while let Some(a) = it.next() {
            if let Some(v) = a.strip_prefix(&format!("{flag}=")) {
                found = Some(v.to_string());
            } else if a == flag {
                if let Some(v) = it.next() {
                    found = Some(v.clone());
                }
            }
        }
        found
    };

    let format = value_of("--format");
    let output = value_of("--output");

    // Report goes to stdout iff there is no real --output file (absent or "-").
    let to_stdout = matches!(output.as_deref(), None | Some("") | Some("-"));
    if !to_stdout {
        return false;
    }

    // Machine-readable iff --format is json/sarif (path inference is moot here
    // since the report is going to stdout, not a file).
    matches!(
        format.as_deref().map(str::to_lowercase).as_deref(),
        Some("json") | Some("sarif")
    )
}

/// Resolve the `tt inspect` output format from an explicit `--format` value
/// (when set) and otherwise from the `--output` path extension.
///
/// Precedence: an explicit `--format` always wins. Accepted `--format` values
/// (case-insensitive): `md`/`markdown`, `json`, `sarif`. An unrecognised value
/// is an error so a typo never silently degrades to markdown.
fn inspect_output_format(
    format: Option<&str>,
    output: Option<&str>,
) -> anyhow::Result<OutputFormat> {
    if let Some(fmt) = format {
        return match fmt.to_lowercase().as_str() {
            "md" | "markdown" => Ok(OutputFormat::Markdown),
            "json" => Ok(OutputFormat::Json),
            "sarif" => Ok(OutputFormat::Sarif),
            other => anyhow::bail!("unknown --format {other:?} (expected one of: md, json, sarif)"),
        };
    }
    Ok(match output {
        Some(p) if p.ends_with(".json") => OutputFormat::Json,
        Some(p) if p.ends_with(".sarif") => OutputFormat::Sarif,
        _ => OutputFormat::Markdown,
    })
}

// ---------------------------------------------------------------------------
// `tt inspect` implementation
// ---------------------------------------------------------------------------

/// Run the inspect engine against `path`, format the results, and either write
/// them to `output` or print to stdout.  Exits non-zero via [`anyhow::bail!`]
/// when any finding meets or exceeds `fail_on`.
fn run_inspect(
    path: &str,
    fail_on: &str,
    output: Option<&str>,
    format: Option<&str>,
) -> anyhow::Result<()> {
    use tt_inspect_core::Severity;

    let fail_on_sev = Severity::from_str_ci(fail_on).unwrap_or(Severity::High);

    let mut engine = tt_inspect_core::Engine::new();
    // Register all 10 P0 production rules.
    for rule in tt_inspect_rules_tier1::all_rules() {
        engine.add_rule(rule);
    }

    let findings = engine.scan(std::path::Path::new(path));

    let formatted = match inspect_output_format(format, output)? {
        OutputFormat::Json => tt_inspect_core::output::format_json(&findings),
        OutputFormat::Sarif => tt_inspect_core::output::format_sarif(&findings),
        OutputFormat::Markdown => tt_inspect_core::output::format_markdown(&findings),
    };

    match output {
        Some(p) if !p.is_empty() && p != "-" => {
            std::fs::write(p, &formatted)
                .map_err(|e| anyhow::anyhow!("failed to write output to {p}: {e}"))?;
            tt_cli::ui::note(&format!("wrote {} finding(s) to {p}", findings.len()));
        }
        _ => {
            print!("{formatted}");
        }
    }

    // Colored severity summary on stderr (the stdout report body stays plain).
    let count = |s: Severity| findings.iter().filter(|f| f.severity == s).count();
    let (c, h, m, l) = (
        count(Severity::Critical),
        count(Severity::High),
        count(Severity::Medium),
        count(Severity::Low),
    );
    if findings.is_empty() {
        tt_cli::ui::ok("Clean — no findings");
    } else if c > 0 {
        tt_cli::ui::error(&format!(
            "{} finding(s) · {c} critical · {h} high · {m} medium · {l} low",
            findings.len()
        ));
    } else {
        tt_cli::ui::warn(&format!(
            "{} finding(s) · {h} high · {m} medium · {l} low",
            findings.len()
        ));
    }

    let above: Vec<_> = findings
        .iter()
        .filter(|f| f.severity.weight() >= fail_on_sev.weight())
        .collect();

    if !above.is_empty() {
        anyhow::bail!(
            "{} finding(s) at or above {:?} severity \
             (use --fail-on critical to disable gating)",
            above.len(),
            fail_on_sev,
        );
    }

    Ok(())
}

/// Run `tt inspect --suggest-plan`: scan `path` for LLM model strings, generate
/// preview route suggestions for each unique model found, and emit a skeleton
/// [`tt_plan_core::PlanInput`] JSON with `proposed_routes` pre-filled.
///
/// Users write the output to a file (via `--output`), fill in `org_id` /
/// `requests` / `pricing`, then run `tt plan --input <file>` to replay.
async fn run_suggest_plan(
    path: &str,
    output: Option<&str>,
    from_db: bool,
    org: Option<&str>,
    window_days: i64,
) -> anyhow::Result<()> {
    use anyhow::Context;

    let json = if from_db {
        let url = std::env::var("DATABASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .context(
                "--from-db requires DATABASE_URL (the gateway's Postgres connection string)",
            )?;
        let pool = tt_core::connect(&url, 4)
            .await
            .context("connect to DATABASE_URL")?;
        let until = chrono::Utc::now();
        // Clamp to [1, 100yr] so an absurd --window-days can't panic chrono's
        // TimeDelta on overflow, and a 0/negative window falls back to 1 day.
        let since = until - chrono::Duration::days(window_days.clamp(1, 36_500));
        let org_uuid = match org {
            Some(s) => Some(uuid::Uuid::parse_str(s).context("--org must be a UUID")?),
            None => None,
        };
        let (resolved_org, requests) =
            tt_cli::telemetry_window::fetch_window(&pool, org_uuid, since, until).await?;
        tt_cli::ui::note(&format!(
            "pulled {} request_logs rows for org {} ({}-day window)",
            requests.len(),
            resolved_org,
            window_days
        ));
        tt_cli::plan_suggest::build_plan_input_json_inner(
            path,
            resolved_org,
            &requests,
            since,
            until,
        )?
    } else {
        tt_cli::plan_suggest::build_plan_input_json(path)?
    };

    match output {
        Some(p) if !p.is_empty() && p != "-" => {
            std::fs::write(p, &json)
                .map_err(|e| anyhow::anyhow!("failed to write plan input to {p}: {e}"))?;
            let hint = if from_db {
                format!("wrote runnable plan-input to {p}  (then: tt plan --input {p})")
            } else {
                format!("wrote plan-input skeleton to {p}  (edit org_id + requests, then: tt plan --input {p})")
            };
            tt_cli::ui::note(&hint);
        }
        _ => {
            print!("{json}");
        }
    }

    Ok(())
}

/// Run `tt inspect --cost-diff`: estimate the projected per-call cost change of
/// LLM model identifiers added/removed between `base` and the working tree,
/// scoped to `path`. Output is markdown (default / `*.json` → JSON) suitable
/// for a GitHub check-run summary. With `fail_on_cost_increase`, exits non-zero
/// on a projected net increase so CI can gate.
fn run_cost_diff(
    path: &str,
    base: &str,
    output: Option<&str>,
    fail_on_cost_increase: bool,
) -> anyhow::Result<()> {
    use std::process::Command as ProcCommand;

    // `git diff <base> -- <path>` — the working tree vs the base ref. We feed
    // the unified-diff text to the pure analyzer, keeping git out of the core.
    let out = ProcCommand::new("git")
        .args(["diff", base, "--", path])
        .output()
        .context("failed to run `git diff` — is git installed and is this a repo?")?;
    if !out.status.success() {
        anyhow::bail!(
            "`git diff {base} -- {path}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let diff_text = String::from_utf8_lossy(&out.stdout);

    // PROD-8: honor a per-repo token profile (`.tokentrimmer/cost-profile.toml`)
    // so the projected per-call cost reflects this repo's typical prompt size.
    // The profile lives at the git repo root, not the (possibly nested) scope
    // `path`; resolve it via `git rev-parse --show-toplevel`, falling back to
    // the scope path / cwd. `load_from_repo` is infallible — a missing or bad
    // file falls back to the default standard profile, never breaking the gate.
    let repo_root = ProcCommand::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_string()))
        .unwrap_or_else(|| PathBuf::from(path));
    let profile = tt_cli::cost_diff::CostProfile::load_from_repo(&repo_root);

    let report = tt_cli::cost_diff::analyze_with_profile(&diff_text, &profile);

    let formatted = match output_format_for(output) {
        OutputFormat::Json => serde_json::to_string_pretty(&report)
            .unwrap_or_else(|e| format!("{{\"error\":\"serialize: {e}\"}}")),
        // SARIF is not a cost-diff format; `output_format_for` never yields it,
        // so cost-diff only ever produces markdown or JSON.
        OutputFormat::Markdown | OutputFormat::Sarif => {
            tt_cli::cost_diff::format_markdown_with_profile(&report, &profile)
        }
    };

    match output {
        Some(p) if !p.is_empty() && p != "-" => {
            std::fs::write(p, &formatted)
                .map_err(|e| anyhow::anyhow!("failed to write output to {p}: {e}"))?;
            tt_cli::ui::note(&format!("wrote cost-diff report to {p}"));
        }
        _ => print!("{formatted}"),
    }

    if report.is_increase() {
        tt_cli::ui::warn(&format!(
            "Net +${:.6} per call projected",
            report.net_projected_usd
        ));
    } else if report.net_projected_usd < 0.0 {
        tt_cli::ui::ok(&format!(
            "Net −${:.6} per call projected",
            report.net_projected_usd.abs()
        ));
    } else {
        tt_cli::ui::note("No net per-call cost change projected.");
    }

    if fail_on_cost_increase && report.is_increase() {
        anyhow::bail!(
            "projected per-call cost increase of +${:.6} \
             (use without --fail-on-cost-increase to report only)",
            report.net_projected_usd
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// `tt audit verify` implementation
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// `tt plan` — replay historical telemetry against a proposed config
// ---------------------------------------------------------------------------

/// Implement `tt plan`.
///
/// v1 reads a serialized [`tt_plan_core::PlanInput`] from a JSON file at
/// `--input`. The JSON-file interface is the universal offline path for CI
/// gates and developer experiments.
///
/// On `--apply`, writes the projected routes to the gateway's Postgres and
/// records a signed `plan.applied` audit entry; requires DATABASE_URL.
async fn run_plan(
    input: Option<&str>,
    output: Option<&str>,
    example: bool,
    apply: bool,
    yes: bool,
) -> anyhow::Result<()> {
    use anyhow::Context;

    if example {
        print_plan_example();
        return Ok(());
    }
    let input_path = input.ok_or_else(|| {
        anyhow::anyhow!("usage: tt plan --input <plan_input.json>  (or --example)")
    })?;

    let raw = std::fs::read_to_string(input_path)
        .map_err(|e| anyhow::anyhow!("read {input_path}: {e}"))?;
    let plan_input: tt_plan_core::PlanInput =
        serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("parse {input_path}: {e}"))?;

    // Capture what the apply path needs before `plan_input` is consumed.
    let org_id = plan_input.org_id;
    let plan_id = plan_input.plan_id;
    let proposed = plan_input.proposed_routes.clone();

    let result =
        tt_plan_core::replay(plan_input).map_err(|e| anyhow::anyhow!("replay failed: {e}"))?;

    let payload = match output {
        Some(p) if p.ends_with(".json") => serde_json::to_string_pretty(&result)?,
        _ => format_plan_text(&result),
    };

    match output {
        Some(p) if p != "-" => {
            std::fs::write(p, &payload)?;
            tt_cli::ui::note(&format!("wrote plan result to {p}"));
        }
        _ => {
            print!("{payload}");
        }
    }

    let agg = &result.aggregates;
    if agg.projected_savings_usd > 0.0 {
        tt_cli::ui::ok(&format!(
            "Projected savings ${:.4} ({:.1}%) · {} of {} requests rerouted",
            agg.projected_savings_usd,
            agg.projected_savings_pct,
            agg.requests_rerouted,
            result.sample_size
        ));
    } else {
        tt_cli::ui::note("No projected savings for this config.");
    }

    if apply {
        let url = std::env::var("DATABASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .context(
                "tt plan --apply requires DATABASE_URL (the gateway's Postgres connection string)",
            )?;
        let pool = tt_core::connect(&url, 4)
            .await
            .context("connect to DATABASE_URL")?;
        let signing_key = tt_cli::local_audit::load_or_create_signing_key()?;
        let chain_path = std::path::Path::new(tt_cli::local_audit::DEFAULT_CHAIN_PATH);
        tt_cli::plan_apply::apply_routes(
            &pool,
            org_id,
            plan_id,
            &proposed,
            &result,
            yes,
            &signing_key,
            chain_path,
        )
        .await?;
    }

    Ok(())
}

/// Human-readable summary of a [`tt_plan_core::PlanResult`]. Mirrors the
/// shape of `docs/03-plan-replay-design.md` § "CLI output format".
fn format_plan_text(r: &tt_plan_core::PlanResult) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# TokenTrimmer Plan\n\nplan_id   : {}\norg_id    : {}\nwindow    : {} → {}\nsample    : {} requests\n\n",
        r.plan_id, r.org_id, r.window_start.to_rfc3339(), r.window_end.to_rfc3339(), r.sample_size
    ));
    let a = &r.aggregates;
    out.push_str("## Aggregates\n\n");
    out.push_str(&format!(
        "  baseline_cost    ${:.4}\n  projected_cost   ${:.4}\n  projected_savings ${:.4} ({:.1}%)\n",
        a.total_baseline_cost_usd, a.total_projected_cost_usd, a.projected_savings_usd, a.projected_savings_pct
    ));
    out.push_str(&format!(
        "  cache_hit_rate   {:.1}%\n  p50_latency      {:.0}ms\n  p95_latency      {:.0}ms\n",
        a.cache_hit_rate_projected * 100.0,
        a.p50_latency_ms_projected,
        a.p95_latency_ms_projected
    ));
    out.push_str(&format!(
        "  requests: {} rerouted, {} unchanged, {} unprice-able\n\n",
        a.requests_rerouted, a.requests_unchanged, a.requests_unprice_able
    ));

    let c = &r.confidence_intervals;
    out.push_str("## 95% confidence intervals\n\n");
    out.push_str(&format!(
        "  savings_usd     ${:.4} – ${:.4}\n  savings_pct     {:.1}% – {:.1}%\n  cache_hit_rate  {:.1}% – {:.1}%\n  p50_latency_ms  {:.0} – {:.0}\n  p95_latency_ms  {:.0} – {:.0}\n\n",
        c.savings_usd_95.0, c.savings_usd_95.1,
        c.savings_pct_95.0, c.savings_pct_95.1,
        c.cache_hit_rate_95.0 * 100.0, c.cache_hit_rate_95.1 * 100.0,
        c.p50_latency_ms_95.0, c.p50_latency_ms_95.1,
        c.p95_latency_ms_95.0, c.p95_latency_ms_95.1,
    ));

    if !a.l2_projections.is_empty() {
        out.push_str("## L2 semantic cache sweep\n\n");
        out.push_str("  threshold  hit_rate  hits/total  poisoning\n");
        for p in &a.l2_projections {
            out.push_str(&format!(
                "  {:>9.2}  {:>7.1}%  {}/{}  {}\n",
                p.threshold,
                p.projected_l2_hit_rate * 100.0,
                p.projected_l2_hits,
                p.total,
                p.poisoning_candidates
            ));
        }
        if a.l2_poisoning_candidates > 0 {
            out.push_str(&format!(
                "  ⚠ {} distinct cache-poisoning candidate(s) across the sweep (similar requests with divergent outcomes)\n",
                a.l2_poisoning_candidates
            ));
        }
        out.push('\n');
    }

    if !r.per_route_breakdown.is_empty() {
        out.push_str("## Per-route\n\n");
        for row in &r.per_route_breakdown {
            out.push_str(&format!(
                "  {} ({}): matched={} baseline=${:.4} projected=${:.4} saved=${:.4}\n",
                row.route_name,
                row.route_id,
                row.matched,
                row.baseline_cost_usd,
                row.projected_cost_usd,
                row.savings_usd
            ));
        }
        out.push('\n');
    }

    if !r.caveats.is_empty() {
        out.push_str("## Caveats\n\n");
        for c in &r.caveats {
            out.push_str(&format!("  - {c}\n"));
        }
    }

    out
}

/// Print a minimal example PlanInput to stdout. Users redirect to a file
/// and edit. Avoids the chicken-and-egg of "I want to try `tt plan` but
/// don't know the JSON shape".
fn plan_example_json() -> serde_json::Value {
    serde_json::json!({
        "plan_id": "00000000-0000-0000-0000-000000000001",
        "org_id":  "00000000-0000-0000-0000-000000000002",
        "window_start": "2026-05-01T00:00:00Z",
        "window_end":   "2026-05-08T00:00:00Z",
        "requests": [
            {
                "id": "00000000-0000-0000-0000-000000000010",
                "org_id": "00000000-0000-0000-0000-000000000002",
                "ts": "2026-05-01T12:00:00Z",
                "provider": "openai",
                "model": "gpt-4o",
                "input_tokens": 1000,
                "output_tokens": 200,
                "cached_tokens": 0,
                "cost_usd": 0.0045,
                "baseline_cost_usd": 0.0045,
                "cached": false,
                "cache_layer": null,
                "matched_route_id": null,
                "latency_ms": 800,
                "upstream_latency_ms": 750,
                "status": 200,
                "tag": null
            }
        ],
        "proposed_routes": [
            {
                "id": "00000000-0000-0000-0000-000000000099",
                "name": "cheap-for-short",
                "priority": 100,
                "enabled": true,
                "when": { "model_in": ["gpt-4o"], "input_tokens_lt": 2000 },
                "then": { "target_model": "gpt-4o-mini" }
            }
        ],
        "pricing": {
            "openai:gpt-4o-mini": {
                "input_per_million": 0.15,
                "output_per_million": 0.60,
                "cached_input_per_million": 0.075
            }
        },
        "config": {
            "l1_ttl_seconds": null,
            "l2_threshold_sweep": [0.85, 0.90, 0.92, 0.95],
            "l2_ttl_seconds": null
        },
        "seed": 42,
        "bootstrap_iterations": 1000
    })
}

fn print_plan_example() {
    println!(
        "{}",
        serde_json::to_string_pretty(&plan_example_json()).unwrap()
    );
}

#[cfg(test)]
mod sentry_scrub_tests {
    use super::*;
    use sentry::protocol::{Event, Exception, Frame, Request, Stacktrace};

    fn frame_with_var(var_name: &str, value: &str) -> Frame {
        let mut f = Frame::default();
        f.vars
            .insert(var_name.into(), serde_json::Value::String(value.into()));
        f
    }

    #[test]
    fn scrubs_request_headers_and_cookies() {
        let mut req = Request::default();
        req.headers
            .insert("authorization".into(), "Bearer tt_live_abc".into());
        req.headers
            .insert("Content-Type".into(), "application/json".into());
        req.cookies = Some("session=xyz".into());
        req.query_string = Some("api_key=foo".into());
        let event = Event {
            request: Some(req),
            ..Default::default()
        };
        let out = scrub_sensitive_event(event).unwrap();
        let r = out.request.unwrap();
        assert_eq!(r.headers.get("authorization").unwrap(), SCRUB_PLACEHOLDER);
        assert_eq!(r.headers.get("Content-Type").unwrap(), "application/json");
        assert!(r.cookies.is_none());
        assert!(r.query_string.is_none());
    }

    #[test]
    fn scrubs_exception_frame_locals() {
        let frame = frame_with_var("api_key", "sk-secret");
        let other = frame_with_var("user_id", "abc");
        let ex = Exception {
            ty: "panic".into(),
            value: Some("boom".into()),
            stacktrace: Some(Stacktrace {
                frames: vec![frame, other],
                ..Default::default()
            }),
            ..Default::default()
        };
        let event = Event {
            exception: vec![ex].into(),
            ..Default::default()
        };
        let out = scrub_sensitive_event(event).unwrap();
        let frames = &out.exception.values[0].stacktrace.as_ref().unwrap().frames;
        assert_eq!(
            frames[0].vars.get("api_key").unwrap(),
            &serde_json::Value::String(SCRUB_PLACEHOLDER.into())
        );
        assert_eq!(
            frames[1].vars.get("user_id").unwrap(),
            &serde_json::Value::String("abc".into())
        );
    }

    #[test]
    fn scrubs_extras_and_tags_by_name() {
        let mut event = Event::default();
        event.extra.insert(
            "TT_MASTER_KEY".into(),
            serde_json::Value::String("xyz".into()),
        );
        event
            .extra
            .insert("model".into(), serde_json::Value::String("gpt-4o".into()));
        event.tags.insert("bearer-prefix".into(), "tt_live".into());
        event.tags.insert("provider".into(), "openai".into());
        let out = scrub_sensitive_event(event).unwrap();
        assert_eq!(
            out.extra.get("TT_MASTER_KEY").unwrap(),
            &serde_json::Value::String(SCRUB_PLACEHOLDER.into())
        );
        assert_eq!(
            out.extra.get("model").unwrap(),
            &serde_json::Value::String("gpt-4o".into())
        );
        assert_eq!(out.tags.get("bearer-prefix").unwrap(), SCRUB_PLACEHOLDER);
        assert_eq!(out.tags.get("provider").unwrap(), "openai");
    }
}

#[cfg(test)]
mod plan_apply_tests {
    use super::*;
    use std::io::Write;

    fn example_input_file() -> tempfile::NamedTempFile {
        let mut f = tempfile::Builder::new()
            .suffix(".json")
            .tempfile()
            .expect("tempfile");
        f.write_all(
            serde_json::to_string(&plan_example_json())
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        f.flush().unwrap();
        f
    }

    /// `--apply` requires a Postgres connection string. With `DATABASE_URL`
    /// unset it must error (and mention `DATABASE_URL`) rather than silently
    /// treating the run as applied. The real apply path (with a live DB) is
    /// covered by the DB-integration tests on `tt_cli::plan_apply::apply_routes`.
    #[tokio::test]
    async fn apply_without_database_url_errors() {
        let input = example_input_file();
        let out = tempfile::Builder::new().suffix(".json").tempfile().unwrap();

        // Ensure DATABASE_URL is absent for the duration of this test, then
        // restore whatever was there before.
        let saved = std::env::var("DATABASE_URL").ok();
        std::env::remove_var("DATABASE_URL");

        let res = run_plan(
            input.path().to_str(),
            out.path().to_str(),
            false,
            true,  // --apply
            false, // --yes
        )
        .await;

        if let Some(v) = saved {
            std::env::set_var("DATABASE_URL", v);
        }

        let err = res.expect_err("--apply without DATABASE_URL must error");
        assert!(
            err.to_string().contains("DATABASE_URL"),
            "error should mention DATABASE_URL: {err}"
        );
    }

    /// The same projection WITHOUT `--apply` succeeds (exit 0) and touches no DB.
    #[tokio::test]
    async fn plan_without_apply_succeeds() {
        let input = example_input_file();
        let out = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        run_plan(
            input.path().to_str(),
            out.path().to_str(),
            false,
            false, // --apply
            false, // --yes
        )
        .await
        .expect("projection without --apply should succeed");
    }
}

#[cfg(test)]
mod inspect_format_tests {
    use super::{inspect_emits_machine_output_to_stdout, inspect_output_format, OutputFormat};

    fn argv(s: &str) -> Vec<String> {
        std::iter::once("tt".to_string())
            .chain(s.split_whitespace().map(str::to_string))
            .collect()
    }

    #[test]
    fn explicit_format_overrides_output_extension() {
        // --format wins over the .json path inference.
        assert!(matches!(
            inspect_output_format(Some("sarif"), Some("out.json")).unwrap(),
            OutputFormat::Sarif
        ));
        assert!(matches!(
            inspect_output_format(Some("md"), Some("out.json")).unwrap(),
            OutputFormat::Markdown
        ));
        assert!(matches!(
            inspect_output_format(Some("json"), None).unwrap(),
            OutputFormat::Json
        ));
    }

    #[test]
    fn output_extension_infers_format_when_no_explicit_flag() {
        assert!(matches!(
            inspect_output_format(None, Some("results.sarif")).unwrap(),
            OutputFormat::Sarif
        ));
        assert!(matches!(
            inspect_output_format(None, Some("findings.json")).unwrap(),
            OutputFormat::Json
        ));
        assert!(matches!(
            inspect_output_format(None, Some("report.md")).unwrap(),
            OutputFormat::Markdown
        ));
        assert!(matches!(
            inspect_output_format(None, None).unwrap(),
            OutputFormat::Markdown
        ));
    }

    #[test]
    fn unknown_format_is_an_error() {
        assert!(inspect_output_format(Some("xml"), None).is_err());
    }

    #[test]
    fn detects_machine_output_to_stdout() {
        // sarif/json to stdout (no --output) → silence logs.
        assert!(inspect_emits_machine_output_to_stdout(argv(
            "inspect . --format sarif"
        )));
        assert!(inspect_emits_machine_output_to_stdout(argv(
            "inspect . --format=json"
        )));
        assert!(inspect_emits_machine_output_to_stdout(argv(
            "inspect . --format sarif --output -"
        )));
    }

    #[test]
    fn ignores_when_not_inspect_or_not_machine_or_to_file() {
        // Not inspect.
        assert!(!inspect_emits_machine_output_to_stdout(argv(
            "plan --example"
        )));
        // Markdown / default → log line is harmless.
        assert!(!inspect_emits_machine_output_to_stdout(argv("inspect .")));
        assert!(!inspect_emits_machine_output_to_stdout(argv(
            "inspect . --format md"
        )));
        // Redirected to a real file → stdout is free for the log line.
        assert!(!inspect_emits_machine_output_to_stdout(argv(
            "inspect . --format sarif --output results.sarif"
        )));
        // cost-diff / suggest-plan never emit SARIF.
        assert!(!inspect_emits_machine_output_to_stdout(argv(
            "inspect . --cost-diff --format json"
        )));
    }
}
