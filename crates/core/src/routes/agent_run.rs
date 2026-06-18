//! Server-side agentic loop (slice 1a): run model->tool->model over the
//! read-only gateway tools until a final answer or `max_turns`. Synchronous;
//! no Redis/no client round-trip (slice 1b). Generic over `TurnCompleter` so
//! tests inject a stub.

use async_trait::async_trait;
use axum::{
    extract::{Extension, State},
    http::HeaderMap,
    Json,
};
use tt_auth::ApiKeyContext;
use tt_shared::{
    context::{ProviderCredentials, SecretString},
    messages::{ChatCompletionRequest, Message, MessageContent},
    RequestContext,
};
use uuid::Uuid;

use crate::{
    error::ApiError,
    middleware::trace::TraceId,
    routes::chat::{self, CompletionOutcome},
    ApiResult, AppState,
};

/// Terminal status of a run.
///
/// `Completed` = the model returned a final (tool-call-free) answer.
/// `Incomplete` = the loop stopped without a final answer (an unknown/client
/// tool requires a slice-1b round-trip, or `max_turns` was reached).
/// `Failed` = a completion turn errored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Completed,
    Incomplete,
    Failed,
}

/// Accumulated token usage across every turn of a run.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct RunUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

/// The result of running the agent loop. The full message transcript is
/// returned so the caller sees the model/tool exchange.
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

/// One completion turn. Production impl wraps `prepare` + `complete_once`
/// (slice 1a Task 4); tests inject a stub. Returns the assistant message +
/// usage for the turn.
#[async_trait]
pub trait TurnCompleter: Send + Sync {
    async fn complete(&self, req: ChatCompletionRequest) -> Result<(Message, RunUsage), ApiError>;
}

/// Default cap on completion turns when the caller does not specify one.
///
/// Consumed by the `POST /v1/agent/runs` handler ([`create_run`]).
pub(crate) const DEFAULT_MAX_TURNS: u32 = 8;
/// Hard upper bound on completion turns regardless of the caller's request.
const MAX_MAX_TURNS: u32 = 32;

/// Run the synchronous agent loop. `model`/`messages`/`tools` come from the
/// request; `max_turns` is clamped to `[1, 32]`.
///
/// Each turn builds a non-streaming [`ChatCompletionRequest`], calls
/// `completer.complete`, appends the assistant message and accumulates usage.
/// If the assistant returns no tool calls the run is `Completed`. If any tool
/// call is not a gateway-executable read-only tool the run is `Incomplete`
/// (slice 1b round-trips it). Otherwise each gateway tool is executed and its
/// result appended as a [`Message::Tool`] before the next turn. A completer
/// error ends the run as `Failed`; exhausting `max_turns` ends it `Incomplete`.
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
                return Run {
                    id,
                    status: RunStatus::Failed,
                    messages,
                    turns: turn + 1,
                    usage,
                    note: Some(format!("turn {turn} failed: {e}")),
                };
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
            return Run {
                id,
                status: RunStatus::Completed,
                messages,
                turns: turn + 1,
                usage,
                note: None,
            };
        }
        // Partition: every tool_call must be gateway-executable in 1a. A single
        // non-gateway (client) tool ends the run as `Incomplete` — slice 1b
        // round-trips it to the caller.
        for tc in &tool_calls {
            if !crate::routes::gateway_tools::is_gateway_tool(&tc.function.name) {
                return Run {
                    id,
                    status: RunStatus::Incomplete,
                    messages,
                    turns: turn + 1,
                    usage,
                    note: Some(format!(
                        "client tool '{}' requires slice-1b round-trip",
                        tc.function.name
                    )),
                };
            }
        }
        for tc in &tool_calls {
            let result = match crate::routes::gateway_tools::execute(
                &tc.function.name,
                &tc.function.arguments,
            ) {
                Ok(s) => s,
                // A tool error is appended as the tool result (not aborted) so
                // the model can read it and react on the next turn.
                Err(e) => format!("tool error: {e}"),
            };
            messages.push(Message::Tool {
                content: MessageContent::Text(result),
                tool_call_id: tc.id.clone(),
            });
        }
    }
    Run {
        id,
        status: RunStatus::Incomplete,
        messages,
        turns: max_turns,
        usage,
        note: Some("max_turns reached".into()),
    }
}

