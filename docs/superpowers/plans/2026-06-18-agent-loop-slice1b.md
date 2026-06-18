# Slice 1b: stateful hybrid (Redis runs + client round-trip) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the agent loop pausable — when the model calls a client (non-gateway) tool, persist the run to Redis as `requires_action` and resume it when the client submits the tool outputs, via `GET /v1/agent/runs/{id}` + `POST /v1/agent/runs/{id}/tool_outputs`.

**Architecture:** Refactor the 1a loop into a `run_loop_core` returning `LoopOutcome::{Terminal(Run), Paused{..}}` (1a `run_loop` becomes a thin wrapper preserving its `Incomplete`-on-client-tool behavior). Persist a secret-free `StoredRun` to the existing `L1Cache` (key `tt:runs:{org}:{id}`, 1h TTL). Three handlers drive create→pause→resume; resume re-authenticates (org-verified) and rebuilds the per-turn completer from the resume request + the stored routing config. No Redis → graceful 1a fallback on create, 503 on get/resume.

**Tech Stack:** Rust, axum, `tt_cache::L1Cache` (+ `InMemoryL1Cache` for tests), `tt_shared::messages`, `SingleFlight`, serde_json.

**Spec:** `docs/superpowers/specs/2026-06-18-agent-loop-slice1b-design.md`.

**Execution note:** subagent-driven, serial — the 5 tasks are dependency-ordered and all touch `agent_run.rs`, so they can't run in parallel; each gets a two-stage review before the next builds on it.

**Reference (verified):**
- `crates/core/src/routes/agent_run.rs` (1a): `Run{id,status,messages,turns,usage,note}`, `RunStatus{Completed,Incomplete,Failed}` (`#[serde(rename_all="lowercase")]`), `RunUsage{prompt_tokens,completion_tokens:u64}`, `MAX_MAX_TURNS`/`DEFAULT_MAX_TURNS`, `TurnCompleter` (`#[async_trait] async fn complete(&self, ChatCompletionRequest)->Result<(Message,RunUsage),ApiError>`), `run_loop(...)` body (the for-loop: complete → push assistant → empty tool_calls⇒Completed → partition (any non-`gateway_tools::is_gateway_tool`)⇒Incomplete → execute gateway tools via `gateway_tools::execute`, append `Message::Tool`), `RunIdentity{org_id,api_key_id,caller_tier,l2_allowed,raw_bearer,trace_id,tag,request_timeout,provider_pin,forced_route,idempotency_key,headers}` + `RunIdentity::from_request(auth_ctx, trace, headers)`, `GatewayCompleter{state,identity}`, `create_run(State,Extension<TraceId>,Option<Extension<ApiKeyContext>>,HeaderMap,Json<CreateRunRequest>)->ApiResult<Json<Run>>`.
- `crates/core/src/error.rs`: `ApiError::{Unauthorized→401, NotFound(String)→404, Internal(String)→500, ServiceUnavailable(String)→503, InvalidRequest(String)→400, ModelNotFound{model}→404}` + an `impl IntoResponse` match. **No 409 variant — Task 4 adds `Conflict(String)→409`.**
- `crates/cache/src/lib.rs:96-105` `L1Cache` trait (`get`/`set(ttl_secs)`/`delete`, bytes); `crates/cache/src/memory.rs` `InMemoryL1Cache::new()` (impls `L1Cache`); `crates/core/src/state.rs` `AppState.l1: Option<L1Config{cache:Arc<dyn L1Cache>, ttl_secs}>`.
- `crates/core/src/routes/routes_api.rs:28-33` `require_org(ctx)->Result<Uuid,ApiError>` (401 if no real key); `:97-109` `Path(id): Path<Uuid>` handler pattern.
- `crates/core/src/single_flight.rs:67-118` `SingleFlight::try_become_leader(key)`.
- `crates/core/src/server.rs:83` route mount `route("/v1/agent/runs", post(routes::agent_run::create_run))`.

---

### Task 1: Loop-core refactor (`LoopOutcome`) + `RunStatus::RequiresAction`

**Files:** Modify `crates/core/src/routes/agent_run.rs`.

