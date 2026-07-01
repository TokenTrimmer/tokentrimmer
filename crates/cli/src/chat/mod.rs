//! `tt chat` — interactive chat REPL routed through the TokenTrimmer gateway,
//! surfacing per-turn cost + savings from the gateway's streaming usage event.

use anyhow::Context as _;
use serde::Deserialize;

use tt_shared::messages::{Message, MessageContent};

use crate::context::ResolvedContext;
use crate::ui;

pub mod budget;
pub mod command;
pub mod compact;
pub mod session;
pub mod shape;
pub mod tools;

pub use command::Command;
use command::{osc52_copy, print_help, wrap_osc52_for_mux, ToolsArg, OSC52_MAX_BYTES};

const DEFAULT_CHAT_MODEL: &str = "gpt-4o-mini";

/// Cost/usage payload from the gateway's terminal `tokentrimmer.usage` SSE event.
#[derive(Debug, Clone, Deserialize)]
pub struct UsageInfo {
    pub cost_usd: f64,
    pub baseline_cost_usd: f64,
    pub saved_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_tokens: u64,
}

impl From<tt_client::StreamUsage> for UsageInfo {
    fn from(u: tt_client::StreamUsage) -> Self {
        Self {
            cost_usd: u.cost_usd,
            baseline_cost_usd: u.baseline_cost_usd,
            saved_usd: u.saved_usd,
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cached_tokens: u.cached_tokens,
        }
    }
}

/// Muted per-turn footer. `saved …%` only when there is a positive saving.
#[must_use]
pub fn format_turn_footer(
    model: &str,
    in_tok: u64,
    out_tok: u64,
    cost_usd: f64,
    saved_usd: f64,
    baseline_usd: f64,
) -> String {
    let mut s = format!(
        "{} {} · {} tok · ${:.4}",
        ui::BULLET,
        model,
        in_tok + out_tok,
        cost_usd
    );
    if baseline_usd > 0.0 && saved_usd > 0.0 {
        let pct = (saved_usd / baseline_usd * 100.0).round();
        s.push_str(&format!(" · saved {pct:.0}%"));
    }
    ui::muted().apply_to(s).to_string()
}

/// Muted per-turn footer for a `--server-loop` turn. Deliberately carries NO
/// `saved …%` segment: the agent-run endpoint reports only gateway-attributed
/// served cost (no baseline), so the footer states the cost and the number of
/// server-side turns rather than a savings claim it can't ground.
#[must_use]
pub fn format_agent_footer(
    model: &str,
    in_tok: u64,
    out_tok: u64,
    cost_usd: f64,
    server_turns: u32,
) -> String {
    ui::muted()
        .apply_to(format!(
            "{} {} · {} tok · ${:.4} · server loop ({} turn(s), gateway-attributed)",
            ui::BULLET,
            model,
            in_tok + out_tok,
            cost_usd,
            server_turns
        ))
        .to_string()
}

/// In-memory conversation state (also the on-disk session format).
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Conversation {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    /// Frozen summary of turns folded away by `chat::compact`, carried on the
    /// wire as a second `System` message right after the system prompt (the
    /// STABLE-PREFIX position — see the invariants in `chat::budget`).
    /// Optional + serde-default: old session files load as `None`, and old
    /// CLI binaries reading new files ignore the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<compact::FrozenSummary>,
}

impl Conversation {
    #[must_use]
    pub fn new(model: String, system: Option<String>) -> Self {
        Self {
            model,
            system,
            messages: Vec::new(),
            summary: None,
        }
    }
    pub fn push_user(&mut self, text: String) {
        self.messages.push(Message::User {
            content: MessageContent::Text(text),
            name: None,
        });
    }
    pub fn push_assistant(&mut self, text: String) {
        self.messages.push(Message::Assistant {
            content: Some(MessageContent::Text(text)),
            tool_calls: Vec::new(),
            name: None,
        });
    }
    pub fn clear(&mut self) {
        self.messages.clear();
        self.summary = None;
    }
    /// The full message list to send, ordered STABLE → VOLATILE:
    /// `[system?, frozen summary?, …messages]`. On Anthropic both `System`
    /// messages land in the cached `system_blocks` prefix; on OpenAI they
    /// extend the auto-cached stable prefix (see `chat::budget` docs).
    #[must_use]
    pub fn wire_messages(&self) -> Vec<Message> {
        let mut v = Vec::new();
        if let Some(s) = &self.system {
            v.push(Message::System {
                content: MessageContent::Text(s.clone()),
            });
        }
        if let Some(sum) = &self.summary {
            v.push(Message::System {
                content: MessageContent::Text(sum.wire_block().to_string()),
            });
        }
        v.extend(self.messages.iter().cloned());
        v
    }
}

