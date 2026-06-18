# Slice 1a: server-side agent loop (over gateway tools) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up `POST /v1/agent/runs` — a synchronous server-side agentic loop that runs model→tool→model over TT's read-only gateway tools until a final answer or `max_turns`, plus the COST-3(U) doc-closure.

**Architecture:** Builds on the merged slice 1a-0 (`complete_once(state, ctx, Prepared) -> CompletionOutcome`). Extract a `prepare(...)` so the loop rebuilds `Prepared` per turn; add a `gateway_tools` executor (the 4 read-only TT tools via their underlying libs, no `core→mcp` dep); a `TurnCompleter` seam so the loop is unit-testable without a provider; the run types + loop; and the endpoint. Synchronous, polling-free in 1a (a run completes or returns `incomplete`); no Redis, no client round-trip (→ 1b).

**Tech Stack:** Rust, axum, `tt-shared` messages, `tt-preview`, `tt-inspect-core`+`tt-inspect-rules-tier1`, `tt-shared::batch_advisor`.

**Spec:** `docs/superpowers/specs/2026-06-17-server-side-agentic-loop-slice1a-design.md`. **Resume context:** memory `server-side-agent-loop`.

**Behavior-preservation gate (Task 1 only):** baseline `TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres cargo test -p tt-core --lib --tests` = **753 passed, 0 failed, 15 ignored**. Use `--lib --tests` (NOT `--all-targets` for the test RUN — benches hang). The 2 `middleware/trace.rs` doctests are pre-existing-broken; ignore. The `cargo test (workspace)` CI check is disk-flaky → `gh run rerun <id> --failed`.

**Reference (verified):**
- `crates/core/src/routes/chat.rs`: `pub(crate) struct Prepared` (862, 31 fields), `pub(crate) enum CompletionOutcome { Dispatched { response: ChatCompletionResponse, headers: Box<CompletionHeaders> }, CacheHit(Response) }` (976), `pub(crate) async fn complete_once(state: &AppState, ctx: &RequestContext, prep: Prepared) -> ApiResult<CompletionOutcome>` (1001); handler builds `Prepared` at 3197 (before/at the non-streaming arm) and calls `complete_once` at 3232. The shared setup runs before the `if req.stream` branch.
- Tool reference impls to PORT (read these): `crates/mcp/src/tools/{find_route_for,preview_cost,inspect_diff,batch_savings}.rs`. Underlying libs: `tt_preview::preview(&PreviewRequest)->Result<PreviewResponse,_>`; `tt_inspect_core::{Language::from_extension, Engine::new/add_rule/scan}` + `tt_inspect_rules_tier1::all_rules()`; `tt_shared::batch_advisor::{project_batch_savings, project_batch_savings_with_tags}` + `tt_shared::pricing::catalog()`; `find_route_for` = pure heuristic `classify_task()/route()` (internal to its mcp file — PORT/inline into core). Allowlist mirrors `crates/core/src/passes/agentic_budget/substep_cache.rs::READ_ONLY_TOOLS` = `["find_route_for","preview_cost","inspect_diff","batch_savings"]`.
- Types (`crates/shared/src/messages.rs`): `Message::{Assistant{content:Option<MessageContent>,tool_calls:Vec<ToolCall>,name}, Tool{content:MessageContent,tool_call_id:String}}`, `ToolCall{id,r#type,function:ToolCallFunction{name,arguments:String}}`, `MessageContent::Text(String)`, `ChatCompletionRequest{model,messages,tools,stream,..}`, `ChatCompletionResponse{choices:Vec<Choice{message,finish_reason}>,usage:Usage{prompt_tokens,completion_tokens,..}}`.
- Router: `crates/core/src/server.rs` `short` router (~77); auth inherited via `.layer(middleware::auth::middleware)`. Auth ctx: `Option<Extension<ApiKeyContext{key_id,org_id,tier}>>`.

---

### Task 1: Extract `prepare(...)` (behavior-preserving refactor)

**Files:** Modify `crates/core/src/routes/chat.rs`.