// ---------------------------------------------------------------------------
// Production completer + `POST /v1/agent/runs` endpoint (slice 1a Task 4)
// ---------------------------------------------------------------------------

/// Run-level caller identity captured once at run creation. Every per-turn
/// completion re-derives the same `RequestContext` + ~16 `prepare` inputs the
/// chat handler builds post-auth (provider + credentials are RE-RESOLVED per
/// turn since routing/the model can change between turns), so each turn routes
/// exactly as a single-shot `/v1/chat/completions` would for that turn's model.
struct RunIdentity {
    /// Caller's org (nil for anonymous/dev), from `ApiKeyContext`.
    org_id: Uuid,
    /// Caller's API key id (nil for anonymous/dev), from `ApiKeyContext`.
    api_key_id: Uuid,
    /// Caller tier (drives L2 entitlement + cache TTL). `None` ⇒ treated Free.
    caller_tier: Option<tt_shared::CallerTier>,
    /// L2 entitlement (paid-tier only) — derived once from `caller_tier`.
    l2_allowed: bool,
    /// Raw bearer (the source provider's key for the legacy passthrough path;
    /// also the cross-provider re-emit credential).
    raw_bearer: String,
    /// Resolved trace id (stable across the run's turns).
    trace_id: Uuid,
    /// `X-TokenTrimmer-Tag` cost-attribution tag, if any.
    tag: Option<String>,
    /// Per-request upstream deadline (`X-TokenTrimmer-Timeout-Ms`), if any.
    request_timeout: Option<std::time::Duration>,
    /// `X-TokenTrimmer-Provider` pin, applied per turn after routing.
    provider_pin: Option<String>,
    /// `X-TokenTrimmer-Route` forced route, passed into routing per turn.
    forced_route: Option<String>,
    /// Sticky-canary idempotency key (stable across the run's turns).
    idempotency_key: String,
    /// The caller's request headers with `X-TokenTrimmer-Cache` STRIPPED, so the
    /// per-turn `tt_extras.cache=bypass` knob is never re-enabled by a header
    /// override (header beats body in `prepare`). All other headers — provider
    /// pin, forced route, timeout, tag, interactive — flow through unchanged.
    headers: HeaderMap,
}

impl RunIdentity {
    /// Build the run-level identity from the auth context + headers, mirroring
    /// the chat handler's post-auth setup (`chat::handler` §2 / §2b). The
    /// sandbox `tt_test_*` short-circuit is intentionally NOT replicated here —
    /// an agent run always drives the real per-turn completion pipeline.
    fn from_request(auth_ctx: Option<&ApiKeyContext>, trace: &str, headers: &HeaderMap) -> Self {
        let raw_bearer = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| {
                s.strip_prefix("Bearer ")
                    .or_else(|| s.strip_prefix("bearer "))
            })
            .unwrap_or("")
            .to_string();

        // Trace id: the trace-middleware extension wins, else a fresh v7. (The
        // chat handler also accepts an `x-tokentrimmer-trace-id` header; the run
        // endpoint inherits that via the same middleware-populated `TraceId`.)
        let trace_id = if !trace.is_empty() {
            Uuid::parse_str(trace).unwrap_or_else(|_| Uuid::now_v7())
        } else {
            headers
                .get("x-tokentrimmer-trace-id")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| Uuid::parse_str(s).ok())
                .unwrap_or_else(Uuid::now_v7)
        };

        let idempotency_key = headers
            .get("x-idempotency-key")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if trace_id != Uuid::nil() {
                    trace_id.to_string()
                } else {
                    Uuid::now_v7().to_string()
                }
            });

        let (org_id, api_key_id, caller_tier) = match auth_ctx {
            Some(c) => (c.org_id, c.key_id, c.tier),
            None => (Uuid::nil(), Uuid::nil(), None),
        };
        let l2_allowed = matches!(
            caller_tier,
            Some(
                tt_shared::CallerTier::Pro
                    | tt_shared::CallerTier::Team
                    | tt_shared::CallerTier::Scale
            )
        );

        // Strip the cache-override header so the per-turn `tt_extras.cache=bypass`
        // is authoritative (`prepare` lets `X-TokenTrimmer-Cache` override the
        // body decision; without this a `read-only`/`force-write` header could
        // re-enable a lookup/insert mid-loop).
        let mut headers = headers.clone();
        headers.remove("x-tokentrimmer-cache");

        Self {
            org_id,
            api_key_id,
            caller_tier,
            l2_allowed,
            raw_bearer,
            trace_id,
            tag: headers
                .get("x-tokentrimmer-tag")
                .and_then(|v| v.to_str().ok())
                .map(String::from),
            request_timeout: chat::timeout_ms_from_header(&headers)
                .map(std::time::Duration::from_millis),
            provider_pin: chat::provider_override_from_header(&headers),
            forced_route: chat::route_override_from_header(&headers),
            idempotency_key,
            headers,
        }
    }
}