/// Running cost/savings totals for the current chat session.
#[derive(Default)]
pub struct Ledger {
    pub turns: u32,
    pub cost_usd: f64,
    pub saved_usd: f64,
    pub baseline_usd: f64,
    /// Estimated tokens removed from history by `chat::shape` tool-result/arg
    /// trimming. Token counts only — never converted into a USD savings claim
    /// (the gateway attributes no spend to these bytes).
    pub tool_trim_tokens: u64,
    /// REAL SPEND on compaction summary calls (also included in `cost_usd`).
    pub compaction_spend_usd: f64,
    /// Number of metered compaction summary calls.
    pub compaction_calls: u32,
    /// Estimated tokens no longer re-sent per future turn thanks to
    /// compaction (Σ dropped − summary). An ESTIMATE — always labeled
    /// `est./unbooked` and never merged into `saved_usd`.
    pub compaction_est_tok_per_turn: u64,
}

impl Ledger {
    pub fn add(&mut self, u: &UsageInfo) {
        self.turns += 1;
        self.cost_usd += u.cost_usd;
        self.saved_usd += u.saved_usd;
        self.baseline_usd += u.baseline_cost_usd;
    }
    /// Meter one compaction summary call. HONESTY GUARD: this is real money,
    /// so it raises the headline `cost_usd` (plus the dedicated compaction
    /// line) — and it NEVER touches `turns`, `saved_usd` or `baseline_usd`:
    /// summarization "saves" only estimated future re-sends, which are not
    /// gateway-attributed savings and must never inflate the saved figures.
    pub fn add_compaction(&mut self, cost_usd: f64) {
        self.cost_usd += cost_usd;
        self.compaction_spend_usd += cost_usd;
        self.compaction_calls += 1;
    }
    /// Book one server-side agent-loop turn (`--server-loop`). HONESTY GUARD:
    /// `POST /v1/agent/runs` returns gateway-attributed served cost but NO
    /// baseline/saved figure, so this raises `turns` + `cost_usd` only and never
    /// touches `saved_usd`/`baseline_usd` — the server loop must not fabricate a
    /// savings % it cannot attribute. (The gateway's down-routing does save; we
    /// simply have no attributed baseline to divide against, so we under-claim.)
    pub fn add_agent_run(&mut self, cost_usd: f64) {
        self.turns += 1;
        self.cost_usd += cost_usd;
    }
    #[must_use]
    pub fn summary(&self) -> String {
        let pct = if self.baseline_usd > 0.0 {
            (self.saved_usd / self.baseline_usd * 100.0).round()
        } else {
            0.0
        };
        let mut s = format!(
            "session: {} turn(s) · ${:.4} spent · saved ${:.4} ({pct:.0}%)",
            self.turns, self.cost_usd, self.saved_usd
        );
        if self.compaction_calls > 0 {
            s.push_str(&format!(
                " · compaction ${:.4} ({} call(s), est. −{} tok/turn, unbooked)",
                self.compaction_spend_usd, self.compaction_calls, self.compaction_est_tok_per_turn
            ));
        }
        if self.tool_trim_tokens > 0 {
            s.push_str(&format!(
                " · tool-trim −{} tok (est.)",
                self.tool_trim_tokens
            ));
        }
        s
    }
}

/// What `/retry` should do, computed by [`prepare_retry`].
#[derive(Debug)]
enum RetryPlan {
    /// Re-run the last turn. `restore` holds the assistant reply that was
    /// removed (push it back to undo a failed retry), or `None` when the
    /// trailing turn was an unanswered user message.
    Ready { restore: Option<Message> },
    /// No user turn in history — nothing to retry.
    Nothing,
}