**Contract:** Extract the shared setup that builds `Prepared` into `pub(crate) async fn prepare(state: &AppState, ctx: &RequestContext, req: &mut ChatCompletionRequest, /* header-derived inputs the setup reads */) -> ApiResult<Prepared>`. The handler calls `let prep = prepare(...)?;` once before the `if req.stream` branch; the non-streaming arm passes `prep` to `complete_once`; the streaming arm reads the fields it needs from `prep` (refactored from inline locals — behavior-identical). All early returns within setup (e.g. credential/route errors) propagate via `?` as today.

- [ ] **Step 1: Carve `prepare`.** Move the pre-branch setup + the `Prepared { .. }` construction (currently inline at ~3197 and earlier) into `prepare`. The handler calls it before the branch; both branches consume `prep`. Keep sandbox short-circuit + auth/ctx build in `handler` (before `prepare`).
- [ ] **Step 2: Build + clippy.** `cargo build -p tt-core` then `cargo clippy --workspace --all-targets -- -D warnings`. Expected PASS.
- [ ] **Step 3: THE GATE.** `TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres cargo test -p tt-core --lib --tests`. Expected: **753 passed, 0 failed**. If different, the carve changed behavior — fix; never edit a test.
- [ ] **Step 4: fmt + commit.** `cargo fmt -p tt-core`; then:
```bash
git add crates/core/src/routes/chat.rs
git commit -m "refactor(core): extract prepare() so the agent loop rebuilds Prepared per turn (slice 1a)" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: `gateway_tools` executor

**Files:** Create `crates/core/src/routes/gateway_tools.rs`; Modify `crates/core/src/routes/mod.rs` (`mod gateway_tools;`); Modify `crates/core/Cargo.toml` (add `tt-inspect-core`, `tt-inspect-rules-tier1` workspace deps if absent).

- [ ] **Step 1: Write the executor (port the 4 MCP tool bodies, calling underlying libs directly).**

Read the 4 reference impls in `crates/mcp/src/tools/` and port their logic into a core executor that does NOT depend on `crates/mcp`. The public API:

```rust
//! Server-side execution of the read-only TT "gateway tools" the agent loop
//! can run inline. Mirrors the MCP tools' logic but calls the underlying libs
//! directly (no `core->mcp` dependency cycle). Read-only/idempotent only —
//! the allowlist mirrors `agentic_budget::substep_cache::READ_ONLY_TOOLS`.

/// The hand-verified read-only tool allowlist. A tool not on this list is NOT
/// gateway-executable (the loop round-trips it to the client in slice 1b).
pub(crate) const GATEWAY_TOOLS: &[&str] =
    &["find_route_for", "preview_cost", "inspect_diff", "batch_savings"];

pub(crate) fn is_gateway_tool(name: &str) -> bool {
    GATEWAY_TOOLS.contains(&name)
}