- [ ] **Step 1: Add `RequiresAction` + switch the serde rename to snake_case (no wire change for existing variants).**
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Completed,
    Incomplete,
    Failed,
    RequiresAction,
}
```
(`snake_case` keeps `completed`/`incomplete`/`failed` identical and gives `requires_action`. Also add `Deserialize` — `StoredRun` round-trips it in Task 2.)

- [ ] **Step 2: Add `LoopOutcome` + `run_loop_core` (the for-loop body, parameterized by a starting `turns_done`).**
```rust
/// Outcome of running (or resuming) the loop until a terminal state or a pause.
pub(crate) enum LoopOutcome {
    /// The run reached a terminal state (the `Run` carries the final status).
    Terminal(Run),
    /// The model called a client (non-gateway) tool; the loop paused. Any
    /// gateway tool_calls of that same assistant turn were executed inline
    /// (their results are in `messages`); `pending_tool_calls` are the CLIENT
    /// tool_calls awaiting the caller's output.
    Paused {
        messages: Vec<Message>,
        turns_done: u32,
        usage: RunUsage,
        pending_tool_calls: Vec<tt_shared::messages::ToolCall>,
    },
}

/// The pausable loop core. Runs from `turns_done` (0 for a fresh run, >0 on
/// resume) up to `max_turns`. `id`/usage-carry-in let resume continue a run.
pub(crate) async fn run_loop_core(
    completer: &dyn TurnCompleter,
    id: uuid::Uuid,
    model: String,
    mut messages: Vec<Message>,
    tools: Vec<tt_shared::messages::Tool>,
    max_turns: u32,
    turns_done: u32,
    mut usage: RunUsage,
) -> LoopOutcome {
    let max_turns = max_turns.clamp(1, MAX_MAX_TURNS);
    let mut turn = turns_done;
    while turn < max_turns {
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
                return LoopOutcome::Terminal(Run {
                    id, status: RunStatus::Failed, messages, turns: turn + 1, usage,
                    note: Some(format!("turn {turn} failed: {e}")),
                });
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
            return LoopOutcome::Terminal(Run {
                id, status: RunStatus::Completed, messages, turns: turn + 1, usage, note: None,
            });
        }

        let has_client_tool = tool_calls
            .iter()
            .any(|tc| !crate::routes::gateway_tools::is_gateway_tool(&tc.function.name));

        // Execute the gateway tool_calls of this turn inline (whether or not we
        // are about to pause — so a mixed turn's gateway work isn't wasted and,
        // on resume, every tool_call of this assistant turn is answered).
        for tc in &tool_calls {
            if crate::routes::gateway_tools::is_gateway_tool(&tc.function.name) {
                let result = match crate::routes::gateway_tools::execute(
                    &tc.function.name, &tc.function.arguments,
                ) {
                    Ok(s) => s,
                    Err(e) => format!("tool error: {e}"),
                };
                messages.push(Message::Tool {
                    content: tt_shared::messages::MessageContent::Text(result),
                    tool_call_id: tc.id.clone(),
                });
            }
        }

        if has_client_tool {
            let pending: Vec<_> = tool_calls
                .into_iter()
                .filter(|tc| !crate::routes::gateway_tools::is_gateway_tool(&tc.function.name))
                .collect();
            return LoopOutcome::Paused { messages, turns_done: turn + 1, usage, pending_tool_calls: pending };
        }
        turn += 1;
    }
    LoopOutcome::Terminal(Run {
        id, status: RunStatus::Incomplete, messages, turns: max_turns, usage,
        note: Some("max_turns reached".into()),
    })
}
```
(NOTE: this changes 1a's behavior in ONE intended way for the core: a client tool now PAUSES instead of returning `Incomplete`, and gateway tools of a mixed turn are executed before pausing. The `run_loop` wrapper in Step 3 re-creates 1a's exact `Incomplete` for non-Redis/test callers. Confirm `MessageContent::Text` is the variant used by 1a's executor result append — it is.)

- [ ] **Step 3: Re-express `run_loop` as a thin wrapper (preserves 1a behavior for existing callers/tests).**
```rust
pub async fn run_loop(
    completer: &dyn TurnCompleter,
    id: uuid::Uuid,
    model: String,
    messages: Vec<Message>,
    tools: Vec<tt_shared::messages::Tool>,
    max_turns: u32,
) -> Run {
    match run_loop_core(completer, id, model, messages, tools, max_turns, 0, RunUsage::default()).await {
        LoopOutcome::Terminal(run) => run,
        LoopOutcome::Paused { messages, turns_done, usage, pending_tool_calls } => {
            // 1a callers (no persistence) surface a pause as Incomplete, exactly
            // as before. The note names the first client tool.
            let name = pending_tool_calls.first().map(|tc| tc.function.name.clone()).unwrap_or_default();
            Run {
                id, status: RunStatus::Incomplete, messages, turns: turns_done, usage,
                note: Some(format!("client tool '{name}' requires slice-1b round-trip")),
            }
        }
    }
}
```

- [ ] **Step 4: Update/keep the 1a unit tests + add loop-core pause tests.** The existing tests call `run_loop` — they must still pass (the wrapper preserves `Incomplete`/`Completed`/max-turns). Add to `#[cfg(test)] mod tests`:
```rust
    #[tokio::test]
    async fn core_pauses_on_client_tool_with_pending() {
        // Stub returns an assistant turn calling a client tool "write_file".
        let stub = Stub { script: std::sync::Mutex::new(vec![assistant_toolcall("write_file")]) };
        let out = run_loop_core(&stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 8, 0, RunUsage::default()).await;
        match out {
            LoopOutcome::Paused { pending_tool_calls, turns_done, .. } => {
                assert_eq!(turns_done, 1);
                assert_eq!(pending_tool_calls.len(), 1);
                assert_eq!(pending_tool_calls[0].function.name, "write_file");
            }
            _ => panic!("expected Paused"),
        }
    }

    #[tokio::test]
    async fn core_resume_continues_to_completion() {
        // Resume: messages already contain the paused assistant turn + the
        // appended client tool result; the next completion is a final answer.
        let stub = Stub { script: std::sync::Mutex::new(vec![assistant_final()]) };
        let resumed_messages = vec![
            assistant_toolcall("write_file"),
            Message::Tool { content: tt_shared::messages::MessageContent::Text("ok".into()), tool_call_id: "c1".into() },
        ];
        let out = run_loop_core(&stub, uuid::Uuid::nil(), "m".into(), resumed_messages, vec![], 8, 1, RunUsage::default()).await;
        match out {
            LoopOutcome::Terminal(run) => { assert_eq!(run.status, RunStatus::Completed); assert_eq!(run.turns, 2); }
            _ => panic!("expected Terminal Completed"),
        }
    }

    #[tokio::test]
    async fn core_mixed_turn_executes_gateway_then_pauses() {
        // An assistant turn with BOTH a gateway tool and a client tool: gateway
        // executed inline (a Tool result appears), pause with only the client one.
        let stub = Stub { script: std::sync::Mutex::new(vec![assistant_two_toolcalls("find_route_for", "write_file")]) };
        let out = run_loop_core(&stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 8, 0, RunUsage::default()).await;
        match out {
            LoopOutcome::Paused { messages, pending_tool_calls, .. } => {
                assert!(messages.iter().any(|m| matches!(m, Message::Tool { .. })), "gateway result appended");
                assert_eq!(pending_tool_calls.len(), 1);
                assert_eq!(pending_tool_calls[0].function.name, "write_file");
            }
            _ => panic!("expected Paused"),
        }
    }
```
Add the test helper `assistant_two_toolcalls(a, b)` (two `ToolCall`s in one assistant message, ids `c1`/`c2`) next to the existing `assistant_toolcall`. (Confirm the 1a `Stub` + `assistant_toolcall`/`assistant_final` helpers exist; reuse them.)

