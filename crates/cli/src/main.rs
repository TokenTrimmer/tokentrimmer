//! `tt` — TokenTrimmer CLI.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};

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
        /// Output destination. Omitted or "-" writes markdown to stdout.
        /// A path ending in ".json" writes JSON; any other path writes markdown.
        #[arg(long)]
        output: Option<String>,
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

        /// Apply the plan via the hosted backend (requires a tt_live_* key).
        /// Not yet wired: the projection is shown, then the command exits
        /// NON-ZERO so CI/automation can't assume the config was applied.
        #[arg(long, conflicts_with = "example")]
        apply: bool,
    },
    /// Audit log helpers.
    Audit {
        #[command(subcommand)]
        action: AuditAction,
    },
    /// Run the MCP server (stdio transport by default).
    Mcp {
        #[arg(long, default_value = "stdio")]
        transport: String,
        #[arg(long)]
        tt_api_key: Option<String>,
        #[arg(long)]
        tt_api_base: Option<String>,
        /// Port to bind when using --transport sse (default 31416).
        #[arg(long, default_value_t = 31416)]
        sse_port: u16,
    },
    /// Store a TokenTrimmer API key for this machine (browser login lands in V2).
    Login {
        /// The tt_live_/tt_test_ key. Use `-` to read it from stdin.
        #[arg(long)]
        token: Option<String>,
        /// Persist a gateway base URL alongside the key.
        #[arg(long)]
        base_url: Option<String>,
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
        #[arg(long, global = true)]
        tt_api_key: Option<String>,
        #[arg(long, global = true)]
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
    },
    /// RAG corpus management.
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
}

#[derive(Subcommand)]
enum RetrievalAction {
    /// Add a doc to a corpus (in-process; not yet persisted).
    DocAdd {
        corpus: String,
        path: String,
        #[arg(long, env = "OPENAI_API_KEY")]
        openai_key: String,
    },
    /// Ad-hoc search.
    Search {
        corpus: String,
        query: String,
        #[arg(long, default_value_t = 5)]
        k: usize,
        #[arg(long, env = "OPENAI_API_KEY")]
        openai_key: String,
    },
}