/// Execute a gateway tool by name with its JSON `arguments` string (OpenAI
/// tool-call convention). Returns the tool result as a string for a
/// `Message::Tool` body. A tool error is returned as `Ok(error_text)` so the
/// model can react (NOT an `Err` that aborts the run) — except a genuinely
/// non-gateway tool which is `Err(GatewayToolError::NotExecutable)`.
pub(crate) fn execute(name: &str, arguments: &str) -> Result<String, GatewayToolError> {
    match name {
        "find_route_for" => Ok(run_find_route_for(arguments)),
        "preview_cost" => Ok(run_preview_cost(arguments)),
        "inspect_diff" => Ok(run_inspect_diff(arguments)),
        "batch_savings" => Ok(run_batch_savings(arguments)),
        _ => Err(GatewayToolError::NotExecutable(name.to_string())),
    }
}
```

Each `run_*(arguments: &str) -> String`: parse the JSON args into the tool's input type (per the reference impl), call the underlying lib, serialize the result to JSON (or a human string matching the MCP tool's output), and on a parse/lib error return a short error string (so the model sees it). `run_find_route_for` ports the pure `classify_task`/`route` heuristic inline. `GatewayToolError::NotExecutable(String)` is the only hard error (the loop maps it to `incomplete` in 1a).

- [ ] **Step 2: Unit tests** (no DB, no provider):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn allowlist_gating() {
        assert!(is_gateway_tool("preview_cost"));
        assert!(!is_gateway_tool("write_file"));
    }
    #[test]
    fn find_route_for_executes_pure_heuristic() {
        let out = execute("find_route_for", r#"{"task_description":"classify this short text"}"#).unwrap();
        assert!(!out.is_empty()); // returns a model recommendation + rationale
    }
    #[test]
    fn unknown_tool_is_not_executable() {
        assert!(matches!(execute("write_file", "{}"), Err(GatewayToolError::NotExecutable(_))));
    }
    #[test]
    fn bad_args_returns_error_text_not_panic() {
        // a gateway tool with unparseable args returns Ok(error_text), not Err/panic
        let out = execute("preview_cost", "not json").unwrap();
        assert!(out.to_lowercase().contains("error") || out.to_lowercase().contains("invalid"));
    }
}
```
- [ ] **Step 3: Run + commit.** `cargo test -p tt-core --lib gateway_tools` → PASS; clippy; fmt; commit:
```bash
git add crates/core/src/routes/gateway_tools.rs crates/core/src/routes/mod.rs crates/core/Cargo.toml
git commit -m "feat(core): gateway_tools executor — the 4 read-only TT tools via underlying libs (slice 1a)" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Run types + the loop (`TurnCompleter` seam)

**Files:** Create `crates/core/src/routes/agent_run.rs`; Modify `crates/core/src/routes/mod.rs` (`pub mod agent_run;`).

- [ ] **Step 1: Write the types + the loop (generic over a completer so it's testable without a provider).**

```rust
//! Server-side agentic loop (slice 1a): run model->tool->model over the
//! read-only gateway tools until a final answer or `max_turns`. Synchronous;
//! no Redis/no client round-trip (slice 1b). Generic over `TurnCompleter` so
//! tests inject a stub.

use async_trait::async_trait;
use tt_shared::messages::{ChatCompletionRequest, ChatCompletionResponse, Message, MessageContent};
use crate::error::ApiError; // adjust path to the crate's ApiError

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus { Completed, Incomplete, Failed }

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct RunUsage { pub prompt_tokens: u64, pub completion_tokens: u64 }

