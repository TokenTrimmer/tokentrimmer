//! `tt harness` — High-efficiency, cost-governed coding agent harness.
//!
//! Bridges TokenTrimmer's local sandboxed execution broker (`execution_broker`),
//! tree-sitter context ranking (`tt_context`), turn compactor, AST inspect rules,
//! and server-side agent loop with optional Fusion multi-model synthesis.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::agent_policy::{load_repository_policy, resolve_agent_policy, OrganizationPolicyMode};
use crate::context::ResolvedContext;
use crate::execution_broker::LocalExecutionBroker;
use crate::ui;
use anyhow::Context as _;

/// Options for launching the coding agent harness.
#[derive(Debug, Clone)]
pub struct HarnessOpts {
    pub prompt: String,
    pub repository: PathBuf,
    pub model: Option<String>,
    pub max_turns: Option<u32>,
    pub max_cost: Option<f64>,
    pub tag: Option<String>,
    pub flag_key: Option<String>,
    pub flag_base: Option<String>,
    /// Token budget for the ranked repository context pack
    /// (ranked files are inlined until this budget is reached).
    pub context_tokens: u32,
    // Fusion multi-model synthesis options
    pub fusion: bool,
    pub strategy: Option<String>,
    pub members: Vec<String>,
    pub arbiter: Option<String>,
    pub quorum: Option<usize>,
}

const DEFAULT_HARNESS_MODEL: &str = "claude-sonnet-5";
const DEFAULT_FUSION_STRATEGY: &str = "synthesize";
/// Default token budget for the ranked repository context pack when the caller
/// does not override it via `--context-tokens`. Exported so `main.rs` can reuse
/// it for the CLI default without duplicating the literal.
pub const DEFAULT_CONTEXT_TOKENS: u32 = 4000;

/// Run the coding agent harness against a target repository.
pub async fn run_harness(opts: HarnessOpts) -> anyhow::Result<()> {
    let repo_canonical = std::fs::canonicalize(&opts.repository).with_context(|| {
        format!(
            "failed to open repository at '{}'",
            opts.repository.display()
        )
    })?;

    let ctx = ResolvedContext::load(opts.flag_key.clone(), opts.flag_base.clone())?;
    let key = ctx
        .api_key_string()
        .context("no API key found — run `tt login` or set TT_API_KEY")?;
    let base = ctx.base_url.trim_end_matches('/').to_string();
    let client = tt_client::Client::new(base, key);

    ui::heading(&format!(
        "TokenTrimmer Harness {} {}",
        ui::BULLET,
        repo_canonical
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    ));

    let repo_policy = load_repository_policy(&repo_canonical)?;
    let resolved_policy = resolve_agent_policy(
        OrganizationPolicyMode::NotConfigured,
        &repo_policy,
        None,
        None,
    )?;

    // 1. Preload & rank repository context via tt-context
    let _ctx_spin = ui::spinner("analyzing codebase & indexing symbols…");
    let pack = tt_context::repo_context(
        &repo_canonical,
        &opts.prompt,
        10,
        opts.context_tokens.max(1),
    );
    drop(_ctx_spin);

    ui::note(&format!(
        "context pack: {} files ranked ({} inlined, ~{} tokens)",
        pack.files.len(),
        pack.files.iter().filter(|f| f.content.is_some()).count(),
        pack.token_estimate
    ));

    // 2. Build local execution sandbox with verified policy
    let run_id = format!("harness_{}", uuid::Uuid::new_v4());
    let broker = Arc::new(LocalExecutionBroker::new(
        &repo_canonical,
        run_id,
        &resolved_policy,
    )?);

    let mut tools = LocalExecutionBroker::tool_definitions(&resolved_policy);
    // Add gateway inspection and context tools
    tools.extend(crate::agent::gateway_tool_defs());

    let primary_model = opts
        .model
        .unwrap_or_else(|| DEFAULT_HARNESS_MODEL.to_string());
    let mut agent = client.agent().model(&primary_model).interactive();

    // Attach system instruction
    let system_prompt = format!(
        "You are an expert autonomous coding agent inside the TokenTrimmer harness.\n\
        Repository: {}\n\
        Follow policy constraints. Test and verify all edits thoroughly before completing.\n\
        Always check for regressions and ensure no broken syntax.",
        repo_canonical.display()
    );
    agent = agent.message(tt_client::system(system_prompt));

    // User prompt enriched with initial ranked symbol outline
    let mut initial_user_msg = format!(
        "Task:\n{}\n\nRelevant Repository Symbols & Context:\n",
        opts.prompt
    );
    for file in &pack.files {
        initial_user_msg.push_str(&format!("- {}: {}\n", file.path.display(), file.summary));
    }
    agent = agent.message(tt_client::user(initial_user_msg));
    agent = agent.tools(tools);

    if let Some(mt) = opts.max_turns {
        agent = agent.max_turns(mt);
    }
    if let Some(mc) = opts.max_cost {
        agent = agent.max_cost_usd(mc);
    }
    if let Some(tag) = opts.tag {
        agent = agent.tag(tag);
    }

    // Configure Fusion if requested
    if opts.fusion {
        let strategy = opts
            .strategy
            .unwrap_or_else(|| DEFAULT_FUSION_STRATEGY.to_string());
        let default_members = vec!["claude-sonnet-5".to_string(), "gpt-5.4".to_string()];
        let members: Vec<&str> = if opts.members.is_empty() {
            default_members.iter().map(|s| s.as_str()).collect()
        } else {
            opts.members.iter().map(|s| s.as_str()).collect()
        };
        let arbiter_ref = opts.arbiter.as_deref().unwrap_or(primary_model.as_str());

        ui::note(&format!(
            "Fusion synthesis active: strategy='{}', members=[{}], arbiter='{}'",
            strategy,
            members.join(", "),
            arbiter_ref
        ));

        agent = agent.fusion(&strategy, &members, Some(arbiter_ref), opts.quorum);
    }

    let started = Instant::now();
    ui::heading("Executing Agent Harness Run");

    let outcome = agent
        .run(&*broker)
        .await
        .map_err(|e| anyhow::anyhow!("agent run failed: {e}"))?;

    let elapsed = started.elapsed();
    crate::agent::print_outcome(&outcome);

    // 3. Inspect staged patch evidence
    let evidence = broker.evidence().await?;
    let changed_files = evidence.patch.changes.len();
    let total_diff_bytes = evidence.patch.diff_bytes;

    if changed_files > 0 {
        ui::ok(&format!(
            "Staged workspace changes: {} file(s) modified, ~{} diff bytes (elapsed {:.2}s)",
            changed_files,
            total_diff_bytes,
            elapsed.as_secs_f64()
        ));
        for file in &evidence.patch.changes {
            ui::note(&format!("  * {} ({:?})", file.path, file.kind));
        }
    } else {
        ui::note(&format!(
            "No repository files were modified (elapsed {:.2}s)",
            elapsed.as_secs_f64()
        ));
    }

    Ok(())
}