/// Prepare the conversation to re-run the last turn. A trailing assistant reply
/// is popped and returned in `restore` so that a *failed* retry can be made a
/// no-op on history (no dangling user turn — two consecutive user messages
/// break the Anthropic/Gemini APIs — and no lost answer).
fn prepare_retry(conv: &mut Conversation) -> RetryPlan {
    match conv.messages.last() {
        Some(Message::Assistant { .. }) => RetryPlan::Ready {
            restore: conv.messages.pop(),
        },
        Some(Message::User { .. }) => RetryPlan::Ready { restore: None },
        _ => RetryPlan::Nothing,
    }
}

/// The text of the most recent assistant reply, if any.
#[must_use]
fn last_assistant_text(conv: &Conversation) -> Option<String> {
    conv.messages.iter().rev().find_map(|m| match m {
        Message::Assistant {
            content: Some(MessageContent::Text(t)),
            ..
        } => Some(t.clone()),
        _ => None,
    })
}

/// Stream one turn. Prints the assistant text live and the cost footer, and
/// returns the full reply for history. Returns `Err` (turn failed) on a non-2xx
/// gateway response so the caller can drop the unanswered user message.
async fn stream_turn(
    client: &tt_client::Client,
    conv: &Conversation,
) -> anyhow::Result<(String, Option<UsageInfo>)> {
    let mut stream = client
        .chat()
        .model(&conv.model)
        .messages(conv.wire_messages())
        // A human is sitting in this REPL: declare interactivity so the
        // gateway hard-clears the advisory batch-eligibility route action
        // (belt-and-braces — streaming is already gateway-cleared, but the
        // CLI states its intent explicitly).
        .interactive()
        .stream()
        .await
        .context("request to gateway failed")?;

    let served_model = stream
        .header_cost()
        .model_used
        .clone()
        .unwrap_or_else(|| conv.model.clone());

    let mut spinner = Some(ui::spinner("…"));
    let mut reply = String::new();
    let mut usage: Option<UsageInfo> = None;

    while let Some(ev) = stream.next().await.context("stream error")? {
        match ev {
            tt_client::StreamEvent::Delta(t) => {
                spinner.take(); // clear the spinner on the first token
                print!("{t}");
                use std::io::Write as _;
                let _ = std::io::stdout().flush();
                reply.push_str(&t);
            }
            tt_client::StreamEvent::Usage(u) => usage = Some(UsageInfo::from(u)),
            _ => {} // StreamEvent is #[non_exhaustive] (external crate) → wildcard required
        }
    }
    drop(spinner);
    println!();
    if let Some(u) = &usage {
        println!(
            "{}",
            format_turn_footer(
                &served_model,
                u.input_tokens,
                u.output_tokens,
                u.cost_usd,
                u.saved_usd,
                u.baseline_cost_usd
            )
        );
    }
    Ok((reply, usage))
}

/// Stream the current conversation: print live, push the assistant reply, and
/// update the ledger. Returns true on success. The caller decides whether to
/// drop the pending user turn on failure.
async fn do_turn(client: &tt_client::Client, conv: &mut Conversation, ledger: &mut Ledger) -> bool {
    match stream_turn(client, conv).await {
        Ok((reply, usage)) => {
            conv.push_assistant(reply);
            if let Some(u) = usage {
                ledger.add(&u);
            }
            true
        }
        Err(e) => {
            ui::error(&format!("{e:#}"));
            false
        }
    }
}