#[derive(Debug, Clone, serde::Serialize)]
pub struct Run {
    pub id: uuid::Uuid,
    pub status: RunStatus,
    pub messages: Vec<Message>,
    pub turns: u32,
    pub usage: RunUsage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One completion turn. Production impl wraps `prepare` + `complete_once`;
/// tests inject a stub. Returns the assistant message + usage.
#[async_trait]
pub trait TurnCompleter: Send + Sync {
    async fn complete(&self, req: ChatCompletionRequest) -> Result<(Message, RunUsage), ApiError>;
}

const DEFAULT_MAX_TURNS: u32 = 8;
const MAX_MAX_TURNS: u32 = 32;

/// Run the loop. `model`/`messages`/`tools` come from the request; `max_turns`
/// is clamped to [1, 32].
pub async fn run_loop(
    completer: &dyn TurnCompleter,
    id: uuid::Uuid,
    model: String,
    mut messages: Vec<Message>,
    tools: Vec<tt_shared::messages::Tool>,
    max_turns: u32,
) -> Run {
    let max_turns = max_turns.clamp(1, MAX_MAX_TURNS);
    let mut usage = RunUsage::default();
    for turn in 0..max_turns {
        let req = ChatCompletionRequest {
            model: model.clone(),
            messages: messages.clone(),
            tools: tools.clone(),
            stream: false,
            ..Default::default()
        };
        let (assistant, turn_usage) = match completer.complete(req).await {
            Ok(x) => x,
            Err(e) => {
                return Run { id, status: RunStatus::Failed, messages, turns: turn + 1, usage,
                             note: Some(format!("turn {turn} failed: {e}")) };
            }
        };
        usage.prompt_tokens += turn_usage.prompt_tokens;
        usage.completion_tokens += turn_usage.completion_tokens;
        messages.push(assistant.clone());

        let tool_calls = match &assistant {
            Message::Assistant { tool_calls, .. } => tool_calls.clone(),
            _ => Vec::new(),
        };
        if tool_calls.is_empty() {
            return Run { id, status: RunStatus::Completed, messages, turns: turn + 1, usage, note: None };
        }
        // Partition: every tool_call must be gateway-executable in 1a.
        for tc in &tool_calls {
            if !crate::routes::gateway_tools::is_gateway_tool(&tc.function.name) {
                return Run { id, status: RunStatus::Incomplete, messages, turns: turn + 1, usage,
                    note: Some(format!("client tool '{}' requires slice-1b round-trip", tc.function.name)) };
            }
        }
        for tc in &tool_calls {
            let result = match crate::routes::gateway_tools::execute(&tc.function.name, &tc.function.arguments) {
                Ok(s) => s,
                Err(e) => format!("tool error: {e}"), // append as result; model can react
            };
            messages.push(Message::Tool {
                content: MessageContent::Text(result),
                tool_call_id: tc.id.clone(),
            });
        }
    }
    Run { id, status: RunStatus::Incomplete, messages, turns: max_turns, usage,
          note: Some("max_turns reached".into()) }
}
```
(NOTE: confirm the exact `ApiError` import path + whether `async_trait` is a workspace dep — it is used widely in core; add if needed. Confirm `Tool`/`Message`/`MessageContent` import paths.)

- [ ] **Step 2: Loop unit tests with a stub completer** (no provider, no DB):
```rust
#[cfg(test)]
mod tests {
    use super::*;
    struct Stub { script: std::sync::Mutex<Vec<Message>> }
    #[async_trait]
    impl TurnCompleter for Stub {
        async fn complete(&self, _req: ChatCompletionRequest) -> Result<(Message, RunUsage), ApiError> {
            let mut s = self.script.lock().unwrap();
            Ok((s.remove(0), RunUsage { prompt_tokens: 1, completion_tokens: 1 }))
        }
    }
    fn assistant_final() -> Message { Message::Assistant { content: Some(MessageContent::Text("done".into())), tool_calls: vec![], name: None } }
    fn assistant_toolcall(name: &str) -> Message {
        Message::Assistant { content: None, name: None, tool_calls: vec![tt_shared::messages::ToolCall {
            id: "c1".into(), r#type: "function".into(),
            function: tt_shared::messages::ToolCallFunction { name: name.into(), arguments: r#"{"task_description":"x"}"#.into() } }] }
    }
    #[tokio::test]
    async fn completes_on_final_answer() {
        let stub = Stub { script: std::sync::Mutex::new(vec![assistant_final()]) };
        let run = run_loop(&stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 8).await;
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(run.turns, 1);
    }
    #[tokio::test]
    async fn gateway_tool_turn_then_final() {
        let stub = Stub { script: std::sync::Mutex::new(vec![assistant_toolcall("find_route_for"), assistant_final()]) };
        let run = run_loop(&stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 8).await;
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(run.turns, 2);
        // transcript carries the tool result between the two assistant turns
        assert!(run.messages.iter().any(|m| matches!(m, Message::Tool { .. })));
    }
    #[tokio::test]
    async fn unknown_tool_is_incomplete() {
        let stub = Stub { script: std::sync::Mutex::new(vec![assistant_toolcall("write_file")]) };
        let run = run_loop(&stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 8).await;
        assert_eq!(run.status, RunStatus::Incomplete);
        assert!(run.note.unwrap().contains("write_file"));
    }
    #[tokio::test]
    async fn max_turns_bound() {
        // always returns a (gateway) tool call → never completes
        let script: Vec<Message> = (0..10).map(|_| assistant_toolcall("find_route_for")).collect();
        let stub = Stub { script: std::sync::Mutex::new(script) };
        let run = run_loop(&stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 3).await;
        assert_eq!(run.status, RunStatus::Incomplete);
        assert_eq!(run.turns, 3);
    }
}
```
- [ ] **Step 3: Run + commit.** `cargo test -p tt-core --lib agent_run` → PASS; clippy; fmt; commit:
```bash
git add crates/core/src/routes/agent_run.rs crates/core/src/routes/mod.rs
git commit -m "feat(core): agent-run loop + types over a TurnCompleter seam (slice 1a)" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: `POST /v1/agent/runs` endpoint + production `TurnCompleter`

**Files:** Modify `crates/core/src/routes/agent_run.rs` (the handler + the prod completer); Modify `crates/core/src/server.rs` (route mount).

- [ ] **Step 1: Production `TurnCompleter`** wrapping `prepare` + `complete_once`:
```rust
/// Production completer: routes + dispatches each turn through the real
/// `prepare` + `complete_once` pipeline (per-turn routing/cache/telemetry).
struct GatewayCompleter<'a> { state: &'a AppState, ctx: RequestContext }

#[async_trait]
impl<'a> TurnCompleter for GatewayCompleter<'a> {
    async fn complete(&self, mut req: ChatCompletionRequest) -> Result<(Message, RunUsage), ApiError> {
        let prep = super::chat::prepare(self.state, &self.ctx, &mut req /*, header inputs */).await?;
        match super::chat::complete_once(self.state, &self.ctx, prep).await? {
            super::chat::CompletionOutcome::Dispatched { response, .. } => {
                let msg = response.choices.into_iter().next()
                    .map(|c| c.message)
                    .ok_or_else(|| ApiError::Upstream("empty choices".into()))?; // adjust ApiError variant
                let usage = RunUsage { prompt_tokens: response.usage.prompt_tokens as u64,
                                       completion_tokens: response.usage.completion_tokens as u64 };
                Ok((msg, usage))
            }
            super::chat::CompletionOutcome::CacheHit(_resp) => {
                // A cache hit returns a prebuilt HTTP Response; for the loop we
                // re-dispatch typed — but a cache hit means the cached answer is
                // valid; parse its body back to a Message. (1a: rare on agent
                // turns; if parsing is awkward, treat as a completed turn by
                // re-running without cache via tt_extras, OR document the
                // limitation. Implementer: pick the behavior-correct option and
                // note it.)
                Err(ApiError::Upstream("cache-hit on agent turn not yet handled (slice 1a)".into()))
            }
        }
    }
}
```
> **Implementer judgment:** the `CacheHit` arm needs a behavior-correct resolution — either (a) bypass L1/L2 for agent-loop turns by setting the request's cache mode off (cleanest: the loop's turns set `tt_extras.cache = "off"` or the equivalent so `complete_once` always returns `Dispatched`), or (b) parse the cached `Response` body back into a `ChatCompletionResponse`. Prefer (a) — it sidesteps the typed/HTTP mismatch and is correct (agent turns are fresh transcripts). Confirm the exact cache-disable knob in `CacheBehavior`/`tt_extras` and use it; update the loop's per-turn `ChatCompletionRequest` accordingly.

- [ ] **Step 2: The endpoint handler:**
```rust
#[derive(serde::Deserialize)]
pub struct CreateRunRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<tt_shared::messages::Tool>,
    #[serde(default)]
    pub max_turns: Option<u32>,
}

pub async fn create_run(
    State(state): State<AppState>,
    Extension(trace): Extension<TraceId>,
    auth_ctx: Option<Extension<ApiKeyContext>>,
    headers: HeaderMap,
    Json(req): Json<CreateRunRequest>,
) -> ApiResult<Json<Run>> {
    // Build RequestContext from auth (mirror chat::handler's ctx build for the
    // non-extractor parts; org/key from auth_ctx, creds from the bearer).
    let ctx = /* build RequestContext exactly as chat::handler does post-auth */;
    let completer = GatewayCompleter { state: &state, ctx };
    let id = uuid::Uuid::new_v4();
    let run = run_loop(&completer, id, req.model, req.messages, req.tools,
                       req.max_turns.unwrap_or(DEFAULT_MAX_TURNS)).await;
    Ok(Json(run))
}
```
> **Implementer:** factor the `RequestContext` construction so both `chat::handler` and `create_run` build it identically (a small shared helper is ideal; if too invasive, replicate the post-auth ctx build with a comment). Credentials resolution must match chat's (the run forwards the caller's bearer/creds per turn).

- [ ] **Step 3: Mount the route** in `crates/core/src/server.rs` `short` router:
```rust
.route("/v1/agent/runs", post(routes::agent_run::create_run))
```
(Auth middleware is inherited via the existing `.layer`.)

- [ ] **Step 4: Build + workspace test + clippy.** `cargo build -p tt-core`; `TEST_DATABASE_URL=... cargo test -p tt-core --lib --tests` (the loop unit tests + everything still green — the prod completer is exercised by an integration test if added, else compile-checked); `cargo clippy --workspace --all-targets -- -D warnings`. Expected PASS.
- [ ] **Step 5: fmt + commit.**
```bash
git add crates/core/src/routes/agent_run.rs crates/core/src/server.rs
git commit -m "feat(core): POST /v1/agent/runs endpoint + production TurnCompleter (slice 1a)" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: COST-3(U) accurate closure (slice 0)

**Files:** Modify `crates/core/src/passes/agentic_budget/mod.rs`.

- [ ] **Step 1: Correct the Sub-lever 3 framing.** The Sub-lever 3 block emits `out.warnings.push(format!("subagent_lane:{target}"))`. Keep the emission (clients may consume it) but rewrite the surrounding doc comment + the module-header "the handler's to wire (deferred there)" lines to state the accurate boundary: the gateway is a per-request proxy; cache-isolation of a down-routed sub-step is already model-keyed; the routed *dispatch* is realized by the **server-side agent loop** (`POST /v1/agent/runs`, slices 1a→2), NOT a missing chat-handler seam. No logic change.
- [ ] **Step 2: Build + test.** `cargo build -p tt-core && cargo test -p tt-core --lib agentic_budget` → PASS (no behavior change).
- [ ] **Step 3: fmt + commit.**
```bash
git add crates/core/src/passes/agentic_budget/mod.rs
git commit -m "docs(core): COST-3(U) — correct Sub-lever 3 framing to the per-request-proxy boundary (slice 1a)" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification (mirror required CI checks)
```bash
cargo fmt --check -p tt-core
cargo clippy --workspace --all-targets -- -D warnings
TEST_DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres cargo test -p tt-core --lib --tests   # 753 + the new loop/executor tests, 0 failed
cargo test --workspace --no-run        # all targets compile
cargo run -q -p tt-cli -- inspect .
cargo test -p tt-plan-core             # determinism goldens untouched
```
> `cargo test (workspace)` CI check is disk-flaky → rerun failed jobs. Determinism goldens unchanged (no plan-core touch).

## Self-Review (against the spec)
- `prepare` extraction so the loop rebuilds Prepared per turn → Task 1 (behavior-preserving, gated). ✓
- Gateway-tool executor (4 read-only tools via underlying libs, no core→mcp) → Task 2. ✓
- Run types + synchronous loop, generic over `TurnCompleter`, max_turns, gateway-only (unknown→incomplete) → Task 3. ✓
- `POST /v1/agent/runs` + production completer (prepare+complete_once per turn) + route mount → Task 4. ✓
- COST-3(U) doc-closure → Task 5. ✓
- Non-goals respected: no Redis, no client round-trip (1b), no levers (2), no streaming (3), no `/v1/chat/completions` behavior change (Task 1 gated). ✓
- **Open implementer judgment flagged:** the `CacheHit`-on-agent-turn handling (Task 4 Step 1 — prefer disabling cache for loop turns); the exact `prepare` header-input set + `RequestContext` build sharing (Tasks 1, 4).
- **Type consistency:** `Run`/`RunStatus`/`RunUsage`/`TurnCompleter`/`run_loop` used identically across Tasks 3–4; `gateway_tools::{is_gateway_tool, execute, GATEWAY_TOOLS}` across Tasks 2–3; `prepare`/`complete_once`/`CompletionOutcome` from the merged 1a-0.