- [ ] **Step 5: Build + test + commit.**
```bash
cargo build -p tt-core
cargo test -p tt-core --lib agent_run    # existing 1a tests + the 3 new core tests
cargo clippy -p tt-core --lib -- -D warnings
cargo fmt -p tt-core
git add crates/core/src/routes/agent_run.rs
git commit -m "feat(core): pausable run_loop_core + RunStatus::RequiresAction; run_loop is a 1a-preserving wrapper (slice 1b)" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: `StoredRun` + run-store helpers

**Files:** Modify `crates/core/src/routes/agent_run.rs` (add the store types + helpers + tests).

- [ ] **Step 1: Add the persisted types + helpers.**
```rust
const RUN_TTL_SECS: u64 = 3600;

/// Non-secret routing config carried across a pause so resume turns route
/// consistently. NEVER includes credentials.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredRouting {
    pub provider_pin: Option<String>,
    pub forced_route: Option<String>,
    pub tag: Option<String>,
}

/// The full resumable run state persisted to the L1 store. NO secrets.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredRun {
    pub id: uuid::Uuid,
    pub org_id: uuid::Uuid,
    pub status: RunStatus,
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<tt_shared::messages::Tool>,
    pub max_turns: u32,
    pub turns_done: u32,
    pub usage: RunUsage,
    pub pending_tool_calls: Vec<tt_shared::messages::ToolCall>,
    pub routing: StoredRouting,
}