/// Drive one turn through the server-side agent loop (`POST /v1/agent/runs`)
/// instead of the plain chat path. The gateway owns the model->tool->model loop
/// (mid-loop down-routing, judge-gated summarize, substep cache); the client
/// posts the whole conversation, gets back a terminal run, and carries only the
/// final assistant text into history — matching [`do_turn`]'s contract so the
/// caller's snapshot/rollback on `false` stays correct.
///
/// With `tools_enabled`, the four read-only gateway tools are advertised so the
/// loop can call them SERVER-SIDE; the client never runs a tool (hence the
/// [`crate::agent::DeclineExecutor`]). Cost is GATEWAY-ATTRIBUTED (the run
/// body's `usage.cost_usd`) — the endpoint returns no baseline/saved, so this
/// books cost only via [`Ledger::add_agent_run`] and never claims a savings %.
async fn agent_turn(
    client: &tt_client::Client,
    conv: &mut Conversation,
    ledger: &mut Ledger,
    tools_enabled: bool,
) -> bool {
    let mut builder = client
        .agent()
        .model(&conv.model)
        // A human is waiting in the REPL: declare interactivity so the gateway
        // never marks this run batch-eligible (mirrors the plain chat path).
        .interactive()
        .messages(conv.wire_messages());
    if tools_enabled {
        builder = builder.tools(crate::agent::gateway_tool_defs());
    }
    let outcome = match builder.run(&crate::agent::DeclineExecutor).await {
        Ok(o) => o,
        Err(e) => {
            ui::error(&format!("agent run failed: {e}"));
            return false;
        }
    };
    let run = &outcome.run;
    let Some(answer) = run.text().map(str::to_string) else {
        ui::warn("the run produced no final text answer");
        return false;
    };
    println!("{answer}");
    conv.push_assistant(answer);
    let u = &run.usage;
    ledger.add_agent_run(u.cost_usd);
    println!(
        "{}",
        format_agent_footer(
            &conv.model,
            u.prompt_tokens,
            u.completion_tokens,
            u.cost_usd,
            run.turns
        )
    );
    if let Some(tax) = run.summarizer_tax_usd {
        if tax > 0.0 {
            ui::note(&format!("summarizer measurement tax: ${tax:.6}"));
        }
    }
    true
}

/// Route a turn to the right driver: the server-side agent loop (`--server-loop`),
/// the client-side tool-calling loop (tools on), or the streamed path (off).
async fn dispatch_turn(
    client: &tt_client::Client,
    conv: &mut Conversation,
    ledger: &mut Ledger,
    reg: &tt_mcp::tools::Registry,
    tools_enabled: bool,
    tool_trim: bool,
    server_loop: bool,
) -> bool {
    if server_loop {
        agent_turn(client, conv, ledger, tools_enabled).await
    } else if tools_enabled {
        tools::run_tool_turn(client, conv, reg, ledger, tool_trim).await
    } else {
        do_turn(client, conv, ledger).await
    }
}

/// Open `$VISUAL`/`$EDITOR` (fallback `vi`) on a temp file and return the
/// composed text, or `None` when left empty / the editor exits non-zero.
fn compose_in_editor() -> anyhow::Result<Option<String>> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    // O_EXCL + 0600 + a randomized name (no symlink/predictable-path attack),
    // auto-removed on drop so every return path cleans up.
    let file = tempfile::Builder::new()
        .prefix("tt-chat-")
        .suffix(".md")
        .tempfile()
        .context("create temp file for editor")?;
    let path = file.path().to_path_buf();
    let status = std::process::Command::new(&editor)
        .arg(&path)
        .status()
        .with_context(|| format!("launch editor `{editor}`"))?;
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    if !status.success() {
        return Ok(None);
    }
    let text = text.trim().to_string();
    Ok(if text.is_empty() { None } else { Some(text) })
}

/// One-line `/tools` status: tool-calling state + result-trim state.
fn print_tools_status(tools_enabled: bool, tool_trim: bool) {
    let trim = if tool_trim { "on" } else { "off" };
    if tools_enabled {
        ui::info(&format!(
            "tools: on (find_route_for, preview_cost, inspect_diff) · result-trim {trim}"
        ));
    } else {
        ui::info(&format!("tools: off · result-trim {trim}"));
    }
}

/// Options for [`run`] — one struct instead of a long positional parameter
/// list (clippy::too_many_arguments-clean and extensible).
#[derive(Debug, Default)]
pub struct RunOpts {
    /// Model to request (the gateway may route it).
    pub model: Option<String>,
    /// Optional system prompt for the conversation.
    pub system: Option<String>,
    /// Resume a saved session by name.
    pub resume: Option<String>,
    /// Enable tool-calling from the start.
    pub tools: bool,
    /// Token budget override for context management.
    pub max_context: Option<u32>,
    /// Disable lossless tool-result/arg trimming in the `/tools` loop
    /// (`chat::shape`; ON by default — lossless minify + class-safe drops).
    pub no_tool_trim: bool,
    /// Enable cache-aware compaction (`chat::compact`). OFF by default — the
    /// summary is a paid model call.
    pub compact: bool,
    /// Compact every K successful turns. Routed through the single
    /// `set_every` gate: K < 2 (incl. an explicit 0) warns + keeps the default.
    pub compact_every: u32,
    /// Model for the compaction summary call.
    pub compact_model: Option<String>,
    /// Drive each turn through the server-side agent loop (`POST /v1/agent/runs`)
    /// so TT's own levers run — mechanical mid-loop down-routing, judge-gated
    /// summarize, budget cap. OFF by default (opt-in; no behavior change).
    pub server_loop: bool,
    /// `--tt-api-key` flag.
    pub flag_key: Option<String>,
    /// `--tt-api-base` flag.
    pub flag_base: Option<String>,
}