#[derive(Subcommand)]
enum RouteAction {
    /// List your routes.
    List,
    /// Show one route by id.
    Show { id: String },
    /// Delete one route by id.
    Rm { id: String },
    /// Add a route. Use --always <model>, or --from <m> --to <m>.
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
        /// Reject (402) any matched request whose estimated cost exceeds this (USD).
        #[arg(long)]
        max_cost: Option<f64>,
        /// Make matched requests skip TokenTrimmer's cache entirely (privacy).
        #[arg(long)]
        disable_cache: bool,
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
enum AuditAction {
    /// Verify the integrity of an audit log hash chain.
    ///
    /// Reads JSONL entries from `[PATH]` (default `.claude/AUDIT-CHAIN.jsonl`).
    /// When the first line is the tt-api export preamble
    /// `{"meta":true,"verifying_key":"<hex>",…}` the verifying key is
    /// extracted automatically. Otherwise pass `--key <hex-file>` with the
    /// hex-encoded Ed25519 verifying key (or `--key-hex <hex>` inline).
    Verify {
        /// Path to the JSONL chain. Defaults to `.claude/AUDIT-CHAIN.jsonl`.
        path: Option<String>,
        /// Filter to a specific org UUID (recorded but not yet enforced — all
        /// entries in the file are verified regardless).
        #[arg(long)]
        org: Option<String>,
        /// Path to a file containing the hex-encoded Ed25519 verifying key.
        /// Overrides the preamble key when both are present.
        #[arg(long)]
        key: Option<String>,
        /// Hex-encoded Ed25519 verifying key inline. Overrides `--key` and the
        /// preamble when present.
        #[arg(long)]
        key_hex: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

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
            cost_diff,
            base,
            fail_on_cost_increase,
            suggest_plan,
        } => {
            if cost_diff {
                run_cost_diff(&path, &base, output.as_deref(), fail_on_cost_increase)?;
            } else if suggest_plan {
                run_suggest_plan(&path, output.as_deref())?;
            } else {
                run_inspect(&path, &fail_on, output.as_deref())?;
            }
        }
        Command::Plan {
            input,
            output,
            example,
            apply,
        } => {
            run_plan(input.as_deref(), output.as_deref(), example, apply)?;
        }
        Command::Audit {
            action:
                AuditAction::Verify {
                    path,
                    org,
                    key,
                    key_hex,
                },
        } => {
            run_audit_verify(
                path.as_deref(),
                org.as_deref(),
                key.as_deref(),
                key_hex.as_deref(),
            )?;
        }
        Command::Mcp {
            transport,
            tt_api_key,
            tt_api_base,
            sse_port,
        } => {
            use tt_mcp::{
                auth,
                resources::{cost_ledger, inspect_baseline},
                tools::{find_route_for, inspect_diff, lookup_semantic_cache, preview_cost},
                Server,
            };
            let ctx = tt_cli::context::ResolvedContext::load(tt_api_key, tt_api_base)?;
            let api_key = auth::validate_api_key(ctx.api_key_string())?;
            let tt_api_base = ctx.base_url;
            let mut server = Server::new();
            server
                .tools
                .register(Box::new(preview_cost::PreviewCostTool));
            server
                .tools
                .register(Box::new(find_route_for::FindRouteForTool));
            server
                .tools
                .register(Box::new(inspect_diff::InspectDiffTool));
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
            match transport.as_str() {
                "stdio" => {
                    tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()?
                        .block_on(server.run_stdio())?;
                }
                "sse" => {
                    let addr: std::net::SocketAddr = format!("127.0.0.1:{sse_port}")
                        .parse()
                        .context("invalid SSE bind address")?;
                    tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()?
                        .block_on(server.run_sse(addr))?;
                }
                other => {
                    anyhow::bail!("unsupported MCP transport `{other}` (supported: stdio, sse)")
                }
            }
        }
        Command::Login { token, base_url } => {
            tt_cli::account::login_with_token(token, base_url)?;
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
            tt_api_key,
            tt_api_base,
        } => {
            tt_cli::chat::run(model, system, tt_api_key, tt_api_base).await?;
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
        } => {
            use tt_cli::init::{run, RunOptions};
            let root = path
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap());
            let opts = RunOptions {
                root,
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
        }
        Command::Retrieval { action } => {
            use tt_cli::retrieval as cli_retrieval;
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(async {
                    match action {
                        RetrievalAction::DocAdd {
                            corpus,
                            path,
                            openai_key,
                        } => {
                            cli_retrieval::add_doc(
                                &corpus,
                                std::path::Path::new(&path),
                                &openai_key,
                            )
                            .await
                        }
                        RetrievalAction::Search {
                            corpus,
                            query,
                            k,
                            openai_key,
                        } => cli_retrieval::search(&corpus, &query, k, &openai_key).await,
                    }
                })?;
        }
        Command::Route {
            action,
            tt_api_key,
            tt_api_base,
        } => {
            use tt_cli::route::{AddArgs, RouteCmd};
            let cmd = match action {
                RouteAction::List => RouteCmd::List,
                RouteAction::Show { id } => RouteCmd::Show(id),
                RouteAction::Rm { id } => RouteCmd::Rm(id),
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
                    max_cost,
                    disable_cache,
                    priority,
                    name,
                    fallback,
                    disabled,
                } => RouteCmd::Add(AddArgs {
                    always,
                    from,
                    to,
                    when_has_images,
                    when_has_audio,
                    when_tag,
                    when_prompt_contains,
                    when_cost_gt,
                    when_cost_lt,
                    max_cost,
                    disable_cache,
                    priority,
                    name,
                    fallback,
                    disabled,
                }),
            };
            if let Err(e) = tt_cli::route::run(cmd, tt_api_key, tt_api_base).await {
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
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("tokio runtime")?
                .block_on(run_listener(cfg))
                .context("tt proxy listener")?;
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

/// Boot the Gateway HTTP server.
///
/// Reads config from env (see [`tt_config::Config::from_env`]). Every external
/// dependency (DB, Redis) is best-effort at boot: a failure logs + continues
/// rather than crash-looping the process. Bind / serve are fatal.
async fn run_gateway(config: tt_config::Config) -> anyhow::Result<()> {
    let bind = format!("0.0.0.0:{}", config.port);

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
                    tracing::info!("L1 cache enabled");
                    Some(Arc::new(c))
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

    let mut state = tt_core::AppState::with_default_providers();
    if let Some(l1) = l1_cache {
        state = state.with_l1(l1, None);
    }

    // Provider credentials: chained store — Postgres primary (when DB +
    // `TT_MASTER_KEY` are configured), env-backed fallback (single-tenant
    // dogfooding from the operator's own `OPENAI_API_KEY` / `ANTHROPIC_API_KEY`
    // / etc. in Fly secrets). The chain means org-specific credentials win,
    // and orgs that haven't onboarded yet fall back to the operator's keys.
    let env_store = tt_auth::EnvProviderCredentialStore::new();
    let credential_store: Arc<dyn tt_auth::ProviderCredentialStore> = match db_pool.as_ref() {
        Some(pool) => {
            match tt_auth::postgres::PostgresProviderCredentialStore::from_env(pool.clone()) {
                Ok(pg) => {
                    tracing::info!("provider credentials: Postgres primary + env fallback");
                    Arc::new(tt_auth::ChainedProviderCredentialStore::new(pg, env_store))
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Postgres credential store unavailable (TT_MASTER_KEY missing / bad); env-only"
                    );
                    Arc::new(env_store)
                }
            }
        }
        None => {
            tracing::warn!("no DB pool; provider credentials are env-only");
            Arc::new(env_store)
        }
    };
    state = state.with_credential_store(credential_store);

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
                let embedder =
                    Arc::new(tt_cache::OpenAIEmbedder::new(openai, "text-embedding-3-small", creds));
                let l2 = Arc::new(tt_cache::PostgresL2Cache::new(pool.clone()));
                state = state.with_l2(l2, embedder, None);
                tracing::info!("L2 semantic cache enabled (pgvector + text-embedding-3-small)");
            }
            _ => tracing::warn!(
                "TT_L2_SEMANTIC_CACHE=1 but DATABASE_URL / TT_OPENAI_EMBED_KEY missing — L2 disabled"
            ),
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
        state = state.with_routing_store(Arc::new(tt_routing::CachingRoutingStore::new(backing)));
        tracing::info!("routing store: Postgres-backed (60s per-org cache)");
    } else if std::env::var("TT_DOGFOOD_GROQ_ROUTING").as_deref() == Ok("1") {
        // Dogfood mode: seed an in-memory route that redirects short flagship
        // model prompts to Groq's llama-3.1-8b-instant for internal testing.
        let backing = Arc::new(tt_routing::InMemoryRoutingStore::new());
        backing.set_routes(
            tt_core::DOGFOOD_ORG_ID,
            vec![tt_routing::Route {
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
                    target_model: "llama-3.1-8b-instant".into(),
                    fallbacks: Vec::new(),
                    force_cache_layer: None,
                    disable_cache: false,
                    max_cost_usd: None,
                },
            }],
        );
        let caching: Arc<dyn tt_routing::RoutingStore> = backing;
        state = state
            .with_routing_store(Arc::new(tt_routing::CachingRoutingStore::new(caching)))
            .with_dogfood_enabled();
        tracing::info!(
            "dogfood routing: short prompts on flagship models → llama-3.1-8b-instant (Groq)"
        );
    } else {
        tracing::warn!("no DB pool; routing disabled (chat requests pass through unrouted)");
    }

    let app = tt_core::build_router(state);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("bind {bind} failed"))?;
    tracing::info!(addr = %bind, "gateway listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

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

// ---------------------------------------------------------------------------
// Output format detection
// ---------------------------------------------------------------------------

/// Whether to emit a markdown report or a JSON array.
enum OutputFormat {
    Markdown,
    Json,
}

/// Infer the desired output format from the destination path.
fn output_format_for(output: Option<&str>) -> OutputFormat {
    match output {
        Some(p) if p.ends_with(".json") => OutputFormat::Json,
        _ => OutputFormat::Markdown,
    }
}

// ---------------------------------------------------------------------------
// `tt inspect` implementation
// ---------------------------------------------------------------------------

/// Run the inspect engine against `path`, format the results, and either write
/// them to `output` or print to stdout.  Exits non-zero via [`anyhow::bail!`]
/// when any finding meets or exceeds `fail_on`.
fn run_inspect(path: &str, fail_on: &str, output: Option<&str>) -> anyhow::Result<()> {
    use tt_inspect_core::Severity;

    let fail_on_sev = Severity::from_str_ci(fail_on).unwrap_or(Severity::High);

    let mut engine = tt_inspect_core::Engine::new();
    // Register all 10 P0 production rules.
    for rule in tt_inspect_rules_tier1::all_rules() {
        engine.add_rule(rule);
    }

    let findings = engine.scan(std::path::Path::new(path));

    let formatted = match output_format_for(output) {
        OutputFormat::Json => tt_inspect_core::output::format_json(&findings),
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
fn run_suggest_plan(path: &str, output: Option<&str>) -> anyhow::Result<()> {
    let json = tt_cli::plan_suggest::build_plan_input_json(path)?;

    match output {
        Some(p) if !p.is_empty() && p != "-" => {
            std::fs::write(p, &json)
                .map_err(|e| anyhow::anyhow!("failed to write plan input to {p}: {e}"))?;
            tt_cli::ui::note(&format!("wrote plan-input skeleton to {p}  (edit org_id + requests, then: tt plan --input {p})"));
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

    let report = tt_cli::cost_diff::analyze(&diff_text);

    let formatted = match output_format_for(output) {
        OutputFormat::Json => serde_json::to_string_pretty(&report)
            .unwrap_or_else(|e| format!("{{\"error\":\"serialize: {e}\"}}")),
        OutputFormat::Markdown => tt_cli::cost_diff::format_markdown(&report),
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
/// `--input`. Production wiring (read from Postgres given a window + diff
/// spec) lands when the hosted Plan endpoint ships; the JSON-file interface
/// stays as the universal offline path for CI gates and developer experiments.
fn run_plan(
    input: Option<&str>,
    output: Option<&str>,
    example: bool,
    apply: bool,
) -> anyhow::Result<()> {
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

    // `--apply` is not wired to a hosted backend yet. The projection above is
    // real, but NO config was applied. Exit non-zero so CI/automation using
    // `--apply` can't silently treat the run as "applied" (rv-plan-apply-cli-honest, §1.8).
    if apply {
        anyhow::bail!(
            "tt plan --apply is not wired yet (needs the hosted backend + a tt_live_* key); \
             the projection above did NOT change any routing config. Apply via the dashboard \
             once it ships. Exiting non-zero so automation does not assume the plan was applied."
        );
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

/// Implement `tt audit verify`.
///
/// Loads JSONL entries from `path` (default `.claude/AUDIT-CHAIN.jsonl`).
/// When the first line is the tt-api export preamble
/// (`{"meta":true,"verifying_key":"<hex>",…}`), the verifying key is
/// extracted automatically. Override sources, in priority order:
///
/// 1. `--key-hex <hex>` (inline)
/// 2. `--key <path>` (file containing hex)
/// 3. preamble line
fn run_audit_verify(
    path: Option<&str>,
    org: Option<&str>,
    key_path: Option<&str>,
    key_hex_inline: Option<&str>,
) -> anyhow::Result<()> {
    let chain_path_str = path.unwrap_or(".claude/AUDIT-CHAIN.jsonl");
    let chain_path = Path::new(chain_path_str);
    if !chain_path.exists() {
        tt_cli::ui::note(&format!("no chain to verify ({chain_path_str} not found)"));
        if let Some(o) = org {
            tt_cli::ui::note(&format!(
                "(org filter --org={o} noted; no entries to filter)"
            ));
        }
        return Ok(());
    }

    let content = std::fs::read_to_string(chain_path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", chain_path.display()))?;

    let parsed = parse_chain_jsonl(&content)?;

    let key_hex = if let Some(h) = key_hex_inline {
        h.trim().to_string()
    } else if let Some(p) = key_path {
        std::fs::read_to_string(p)
            .map_err(|e| anyhow::anyhow!("failed to read key file {p}: {e}"))?
            .trim()
            .to_string()
    } else if let Some(h) = parsed.preamble_verifying_key {
        tt_cli::ui::note("verifying-key sourced from export preamble");
        h
    } else {
        anyhow::bail!(
            "no verifying key found: pass --key <path>, --key-hex <hex>, or use an \
             export with a preamble line"
        );
    };

    let key_bytes =
        hex::decode(key_hex.trim()).map_err(|e| anyhow::anyhow!("key hex decode failed: {e}"))?;
    let key_array: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("verifying key must be exactly 32 bytes (64 hex chars)"))?;
    let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(&key_array)
        .map_err(|e| anyhow::anyhow!("invalid Ed25519 verifying key: {e}"))?;

    tt_cli::ui::note(&format!("loaded {} entries", parsed.entries.len()));

    if let Some(o) = org {
        tt_cli::ui::note(&format!(
            "(--org={o} noted; filtering is deferred — verifies full chain)"
        ));
    }

    match tt_telemetry::audit::verify_chain(&parsed.entries, &verifying_key) {
        Ok(()) => {
            tt_cli::ui::ok(&format!(
                "chain OK — all {} entries verified",
                parsed.entries.len()
            ));
        }
        Err(e) => {
            anyhow::bail!("chain verification FAILED: {e}");
        }
    }

    Ok(())
}

/// Result of parsing a JSONL chain file. The preamble line (if present) is
/// stripped out — only real audit entries land in `entries`.
struct ParsedChain {
    entries: Vec<tt_telemetry::audit::AuditEntry>,
    preamble_verifying_key: Option<String>,
}

fn parse_chain_jsonl(content: &str) -> anyhow::Result<ParsedChain> {
    let mut preamble_verifying_key: Option<String> = None;
    let mut entries: Vec<tt_telemetry::audit::AuditEntry> = Vec::new();

    for (i, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Try the preamble shape first when we're on the first non-empty
        // line. The preamble carries `"meta": true` so it never collides
        // with a real `AuditEntry`.
        if entries.is_empty() && preamble_verifying_key.is_none() {
            let v: serde_json::Value = serde_json::from_str(trimmed)
                .map_err(|e| anyhow::anyhow!("failed to parse line {} as JSON: {e}", i + 1))?;
            if v.get("meta").and_then(|m| m.as_bool()) == Some(true) {
                preamble_verifying_key = v
                    .get("verifying_key")
                    .and_then(|k| k.as_str())
                    .map(String::from);
                continue;
            }
            // Fall through — not a preamble, parse as entry.
            let entry: tt_telemetry::audit::AuditEntry = serde_json::from_value(v)
                .map_err(|e| anyhow::anyhow!("failed to parse line {} as entry: {e}", i + 1))?;
            entries.push(entry);
            continue;
        }
        let entry: tt_telemetry::audit::AuditEntry = serde_json::from_str(trimmed)
            .map_err(|e| anyhow::anyhow!("failed to parse line {} as entry: {e}", i + 1))?;
        entries.push(entry);
    }

    Ok(ParsedChain {
        entries,
        preamble_verifying_key,
    })
}

#[cfg(test)]
mod audit_verify_tests {
    use super::*;

    #[test]
    fn parses_preamble_line() {
        let content = r#"{"meta":true,"verifying_key":"aa","entry_count":0}"#;
        let parsed = parse_chain_jsonl(content).unwrap();
        assert_eq!(parsed.preamble_verifying_key.as_deref(), Some("aa"));
        assert!(parsed.entries.is_empty());
    }

    #[test]
    fn handles_chain_without_preamble() {
        let entry = serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "org_id": "00000000-0000-0000-0000-000000000002",
            "timestamp": "2026-05-27T00:00:00Z",
            "actor": {"type": "system"},
            "event": "x",
            "payload": {},
            "prev_hash": "0".repeat(64),
            "hash": "f".repeat(64),
            "signature": "a".repeat(128),
        });
        let content = serde_json::to_string(&entry).unwrap();
        let parsed = parse_chain_jsonl(&content).unwrap();
        assert!(parsed.preamble_verifying_key.is_none());
        assert_eq!(parsed.entries.len(), 1);
    }

    #[test]
    fn ignores_blank_lines() {
        let content = "\n\n";
        let parsed = parse_chain_jsonl(content).unwrap();
        assert!(parsed.preamble_verifying_key.is_none());
        assert!(parsed.entries.is_empty());
    }

    #[test]
    fn preamble_then_entries() {
        let entry = serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "org_id": "00000000-0000-0000-0000-000000000002",
            "timestamp": "2026-05-27T00:00:00Z",
            "actor": {"type": "system"},
            "event": "x",
            "payload": {},
            "prev_hash": "0".repeat(64),
            "hash": "f".repeat(64),
            "signature": "a".repeat(128),
        });
        let content = format!(
            r#"{{"meta":true,"verifying_key":"deadbeef"}}{}{}"#,
            "\n",
            serde_json::to_string(&entry).unwrap()
        );
        let parsed = parse_chain_jsonl(&content).unwrap();
        assert_eq!(parsed.preamble_verifying_key.as_deref(), Some("deadbeef"));
        assert_eq!(parsed.entries.len(), 1);
    }
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

    /// `--apply` must exit non-zero until the hosted backend is wired, so CI /
    /// automation can't assume a no-op run actually applied the config.
    #[test]
    fn apply_exits_nonzero_until_wired() {
        let input = example_input_file();
        let out = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        let res = run_plan(
            input.path().to_str(),
            out.path().to_str(),
            false,
            true, // --apply
        );
        let err = res.expect_err("--apply must error until the hosted backend is wired");
        assert!(
            err.to_string().contains("apply"),
            "error should mention apply: {err}"
        );
    }

    /// The same projection WITHOUT `--apply` succeeds (exit 0).
    #[test]
    fn plan_without_apply_succeeds() {
        let input = example_input_file();
        let out = tempfile::Builder::new().suffix(".json").tempfile().unwrap();
        run_plan(input.path().to_str(), out.path().to_str(), false, false)
            .expect("projection without --apply should succeed");
    }
}