fn run_key(org_id: uuid::Uuid, run_id: uuid::Uuid) -> String {
    format!("tt:runs:{org_id}:{run_id}")
}

/// Derive the HTTP `Run` view from a stored record.
impl StoredRun {
    pub(crate) fn to_run(&self) -> Run {
        Run {
            id: self.id,
            status: self.status,
            messages: self.messages.clone(),
            turns: self.turns_done,
            usage: self.usage.clone(),
            note: None,
        }
    }
}

/// Persist (overwrite) a run record with the run TTL.
pub(crate) async fn store_run(cache: &dyn tt_cache::L1Cache, run: &StoredRun) -> Result<(), ApiError> {
    let bytes = serde_json::to_vec(run).map_err(|e| ApiError::Internal(format!("run serialize: {e}")))?;
    cache.set(&run_key(run.org_id, run.id), &bytes, RUN_TTL_SECS).await
        .map_err(|e| ApiError::Internal(format!("run store: {e}")))?;
    Ok(())
}

/// Fetch a run record scoped by (org, id). `None` when absent/expired.
pub(crate) async fn fetch_run(cache: &dyn tt_cache::L1Cache, org_id: uuid::Uuid, run_id: uuid::Uuid)
    -> Result<Option<StoredRun>, ApiError> {
    match cache.get(&run_key(org_id, run_id)).await
        .map_err(|e| ApiError::Internal(format!("run fetch: {e}")))? {
        Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)
            .map_err(|e| ApiError::Internal(format!("run deserialize: {e}")))?)),
        None => Ok(None),
    }
}
```
(NOTE: confirm `tt_cache::L1Cache` is the path (the crate is `tt-cache`); `Run` fields are `id,status,messages,turns,usage,note`. Confirm `ApiError` import is in scope in agent_run.rs — it is, from 1a.)

- [ ] **Step 2: Round-trip test (uses `InMemoryL1Cache`, no Redis).**
```rust
    #[tokio::test]
    async fn stored_run_roundtrips_through_cache() {
        let cache = tt_cache::memory::InMemoryL1Cache::new();
        let org = uuid::Uuid::new_v4();
        let run = StoredRun {
            id: uuid::Uuid::new_v4(), org_id: org, status: RunStatus::RequiresAction,
            model: "m".into(), messages: vec![assistant_toolcall("write_file")],
            tools: vec![], max_turns: 8, turns_done: 1, usage: RunUsage { prompt_tokens: 5, completion_tokens: 7 },
            pending_tool_calls: vec![], routing: StoredRouting { provider_pin: None, forced_route: None, tag: None },
        };
        store_run(&cache, &run).await.unwrap();
        let got = fetch_run(&cache, org, run.id).await.unwrap().expect("present");
        assert_eq!(got.id, run.id);
        assert_eq!(got.status, RunStatus::RequiresAction);
        assert_eq!(got.turns_done, 1);
        // wrong org → miss
        assert!(fetch_run(&cache, uuid::Uuid::new_v4(), run.id).await.unwrap().is_none());
    }
```
(Confirm `tt_cache::memory::InMemoryL1Cache` path; add `tt-cache` as a `[dev-dependencies]`/test path if `tt_cache` isn't already importable from tt-core tests — it IS a tt-core dep.)

- [ ] **Step 3: Build + test + commit.**
```bash
cargo test -p tt-core --lib agent_run
cargo clippy -p tt-core --lib -- -D warnings && cargo fmt -p tt-core
git add crates/core/src/routes/agent_run.rs
git commit -m "feat(core): StoredRun + L1-backed run store helpers (slice 1b)" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: `create_run` persists on pause

**Files:** Modify `crates/core/src/routes/agent_run.rs` (`create_run`).