/// Production completer: routes + dispatches each turn through the real
/// `chat::prepare` + `chat::complete_once` pipeline (per-turn routing / cache /
/// telemetry), exactly as a single-shot non-streaming `/v1/chat/completions`
/// would for that turn's model.
struct GatewayCompleter<'a> {
    state: &'a AppState,
    /// Run-level caller identity. Per-turn the completer builds a FRESH
    /// `RequestContext` from these (never reusing a `prepare`-rebound ctx), so a
    /// cross-provider / provider-pin credential rebind from one turn can never
    /// leak into the next turn's routing.
    identity: RunIdentity,
}

impl GatewayCompleter<'_> {
    /// Disable L1/L2 cache lookup AND insert for a per-turn request by setting
    /// `tt_extras.cache = {"mode":"bypass"}` (→ `CacheBehavior::resolve` yields
    /// `do_lookup=false, do_insert=false`). An agent turn is a fresh, evolving
    /// transcript — serving a cached answer mid-loop would be wrong — so
    /// `complete_once` always returns `Dispatched` and never the `CacheHit` arm
    /// (the headers carrying the cache override are also stripped in
    /// [`RunIdentity::from_request`], so a header can't re-enable caching).
    fn disable_cache(req: &mut ChatCompletionRequest) {
        req.tt_extras
            .insert("cache".to_string(), serde_json::json!({ "mode": "bypass" }));
    }
}

#[async_trait]
impl TurnCompleter for GatewayCompleter<'_> {
    async fn complete(
        &self,
        mut req: ChatCompletionRequest,
    ) -> Result<(Message, RunUsage), ApiError> {
        Self::disable_cache(&mut req);

        // Per-turn provider resolution: the model can change between turns
        // (routing / a tool-driven downgrade), so resolve fresh from THIS turn's
        // `req.model`, mirroring the chat handler's step 1.
        let provider =
            self.state
                .registry
                .resolve(&req.model)
                .ok_or_else(|| ApiError::ModelNotFound {
                    model: req.model.clone(),
                })?;
        let source_provider_id = provider.id().to_string();

        // Per-turn credentials, resolved against THIS turn's provider exactly as
        // the chat handler does (store hit wins; anonymous BYO bearer fallback;
        // verified-org miss fails closed → deferred error inside `prepare`).
        let resolved_source_creds = chat::resolve_credentials(
            self.state,
            self.identity.org_id,
            provider.id(),
            &self.identity.raw_bearer,
        )
        .await;
        let source_creds_missing = resolved_source_creds.is_none();
        let credentials = resolved_source_creds.unwrap_or_else(|| ProviderCredentials {
            api_key: SecretString::new(self.identity.raw_bearer.clone()),
            base_url: None,
            extra_headers: Vec::new(),
        });

        // FRESH per-turn context built from the run-level identity. Cloning the
        // base identity (not a prior turn's rebound `ctx`) guarantees a
        // cross-provider credential rebind inside `prepare` never leaks forward.
        let mut ctx = RequestContext {
            trace_id: self.identity.trace_id,
            org_id: self.identity.org_id,
            api_key_id: self.identity.api_key_id,
            credentials,
            tag: self.identity.tag.clone(),
            deadline: self.identity.request_timeout,
        };

        let request_started = std::time::Instant::now();
        let prep = chat::prepare(
            self.state,
            &mut ctx,
            &mut req,
            &self.identity.headers,
            provider,
            self.identity.provider_pin.clone(),
            self.identity.forced_route.clone(),
            self.identity.request_timeout,
            self.identity.idempotency_key.clone(),
            self.identity.raw_bearer.clone(),
            self.identity.org_id,
            source_provider_id,
            source_creds_missing,
            self.identity.caller_tier,
            self.identity.l2_allowed,
            Default::default(),
            request_started,
        )
        .await?;

        match chat::complete_once(self.state, &ctx, prep).await? {
            CompletionOutcome::Dispatched { response, .. } => {
                let usage = RunUsage {
                    prompt_tokens: response.usage.prompt_tokens,
                    completion_tokens: response.usage.completion_tokens,
                };
                let msg = response
                    .choices
                    .into_iter()
                    .next()
                    .map(|c| c.message)
                    .ok_or_else(|| {
                        ApiError::Internal("agent turn: provider returned no choices".into())
                    })?;
                Ok((msg, usage))
            }
            // Unreachable in practice — every per-turn request disables the cache
            // (lookup + insert), so `complete_once` always dispatches. Treat a
            // cache hit as an internal invariant violation rather than silently
            // mishandling the prebuilt HTTP `Response` (which the loop can't turn
            // back into a typed `Message`).
            CompletionOutcome::CacheHit(_) => Err(ApiError::Internal(
                "agent turn unexpectedly served from cache (cache should be disabled per turn)"
                    .into(),
            )),
        }
    }
}