/// Entry point for `tt chat`.
pub async fn run(opts: RunOpts) -> anyhow::Result<()> {
    let RunOpts {
        model,
        system,
        resume,
        tools,
        max_context,
        no_tool_trim,
        compact,
        compact_every,
        compact_model,
        server_loop,
        flag_key,
        flag_base,
    } = opts;
    let ctx = ResolvedContext::load(flag_key, flag_base)?;
    let key = ctx
        .api_key_string()
        .context("no API key — run `tt login --token <KEY>` or set TT_API_KEY")?;
    let base = ctx.base_url.trim_end_matches('/').to_string();
    let http = reqwest::Client::new();
    let client = tt_client::Client::new(base.clone(), key.clone());

    let mut conv = match resume {
        Some(n) => {
            session::load(&session::sessions_dir(), &n).with_context(|| format!("resume `{n}`"))?
        }
        None => Conversation::new(
            model.unwrap_or_else(|| DEFAULT_CHAT_MODEL.to_string()),
            system,
        ),
    };
    let mut ledger = Ledger::default();
    let registry = tools::build_registry();
    let mut tools_enabled = tools;
    let mut tool_trim = !no_tool_trim;
    // Best-effort: real per-model windows from the gateway catalog. On any
    // failure (offline / old gateway / pre-auth) fall back to the prefix table.
    let catalog_windows = match crate::catalog::fetch_catalog(&http, &base, Some(&key)).await {
        Ok(models) => crate::catalog::windows_map(&models),
        Err(_) => std::collections::HashMap::new(),
    };
    let mut ctx = budget::ContextState::new(max_context, catalog_windows);
    // Cache-aware compaction (OFF by default — the summary is a paid call).
    // The flag shares the `/compact every` set_every gate: K < 2 (incl. an
    // explicit `--compact-every 0`) warns and keeps the default cadence.
    let mut cstate = compact::CompactionState::new(compact, compact_model);
    if let Err(msg) = cstate.set_every(compact_every) {
        ui::warn(&msg);
    }
    ui::heading(&format!(
        "tt chat · {} via TokenTrimmer{}{}   (/help)",
        conv.model,
        if server_loop { " · server loop" } else { "" },
        if tools_enabled { " · tools on" } else { "" }
    ));

    let mut rl = rustyline::DefaultEditor::new().context("init readline")?;
    let prompt = ui::accent().apply_to("› ").to_string();
    loop {
        match rl.readline(&prompt) {
            Ok(line) => {
                let _ = rl.add_history_entry(line.as_str());
                match Command::parse(&line) {
                    Command::Chat(t) if t.is_empty() => {}
                    Command::Chat(t) => {
                        let snapshot = conv.messages.clone();
                        conv.push_user(t);
                        ctx.manage(&mut conv);
                        if !dispatch_turn(
                            &client,
                            &mut conv,
                            &mut ledger,
                            &registry,
                            tools_enabled,
                            tool_trim,
                            server_loop,
                        )
                        .await
                        {
                            // failed turn → no-op on history: drop the user turn
                            // AND undo any trim manage() did before sending.
                            conv.messages = snapshot;
                        } else {
                            // Compaction runs only AFTER a successful turn —
                            // never between snapshot and dispatch, so the
                            // restore contract above stays byte-for-byte.
                            cstate.note_turn();
                            compact::maybe_compact(
                                &client,
                                &mut conv,
                                &mut cstate,
                                &mut ctx,
                                &mut ledger,
                            )
                            .await;
                        }
                    }
                    Command::Help => print_help(),
                    Command::Clear => {
                        conv.clear();
                        cstate.reset_cadence(); // counter is runtime-only
                        ui::info("(conversation cleared)");
                    }
                    Command::Model(Some(m)) => {
                        conv.model = m;
                        ui::info(&format!("model → {}", conv.model));
                    }
                    Command::Model(None) => ui::info(&format!("model: {}", conv.model)),
                    Command::System(Some(s)) => {
                        conv.system = Some(s);
                        ui::info("(system prompt set)");
                    }
                    Command::System(None) => match &conv.system {
                        Some(s) => ui::info(&format!("system: {s}")),
                        None => ui::info("(no system prompt)"),
                    },
                    Command::Save(name) => {
                        let n = name.unwrap_or_else(|| session::auto_name(&conv));
                        match session::save(&session::sessions_dir(), &n, &conv) {
                            Ok(p) => ui::success(&format!("saved session → {}", p.display())),
                            Err(e) => ui::error(&format!("{e:#}")),
                        }
                    }
                    Command::Resume(name) => match session::load(&session::sessions_dir(), &name) {
                        Ok(c) => {
                            conv = c;
                            // first compaction fires K turns after a resume
                            cstate.reset_cadence();
                            ui::info(&format!("(resumed · {} messages)", conv.messages.len()));
                        }
                        Err(e) => ui::error(&format!("{e:#}")),
                    },
                    Command::Sessions => {
                        let metas = session::list(&session::sessions_dir()).unwrap_or_default();
                        if metas.is_empty() {
                            ui::info("no saved sessions");
                        } else {
                            let mut t =
                                ui::table(&["NAME", "MODEL", "TURNS"], console::colors_enabled());
                            for m in metas {
                                t.add_row(vec![m.name, m.model, m.turns.to_string()]);
                            }
                            println!("{t}");
                        }
                    }
                    Command::Cost => ui::info(&ledger.summary()),
                    Command::Context(set) => {
                        if let Some(0) = set {
                            ctx.override_budget = None;
                            ui::info("context budget cleared → using the per-model window");
                        } else if let Some(n) = set {
                            ctx.override_budget = Some(n);
                            ui::info(&format!("context budget → {n} tokens"));
                        } else {
                            let budget = ctx.budget(&conv.model);
                            let est = ctx.estimate(&conv);
                            let pct = (f64::from(est) / f64::from(budget) * 100.0) as u32;
                            ui::info(&format!(
                                "context: ~{est} / {budget} tokens ({pct}%) [{}]",
                                conv.model
                            ));
                        }
                    }
                    Command::Trim => {
                        let dropped = budget::manual_trim(&mut conv, &ctx);
                        ui::info(&format!("trimmed {dropped} old message(s)"));
                    }
                    Command::Tools(arg) => match arg {
                        ToolsArg::Toggle | ToolsArg::Set(_) => {
                            tools_enabled = match arg {
                                ToolsArg::Set(b) => b,
                                _ => !tools_enabled,
                            };
                            print_tools_status(tools_enabled, tool_trim);
                        }
                        ToolsArg::Trim(b) => {
                            tool_trim = b;
                            print_tools_status(tools_enabled, tool_trim);
                        }
                        ToolsArg::Bad(usage) => ui::warn(&usage),
                    },
                    Command::Compact(arg) => {
                        compact::handle(
                            arg,
                            &client,
                            &mut conv,
                            &mut cstate,
                            &mut ctx,
                            &mut ledger,
                        )
                        .await;
                    }
                    Command::Editor => match compose_in_editor() {
                        Ok(Some(t)) => {
                            let snapshot = conv.messages.clone();
                            conv.push_user(t);
                            ctx.manage(&mut conv);
                            if !dispatch_turn(
                                &client,
                                &mut conv,
                                &mut ledger,
                                &registry,
                                tools_enabled,
                                tool_trim,
                                server_loop,
                            )
                            .await
                            {
                                conv.messages = snapshot; // no-op on history
                            } else {
                                cstate.note_turn();
                                compact::maybe_compact(
                                    &client,
                                    &mut conv,
                                    &mut cstate,
                                    &mut ctx,
                                    &mut ledger,
                                )
                                .await;
                            }
                        }
                        Ok(None) => ui::info("(editor: nothing sent)"),
                        Err(e) => ui::error(&format!("{e:#}")),
                    },
                    Command::Retry => match prepare_retry(&mut conv) {
                        RetryPlan::Ready { restore } => {
                            if !dispatch_turn(
                                &client,
                                &mut conv,
                                &mut ledger,
                                &registry,
                                tools_enabled,
                                tool_trim,
                                server_loop,
                            )
                            .await
                            {
                                // Failed retry → no-op on history: restore the
                                // prior reply so we never leave a dangling user
                                // turn or lose a good answer.
                                if let Some(a) = restore {
                                    conv.messages.push(a);
                                }
                            }
                        }
                        RetryPlan::Nothing => ui::warn("nothing to retry"),
                    },
                    Command::Copy => {
                        match last_assistant_text(&conv).filter(|t| !t.trim().is_empty()) {
                            None => ui::warn("nothing to copy"),
                            Some(_) if !console::user_attended() => {
                                ui::warn("/copy needs an interactive terminal")
                            }
                            Some(text) if text.len() > OSC52_MAX_BYTES => ui::warn(&format!(
                                "reply too large to copy ({} KB; OSC52 cap ~{} KB)",
                                text.len() / 1024,
                                OSC52_MAX_BYTES / 1024
                            )),
                            Some(text) => {
                                let seq = wrap_osc52_for_mux(
                                    osc52_copy(&text),
                                    std::env::var_os("TMUX").is_some(),
                                    &std::env::var("TERM").unwrap_or_default(),
                                );
                                print!("{seq}");
                                use std::io::Write as _;
                                let _ = std::io::stdout().flush();
                                ui::info("(sent to clipboard — needs an OSC52-capable terminal)");
                            }
                        }
                    }
                    Command::Exit => break,
                    Command::Unknown(c) => {
                        ui::warn(&format!("unknown command /{c} — /help for commands"))
                    }
                }
            }
            Err(rustyline::error::ReadlineError::Interrupted) => continue, // Ctrl-C: cancel line
            Err(rustyline::error::ReadlineError::Eof) => break,            // Ctrl-D: quit
            Err(e) => {
                ui::error(&format!("input error: {e}"));
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use httpmock::prelude::*;

    #[tokio::test]
    async fn stream_turn_streams_reply_and_usage() {
        let server = MockServer::start_async().await;
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "event: tokentrimmer.usage\n",
            "data: {\"cost_usd\":0.0001,\"baseline_cost_usd\":0.0004,\"saved_usd\":0.0003,\"input_tokens\":10,\"output_tokens\":2,\"cached_tokens\":0}\n\n",
            "data: [DONE]\n\n",
        );
        server.mock(|when, then| {
            when.method(POST)
                .path("/v1/chat/completions")
                // `tt chat` always declares interactivity — a human is waiting,
                // so the gateway must never mark this traffic batch-eligible.
                .header("x-tokentrimmer-interactive", "1");
            then.status(200)
                .header("content-type", "text/event-stream")
                .header("x-tokentrimmer-model-used", "gpt-4o-mini")
                .body(sse);
        });

        let client = tt_client::Client::new(server.base_url(), "k");
        let mut conv = Conversation::new("gpt-4o-mini".into(), None);
        conv.push_user("hi".into());

        let (reply, usage) = stream_turn(&client, &conv).await.unwrap();
        assert_eq!(reply, "Hello");
        let u = usage.expect("usage event");
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.output_tokens, 2);
    }

    #[tokio::test]
    async fn agent_turn_drives_server_loop_and_books_gateway_cost() {
        let server = MockServer::start_async().await;
        // The server-side loop endpoint: a terminal completed run whose usage
        // aggregates the whole loop's served cost (no per-turn headers here).
        let m = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/agent/runs")
                // A human is waiting in the REPL — declare interactivity so the
                // gateway never marks this run batch-eligible.
                .header("x-tokentrimmer-interactive", "1");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(serde_json::json!({
                    "id": "r1", "status": "completed", "turns": 2,
                    "usage": { "prompt_tokens": 15, "completion_tokens": 6, "cost_usd": 0.0004 },
                    "messages": [
                        { "role": "user", "content": "hi" },
                        { "role": "assistant", "content": "Hello from the loop." }
                    ]
                }));
        });

        let client = tt_client::Client::new(server.base_url(), "k");
        let mut conv = Conversation::new("gpt-4o-mini".into(), None);
        conv.push_user("hi".into());
        let mut ledger = Ledger::default();

        // tools off → no gateway tools advertised, but the loop still runs.
        let ok = agent_turn(&client, &mut conv, &mut ledger, false).await;
        assert!(ok);
        m.assert();
        // Only the final assistant text is carried into history (matches do_turn).
        assert!(
            matches!(conv.messages.last(), Some(Message::Assistant { content: Some(MessageContent::Text(t)), .. }) if t == "Hello from the loop."),
            "last = {:?}",
            conv.messages.last()
        );
        // Gateway-attributed cost is booked; saved/baseline stay zero (honesty).
        assert_eq!(ledger.turns, 1);
        assert!((ledger.cost_usd - 0.0004).abs() < 1e-9);
        assert_eq!(ledger.saved_usd, 0.0);
        assert_eq!(ledger.baseline_usd, 0.0);
    }

    #[test]
    fn footer_formats_with_savings() {
        console::set_colors_enabled(false);
        let s = format_turn_footer("gpt-4o-mini", 10, 20, 0.0001, 0.0003, 0.0004);
        assert_eq!(s, "· gpt-4o-mini · 30 tok · $0.0001 · saved 75%");
        let s2 = format_turn_footer("gpt-4o", 5, 5, 0.001, 0.0, 0.0);
        assert_eq!(s2, "· gpt-4o · 10 tok · $0.0010");
    }

    #[test]
    fn agent_footer_states_cost_without_a_savings_claim() {
        console::set_colors_enabled(false);
        // The server-loop footer reports gateway-attributed cost + server turns
        // and NEVER a `saved …%` segment (the agent endpoint has no baseline).
        let s = format_agent_footer("gpt-4o-mini", 15, 6, 0.0004, 2);
        assert_eq!(
            s,
            "· gpt-4o-mini · 21 tok · $0.0004 · server loop (2 turn(s), gateway-attributed)"
        );
        assert!(!s.contains("saved"), "{s}");
    }

    #[test]
    fn add_agent_run_books_cost_only_never_savings() {
        // HONESTY GUARD: an agent-loop turn raises turns + cost, but must leave
        // saved/baseline at zero (no attributed baseline to claim a % against).
        let mut l = Ledger::default();
        l.add_agent_run(0.0004);
        l.add_agent_run(0.0006);
        assert_eq!(l.turns, 2);
        assert!((l.cost_usd - 0.001).abs() < 1e-9);
        assert_eq!(l.saved_usd, 0.0);
        assert_eq!(l.baseline_usd, 0.0);
    }

    #[test]
    fn ledger_accumulates() {
        console::set_colors_enabled(false);
        let u = UsageInfo {
            cost_usd: 0.001,
            baseline_cost_usd: 0.004,
            saved_usd: 0.003,
            input_tokens: 1,
            output_tokens: 1,
            cached_tokens: 0,
        };
        let mut l = Ledger::default();
        l.add(&u);
        l.add(&u);
        assert_eq!(l.turns, 2);
        let s = l.summary();
        assert!(s.contains("2 turn"), "{s}");
        assert!(s.contains("75%"), "{s}");
    }

    #[test]
    fn prepare_retry_pops_and_classifies() {
        let mut c = Conversation::new("m".into(), None);
        assert!(matches!(prepare_retry(&mut c), RetryPlan::Nothing));
        c.push_user("hi".into());
        c.push_assistant("yo".into());
        // ends with assistant → pop it, hand it back for restore-on-failure
        assert!(matches!(
            prepare_retry(&mut c),
            RetryPlan::Ready { restore: Some(_) }
        ));
        assert_eq!(c.messages.len(), 1); // assistant popped, user remains
                                         // now ends with a user (unanswered) → ready, nothing to restore
        assert!(matches!(
            prepare_retry(&mut c),
            RetryPlan::Ready { restore: None }
        ));
        assert_eq!(c.messages.len(), 1);
    }

    #[test]
    fn last_assistant_text_finds_latest() {
        let mut c = Conversation::new("m".into(), None);
        assert!(last_assistant_text(&c).is_none());
        c.push_user("hi".into());
        c.push_assistant("first".into());
        c.push_user("more".into());
        c.push_assistant("second".into());
        assert_eq!(last_assistant_text(&c).as_deref(), Some("second"));
    }

    #[test]
    fn conversation_clear_and_system() {
        let mut c = Conversation::new("m".into(), Some("be brief".into()));
        c.push_user("hi".into());
        c.push_assistant("yo".into());
        assert_eq!(c.messages.len(), 2);
        assert_eq!(c.wire_messages().len(), 3); // system prepended
        c.clear();
        assert!(c.messages.is_empty());
    }
}