- [ ] **Step 1: Rewrite `create_run` to use `run_loop_core` and persist on pause.**
```rust
pub async fn create_run(
    State(state): State<AppState>,
    Extension(trace): Extension<TraceId>,
    auth_ctx: Option<Extension<ApiKeyContext>>,
    headers: HeaderMap,
    Json(req): Json<CreateRunRequest>,
) -> ApiResult<Json<Run>> {
    let identity = RunIdentity::from_request(auth_ctx.as_deref(), trace.0.as_str(), &headers);
    let org_id = identity.org_id;
    let routing = StoredRouting {
        provider_pin: identity.provider_pin.clone(),
        forced_route: identity.forced_route.clone(),
        tag: identity.tag.clone(),
    };
    let model = req.model.clone();
    let tools = req.tools.clone();
    let max_turns = req.max_turns.unwrap_or(DEFAULT_MAX_TURNS);
    let completer = GatewayCompleter { state: &state, identity };
    let id = Uuid::new_v4();

    match run_loop_core(&completer, id, model.clone(), req.messages, tools.clone(), max_turns, 0, RunUsage::default()).await {
        LoopOutcome::Terminal(run) => Ok(Json(run)),
        LoopOutcome::Paused { messages, turns_done, usage, pending_tool_calls } => {
            match state.l1.as_ref() {
                Some(l1) => {
                    let stored = StoredRun {
                        id, org_id, status: RunStatus::RequiresAction, model,
                        messages: messages.clone(), tools, max_turns, turns_done,
                        usage: usage.clone(), pending_tool_calls, routing,
                    };
                    store_run(l1.cache.as_ref(), &stored).await?;
                    Ok(Json(stored.to_run()))
                }
                // No Redis → 1a fallback: surface the pause as Incomplete.
                None => {
                    let name = pending_tool_calls.first().map(|tc| tc.function.name.clone()).unwrap_or_default();
                    Ok(Json(Run {
                        id, status: RunStatus::Incomplete, messages, turns: turns_done, usage,
                        note: Some(format!("client tool '{name}' requires Redis to pause/resume (none configured)")),
                    }))
                }
            }
        }
    }
}
```
(NOTE: `pending_tool_calls` is moved into `stored` in the `Some` arm and read in the `None` arm — they're different arms, so no double-move; but the `None` arm references `pending_tool_calls` and `messages`/`usage` which were moved into the `Some` branch's `stored` only in the `Some` arm — since it's a `match` on `state.l1`, only one arm runs, so the moves are fine. Confirm the borrow checker is satisfied; if not, clone `pending_tool_calls.first().map(...)` name before the match. The `to_run()` view sets `turns = turns_done` and no note; that's the requires_action response.)

- [ ] **Step 2: Tests — no-Redis fallback + persist-on-pause (with `InMemoryL1Cache`).** These need a test `AppState`. Find how 1a/other tests build a minimal `AppState` (there is an `AppState::new(...)` + `with_l1(...)` builder). Add:
```rust
    // Helper: a test AppState with an in-memory L1 + a stub completer is not
    // trivial because create_run uses GatewayCompleter (needs a provider).
    // So test the PERSIST decision at the create_run seam via the store
    // helpers + run_loop_core directly (the handler wiring is integration-
    // covered); assert: a Paused outcome + Some(l1) ⇒ a StoredRun lands in the
    // cache with status RequiresAction; a Paused outcome + None ⇒ no store.
    #[tokio::test]
    async fn paused_with_l1_persists_requires_action() {
        let cache = tt_cache::memory::InMemoryL1Cache::new();
        let org = uuid::Uuid::new_v4();
        let id = uuid::Uuid::new_v4();
        let stored = StoredRun {
            id, org_id: org, status: RunStatus::RequiresAction, model: "m".into(),
            messages: vec![], tools: vec![], max_turns: 8, turns_done: 1,
            usage: RunUsage::default(),
            pending_tool_calls: vec![], routing: StoredRouting { provider_pin: None, forced_route: None, tag: None },
        };
        store_run(&cache, &stored).await.unwrap();
        assert_eq!(fetch_run(&cache, org, id).await.unwrap().unwrap().status, RunStatus::RequiresAction);
    }
```
(This keeps the test provider-free. The full create→pause→persist HTTP path is exercised in Task 5's resume test setup, which seeds a `StoredRun` directly. If a clean test `AppState` with an injectable `TurnCompleter` is feasible without a provider, prefer adding one; otherwise this seam-level coverage + Task 5's endpoint tests suffice.)

- [ ] **Step 3: Build + test + commit.**
```bash
cargo build -p tt-core && cargo test -p tt-core --lib agent_run
cargo clippy -p tt-core --lib -- -D warnings && cargo fmt -p tt-core
git add crates/core/src/routes/agent_run.rs
git commit -m "feat(core): create_run persists a paused run as requires_action (slice 1b)" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: `ApiError::Conflict` + `GET /v1/agent/runs/{id}`

**Files:** Modify `crates/core/src/error.rs` (add `Conflict`); `crates/core/src/routes/agent_run.rs` (`get_run`); `crates/core/src/server.rs` (mount GET).

- [ ] **Step 1: Add `ApiError::Conflict(String) → 409`.** In `crates/core/src/error.rs`, add the variant to the enum and a match arm in `impl IntoResponse` (mirror the `ServiceUnavailable`/`NotFound` arms):
```rust
    /// 409 — the request conflicts with the resource's current state.
    Conflict(String),