/// Request body for `POST /v1/agent/runs`.
#[derive(serde::Deserialize)]
pub struct CreateRunRequest {
    /// Model id for every turn (routing may rewrite it per turn).
    pub model: String,
    /// Initial transcript (system/user messages).
    pub messages: Vec<Message>,
    /// Tool definitions advertised to the model. Defaults to none.
    #[serde(default)]
    pub tools: Vec<tt_shared::messages::Tool>,
    /// Turn cap; clamped to `[1, 32]`. Defaults to [`DEFAULT_MAX_TURNS`].
    #[serde(default)]
    pub max_turns: Option<u32>,
}

/// `POST /v1/agent/runs` — run a synchronous server-side agent loop
/// (model→tool→model over the read-only gateway tools) until a final answer or
/// `max_turns`. Auth is inherited from the router's auth middleware (the
/// `ApiKeyContext` extension); identity + credentials are built per the chat
/// handler's post-auth setup and forwarded per turn.
pub async fn create_run(
    State(state): State<AppState>,
    Extension(trace): Extension<TraceId>,
    auth_ctx: Option<Extension<ApiKeyContext>>,
    headers: HeaderMap,
    Json(req): Json<CreateRunRequest>,
) -> ApiResult<Json<Run>> {
    let identity = RunIdentity::from_request(auth_ctx.as_deref(), trace.0.as_str(), &headers);
    let completer = GatewayCompleter {
        state: &state,
        identity,
    };
    let id = Uuid::new_v4();
    let run = run_loop(
        &completer,
        id,
        req.model,
        req.messages,
        req.tools,
        req.max_turns.unwrap_or(DEFAULT_MAX_TURNS),
    )
    .await;
    Ok(Json(run))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted completer: each call pops the next assistant message from the
    /// script. Lets the loop be exercised with no provider and no DB.
    struct Stub {
        script: std::sync::Mutex<Vec<Message>>,
    }

    #[async_trait]
    impl TurnCompleter for Stub {
        async fn complete(
            &self,
            _req: ChatCompletionRequest,
        ) -> Result<(Message, RunUsage), ApiError> {
            let mut s = self.script.lock().unwrap();
            Ok((
                s.remove(0),
                RunUsage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                },
            ))
        }
    }

    fn assistant_final() -> Message {
        Message::Assistant {
            content: Some(MessageContent::Text("done".into())),
            tool_calls: vec![],
            name: None,
        }
    }

    fn assistant_toolcall(name: &str) -> Message {
        Message::Assistant {
            content: None,
            name: None,
            tool_calls: vec![tt_shared::messages::ToolCall {
                id: "c1".into(),
                r#type: "function".into(),
                function: tt_shared::messages::ToolCallFunction {
                    name: name.into(),
                    arguments: r#"{"task_description":"x"}"#.into(),
                },
            }],
        }
    }

    #[tokio::test]
    async fn completes_on_final_answer() {
        let stub = Stub {
            script: std::sync::Mutex::new(vec![assistant_final()]),
        };
        let run = run_loop(&stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 8).await;
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(run.turns, 1);
    }

    #[tokio::test]
    async fn gateway_tool_turn_then_final() {
        let stub = Stub {
            script: std::sync::Mutex::new(vec![
                assistant_toolcall("find_route_for"),
                assistant_final(),
            ]),
        };
        let run = run_loop(&stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 8).await;
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(run.turns, 2);
        // transcript carries the tool result between the two assistant turns
        assert!(run
            .messages
            .iter()
            .any(|m| matches!(m, Message::Tool { .. })));
    }

    #[tokio::test]
    async fn unknown_tool_is_incomplete() {
        let stub = Stub {
            script: std::sync::Mutex::new(vec![assistant_toolcall("write_file")]),
        };
        let run = run_loop(&stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 8).await;
        assert_eq!(run.status, RunStatus::Incomplete);
        assert!(run.note.unwrap().contains("write_file"));
    }

    #[tokio::test]
    async fn max_turns_bound() {
        // always returns a (gateway) tool call → never completes
        let script: Vec<Message> = (0..10)
            .map(|_| assistant_toolcall("find_route_for"))
            .collect();
        let stub = Stub {
            script: std::sync::Mutex::new(script),
        };
        let run = run_loop(&stub, uuid::Uuid::nil(), "m".into(), vec![], vec![], 3).await;
        assert_eq!(run.status, RunStatus::Incomplete);
        assert_eq!(run.turns, 3);
    }

    // ----- Task 4 wiring (no provider, no DB) -----

    #[test]
    fn disable_cache_sets_bypass_mode() {
        // The per-turn cache-disable knob must parse to `CacheMode::Bypass`,
        // which `CacheBehavior::resolve` maps to `do_lookup=false,
        // do_insert=false` — so `complete_once` always returns `Dispatched`.
        let mut req = ChatCompletionRequest::default();
        GatewayCompleter::disable_cache(&mut req);
        let cfg = tt_shared::parse_cache_control(&req.tt_extras)
            .expect("cache knob present after disable_cache");
        assert_eq!(cfg.mode, tt_shared::CacheMode::Bypass);
    }

    #[test]
    fn run_identity_strips_cache_override_header() {
        // A caller-supplied `X-TokenTrimmer-Cache` header must be stripped so it
        // cannot re-enable lookups/inserts mid-loop (header beats body in
        // `prepare`). Non-cache headers (e.g. the tag) survive.
        let mut headers = HeaderMap::new();
        headers.insert("x-tokentrimmer-cache", "force-write".parse().unwrap());
        headers.insert("x-tokentrimmer-tag", "proj-x".parse().unwrap());
        let id = RunIdentity::from_request(None, "", &headers);
        assert!(!id.headers.contains_key("x-tokentrimmer-cache"));
        assert_eq!(id.tag.as_deref(), Some("proj-x"));
        // Anonymous caller → nil org/key, no L2 entitlement.
        assert_eq!(id.org_id, Uuid::nil());
        assert!(!id.l2_allowed);
    }

    #[test]
    fn run_identity_carries_paid_tier_l2() {
        let ctx = ApiKeyContext {
            key_id: Uuid::from_u128(1),
            org_id: Uuid::from_u128(2),
            tier: Some(tt_shared::CallerTier::Pro),
        };
        let id = RunIdentity::from_request(Some(&ctx), "", &HeaderMap::new());
        assert_eq!(id.org_id, Uuid::from_u128(2));
        assert_eq!(id.api_key_id, Uuid::from_u128(1));
        assert!(id.l2_allowed);
    }
}
