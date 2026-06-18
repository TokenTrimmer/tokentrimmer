//! Server-side agentic loop (slice 1a): run model->tool->model over the
//! read-only gateway tools until a final answer or `max_turns`. Synchronous;
//! no Redis/no client round-trip (slice 1b). Generic over `TurnCompleter` so
//! tests inject a stub.

use async_trait::async_trait;
use tt_shared::messages::{ChatCompletionRequest, Message, MessageContent};

use crate::error::ApiError;

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
/// Consumed by the `POST /v1/agent/runs` handler (slice 1a Task 4); the
/// narrow `dead_code` allow scopes the unused-until-then warning to this one
/// item rather than the whole module.
#[allow(dead_code)]
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
}