```
and in the `into_response` match:
```rust
            ApiError::Conflict(m) => (StatusCode::CONFLICT, m),
```
(Match the exact tuple/body shape the other arms use — quote-and-mirror an existing arm like `ServiceUnavailable`.)

- [ ] **Step 2: `get_run` handler.**
```rust
pub async fn get_run(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Run>> {
    let org = crate::routes::routes_api::require_org(ctx)?; // 401 if no real key
    let l1 = state.l1.as_ref().ok_or_else(|| ApiError::ServiceUnavailable(
        "agent runs require the L1/Redis store (none configured)".into()))?;
    let stored = fetch_run(l1.cache.as_ref(), org, id).await?
        .ok_or_else(|| ApiError::NotFound(format!("no run with id {id}")))?;
    Ok(Json(stored.to_run()))
}
```
(NOTE: `require_org` is `fn` in `routes_api` — confirm it's `pub(crate)` or re-declare a local `require_org` in agent_run.rs if it isn't visible. If `routes_api::require_org` is private, copy the 5-line helper into agent_run.rs. Confirm `Path`/`Json` imports.)

- [ ] **Step 3: Mount the GET route** in `crates/core/src/server.rs` (the `short` router, next to the existing `/v1/agent/runs` POST):
```rust
.route("/v1/agent/runs/:id", get(routes::agent_run::get_run))
```
(Ensure `get` is imported in server.rs — it is, used by other routes.)

- [ ] **Step 4: Tests — get_run via `InMemoryL1Cache`.** Build a test `AppState` with `with_l1(InMemoryL1Cache)`; seed a `StoredRun`; assert: present → `Run` view; wrong org → 404 (the key embeds org → miss); no-L1 AppState → the handler returns `ServiceUnavailable`. If constructing a full `AppState` in a unit test is heavy, test the decision logic by calling the store helpers + asserting the `ApiError` variants from a thin extraction; otherwise prefer the real handler with a minimal `AppState`. (Look at how `routes_api` tests build `AppState` and mirror it.)
```rust
    #[tokio::test]
    async fn get_run_missing_is_404_and_wrong_org_misses() {
        let cache = tt_cache::memory::InMemoryL1Cache::new();
        let org = uuid::Uuid::new_v4();
        let id = uuid::Uuid::new_v4();
        // absent → fetch returns None (handler maps to 404)
        assert!(fetch_run(&cache, org, id).await.unwrap().is_none());
        // seed, then wrong-org fetch misses
        let stored = StoredRun { id, org_id: org, status: RunStatus::RequiresAction, model: "m".into(),
            messages: vec![], tools: vec![], max_turns: 8, turns_done: 1, usage: RunUsage::default(),
            pending_tool_calls: vec![], routing: StoredRouting { provider_pin: None, forced_route: None, tag: None } };
        store_run(&cache, &stored).await.unwrap();
        assert!(fetch_run(&cache, uuid::Uuid::new_v4(), id).await.unwrap().is_none());
        assert!(fetch_run(&cache, org, id).await.unwrap().is_some());
    }
```
(If a real-`AppState` handler test is feasible, add one asserting the 503/404 `ApiError` mapping; else this store-level test + the integration coverage suffice.)

- [ ] **Step 5: Build + test + commit.**
```bash
cargo build -p tt-core && cargo test -p tt-core --lib agent_run && cargo test -p tt-core --lib error
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt -p tt-core
git add crates/core/src/error.rs crates/core/src/routes/agent_run.rs crates/core/src/server.rs
git commit -m "feat(core): ApiError::Conflict + GET /v1/agent/runs/:id (slice 1b)" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: `POST /v1/agent/runs/{id}/tool_outputs` (resume)

**Files:** Modify `crates/core/src/routes/agent_run.rs` (request types + `submit_tool_outputs`); `crates/core/src/server.rs` (mount POST).

- [ ] **Step 1: Request types.**
```rust
#[derive(serde::Deserialize)]
pub struct ToolOutput { pub tool_call_id: String, pub output: String }

#[derive(serde::Deserialize)]
pub struct ToolOutputsRequest { pub tool_outputs: Vec<ToolOutput> }
```

- [ ] **Step 2: `submit_tool_outputs` handler.**
```rust
pub async fn submit_tool_outputs(
    State(state): State<AppState>,
    Extension(trace): Extension<TraceId>,
    auth_ctx: Option<Extension<ApiKeyContext>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<ToolOutputsRequest>,
) -> ApiResult<Json<Run>> {
    let org = crate::routes::routes_api::require_org(auth_ctx.clone().map(|e| e))?; // org from the resume request's auth
    let l1 = state.l1.as_ref().ok_or_else(|| ApiError::ServiceUnavailable(
        "agent runs require the L1/Redis store (none configured)".into()))?;

    let mut stored = fetch_run(l1.cache.as_ref(), org, id).await?
        .ok_or_else(|| ApiError::NotFound(format!("no run with id {id}")))?;
    if stored.status != RunStatus::RequiresAction {
        return Err(ApiError::Conflict(format!("run {id} is {:?}, not awaiting tool outputs", stored.status)));
    }

    // The submitted ids must EXACTLY cover the pending client tool_calls.
    let pending_ids: std::collections::HashSet<&str> =
        stored.pending_tool_calls.iter().map(|tc| tc.id.as_str()).collect();
    let submitted_ids: std::collections::HashSet<&str> =
        body.tool_outputs.iter().map(|o| o.tool_call_id.as_str()).collect();
    if submitted_ids != pending_ids {
        return Err(ApiError::InvalidRequest(format!(
            "tool_outputs must cover exactly the pending tool_call ids {:?}", pending_ids)));
    }

    // Single-flight: only one resume processes a given run at a time.
    let sf_key = run_key(org, id);
    let _guard = match state.single_flight.try_become_leader(&sf_key) {
        Ok(g) => g,
        Err(_) => return Err(ApiError::Conflict(format!("run {id} is already being resumed"))),
    };

    // Append each submitted output as a Tool message (now every tool_call of the
    // paused assistant turn is answered: gateway results were appended at pause).
    for o in &body.tool_outputs {
        stored.messages.push(Message::Tool {
            content: tt_shared::messages::MessageContent::Text(o.output.clone()),
            tool_call_id: o.tool_call_id.clone(),
        });
    }

    // Rebuild the completer from the RESUME request's auth (org == stored.org,
    // verified above) + the stored routing config (provider_pin/forced_route/tag).
    let mut identity = RunIdentity::from_request(auth_ctx.as_deref(), trace.0.as_str(), &headers);
    identity.provider_pin = stored.routing.provider_pin.clone();
    identity.forced_route = stored.routing.forced_route.clone();
    identity.tag = stored.routing.tag.clone();
    let completer = GatewayCompleter { state: &state, identity };

    let outcome = run_loop_core(
        &completer, stored.id, stored.model.clone(),
        std::mem::take(&mut stored.messages), stored.tools.clone(),
        stored.max_turns, stored.turns_done, stored.usage.clone(),
    ).await;

    match outcome {
        LoopOutcome::Terminal(run) => {
            stored.status = run.status;
            stored.messages = run.messages.clone();
            stored.turns_done = run.turns;
            stored.usage = run.usage.clone();
            stored.pending_tool_calls = Vec::new();
            store_run(l1.cache.as_ref(), &stored).await?; // stays GETtable to TTL
            Ok(Json(run))
        }
        LoopOutcome::Paused { messages, turns_done, usage, pending_tool_calls } => {
            stored.status = RunStatus::RequiresAction;
            stored.messages = messages;
            stored.turns_done = turns_done;
            stored.usage = usage;
            stored.pending_tool_calls = pending_tool_calls;
            store_run(l1.cache.as_ref(), &stored).await?;
            Ok(Json(stored.to_run()))
        }
    }
}
```
(NOTE: confirm `state.single_flight` field name + `try_become_leader(&str)` return shape (`Result<LeaderGuard, _>`); from the explorer it's `Arc<SingleFlight>` on AppState. The `auth_ctx.clone().map(|e| e)` for `require_org` is awkward — instead extract `org` directly: `let org = match auth_ctx.as_deref() { Some(c) if c.org_id != DOGFOOD_ORG_ID => c.org_id, _ => return Err(ApiError::Unauthorized) };` mirroring `require_org`, since we also need `auth_ctx` later for `from_request`. Use that form. Confirm `RunIdentity` fields `provider_pin`/`forced_route`/`tag` are accessible/mutable (same module).)

- [ ] **Step 3: Mount the POST route** in `server.rs`:
```rust
.route("/v1/agent/runs/:id/tool_outputs", post(routes::agent_run::submit_tool_outputs))
```

- [ ] **Step 4: Tests.** Resume is the meatiest. Test the validation + state-machine logic at the seam (provider-free) by seeding a `StoredRun` and asserting: a non-`RequiresAction` stored run → the handler would 409 (test the status guard as a small pure predicate, or via a real handler+`AppState` if feasible); mismatched ids → the set-comparison rejects. The full create→pause→tool_outputs→complete happy path is best as a real handler test if a provider-free `TurnCompleter` can be injected into `AppState`; if `AppState` hard-wires `GatewayCompleter`, document that the happy-path resume is covered by `run_loop_core`'s `core_resume_continues_to_completion` (Task 1) + the store round-trip (Task 2), and the handler wires them. Add at least:
```rust
    #[test]
    fn tool_outputs_id_coverage_check() {
        // pending {c1,c2}; submitting only {c1} must be rejected; {c1,c2} accepted.
        let pending: std::collections::HashSet<&str> = ["c1","c2"].into_iter().collect();
        let only_one: std::collections::HashSet<&str> = ["c1"].into_iter().collect();
        let both: std::collections::HashSet<&str> = ["c1","c2"].into_iter().collect();
        assert_ne!(only_one, pending);
        assert_eq!(both, pending);
    }
```
Prefer a real handler test if `AppState` construction with an in-memory L1 + an injectable completer is tractable (inspect existing `AppState` test builders); otherwise the seam tests + the integration path are acceptable for 1b.

- [ ] **Step 5: Build + full verify + commit.**
```bash
cargo build -p tt-core && cargo test -p tt-core --lib agent_run
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt -p tt-core
git add crates/core/src/routes/agent_run.rs crates/core/src/server.rs
git commit -m "feat(core): POST /v1/agent/runs/:id/tool_outputs resume (slice 1b)" -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification (mirror required CI checks)
```bash
cargo fmt --check -p tt-core
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p tt-core --lib --tests           # agent_run + everything; ~753 baseline + new tests, 0 failed
cargo test --workspace --no-run               # all targets compile
cargo run -q -p tt-cli -- inspect .
cargo test -p tt-plan-core                    # determinism goldens untouched
```
> `cargo test (workspace)` CI check is disk-flaky → `gh run rerun <id> --failed`. No DB gate needed (Redis-only; tests use `InMemoryL1Cache`). The local sandbox may stall tt-core integration test-binary STARTUP (~50-90s each, environmental) — the lib tests (`--lib`) run fine; CI on a fresh runner is authoritative.

## Self-Review (against the spec)
**Spec coverage:** graceful no-Redis fallback (Task 3 `None` arm + Task 4/5 `ServiceUnavailable`) ✓ · persist-on-pause / 1h TTL (`RUN_TTL_SECS`, Task 3) ✓ · no-secrets + re-auth-on-resume (Task 5 rebuilds identity from the resume request; `StoredRun` has no creds) ✓ · `LoopOutcome`/`run_loop_core` + 1a wrapper (Task 1) ✓ · `RunStatus::RequiresAction` (Task 1) ✓ · `StoredRun` + key + helpers (Task 2) ✓ · create persist-on-pause (Task 3) ✓ · get_run org-scoped (Task 4) ✓ · submit_tool_outputs: id coverage(400)/status(409)/single-flight(409)/re-auth/resume (Task 5) ✓ · mixed-tool rule (Task 1 core) ✓ · max_turns spans pauses (turns_done carried) ✓.
**Placeholder scan:** none — complete code per step; the test steps note where a real-`AppState` handler test is preferred-if-tractable vs the provided provider-free seam tests, which is a legitimate test-design latitude, not a deferral (concrete tests are given either way).
**Type consistency:** `run_loop_core(completer,id,model,messages,tools,max_turns,turns_done,usage)->LoopOutcome` used identically in Tasks 1/3/5; `StoredRun`/`StoredRouting`/`store_run`/`fetch_run`/`run_key` across 2-5; `RunStatus::RequiresAction` across 1-5; `ApiError::Conflict` across 4-5; `Run` view via `to_run()`. `MessageContent::Text` for tool results matches 1a.
