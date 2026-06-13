//! `POST /v1/responses` — stateless OpenAI Responses API ingress.
//!
//! The gateway's canonical execution pipeline is still OpenAI Chat Completions.
//! This handler accepts the common stateless Responses API shape, translates it
//! into a [`ChatCompletionRequest`], dispatches through the same chat handler as
//! `/v1/chat/completions`, then reshapes successful JSON responses into the
//! Responses object shape.

use std::collections::HashMap;

use axum::{
    body::{Body, Bytes},
    extract::{Extension, State},
    http::{header, HeaderMap},
    response::Response,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tt_auth::ApiKeyContext;
use tt_shared::{
    messages::{
        ImageUrl, InputAudio, ResponseFormat, ToolCallFunction, ToolChoiceFunction, ToolFunction,
    },
    ChatCompletionRequest, ChatCompletionResponse, ContentPart, Message, MessageContent, Tool,
    ToolCall, ToolChoice, Usage,
};

use crate::{middleware::trace::TraceId, routes::chat, ApiError, ApiResult, AppState};

/// Handler for `POST /v1/responses`.
pub async fn handler(
    State(state): State<AppState>,
    Extension(trace): Extension<TraceId>,
    auth_ctx: Option<Extension<ApiKeyContext>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let inbound: ResponsesRequest =
        serde_json::from_slice(&body).map_err(|e| ApiError::InvalidRequest(e.to_string()))?;
    let chat_req = inbound.into_chat_request()?;

    let chat_resp = chat::handler(
        State(state),
        Extension(trace),
        auth_ctx,
        None,
        headers,
        Json(chat_req),
    )
    .await?;

    transcode_json_response(chat_resp).await
}

#[derive(Debug, Deserialize)]
struct ResponsesRequest {
    model: String,
    #[serde(default)]
    input: Option<ResponsesInput>,
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    top_p: Option<f32>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    #[serde(default)]
    tools: Vec<ResponsesTool>,
    #[serde(default)]
    tool_choice: Option<ResponsesToolChoice>,
    #[serde(default)]
    text: Option<ResponsesTextConfig>,
    #[serde(default)]
    stop: Option<ResponsesStop>,
    #[serde(default)]
    presence_penalty: Option<f32>,
    #[serde(default)]
    frequency_penalty: Option<f32>,
    #[serde(default)]
    n: Option<u32>,
    #[serde(default)]
    seed: Option<i64>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default)]
    parallel_tool_calls: Option<bool>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    reasoning: Option<Value>,
    #[serde(default)]
    store: Option<bool>,
    #[serde(default)]
    previous_response_id: Option<String>,
    #[serde(default)]
    conversation: Option<Value>,
    #[serde(default)]
    background: Option<bool>,
    #[serde(default)]
    include: Option<Value>,
    #[serde(flatten, default)]
    extra: HashMap<String, Value>,
}

impl ResponsesRequest {
    fn into_chat_request(self) -> ApiResult<ChatCompletionRequest> {
        self.validate_supported()?;

        let mut messages = Vec::new();
        if let Some(instructions) = self.instructions {
            messages.push(Message::System {
                content: MessageContent::Text(instructions),
            });
        }
        if let Some(input) = self.input {
            messages.extend(input.into_messages()?);
        }
        if messages.is_empty() {
            return Err(ApiError::InvalidRequest(
                "responses input is required for this stateless bridge".to_string(),
            ));
        }

        let mut extra = HashMap::new();
        if let Some(metadata) = self.metadata {
            extra.insert("metadata".to_string(), metadata);
        }
        for (key, value) in self.extra {
            if is_chat_passthrough_field(&key) {
                extra.insert(key, value);
            } else if !value.is_null() {
                return Err(ApiError::InvalidRequest(format!(
                    "unsupported /v1/responses field for stateless bridge: {key}"
                )));
            }
        }

        Ok(ChatCompletionRequest {
            model: self.model,
            messages,
            temperature: self.temperature,
            top_p: self.top_p,
            max_tokens: self.max_output_tokens,
            max_completion_tokens: None,
            stream: false,
            stream_options: None,
            tools: self
                .tools
                .into_iter()
                .map(ResponsesTool::into_chat_tool)
                .collect::<ApiResult<_>>()?,
            tool_choice: self
                .tool_choice
                .map(ResponsesToolChoice::into_chat_tool_choice)
                .transpose()?,
            response_format: self
                .text
                .map(ResponsesTextConfig::into_chat_response_format)
                .transpose()?
                .flatten(),
            stop: self.stop.map(ResponsesStop::into_vec).unwrap_or_default(),
            presence_penalty: self.presence_penalty,
            frequency_penalty: self.frequency_penalty,
            n: self.n,
            seed: self.seed,
            user: self.user,
            parallel_tool_calls: self.parallel_tool_calls,
            reasoning_effort: self
                .reasoning_effort
                .or_else(|| reasoning_effort(self.reasoning)),
            tt_extras: HashMap::new(),
            extra,
        })
    }

    fn validate_supported(&self) -> ApiResult<()> {
        if self.stream {
            return Err(ApiError::InvalidRequest(
                "streaming /v1/responses is not supported yet; use /v1/chat/completions for SSE"
                    .to_string(),
            ));
        }
        if self.store == Some(true) {
            return Err(ApiError::InvalidRequest(
                "stateful /v1/responses store is not supported; set store=false".to_string(),
            ));
        }
        if self.previous_response_id.is_some() {
            return Err(ApiError::InvalidRequest(
                "previous_response_id is not supported by the stateless /v1/responses bridge"
                    .to_string(),
            ));
        }
        if self.conversation.as_ref().is_some_and(non_null) {
            return Err(ApiError::InvalidRequest(
                "conversation state is not supported by the stateless /v1/responses bridge"
                    .to_string(),
            ));
        }
        if self.background == Some(true) {
            return Err(ApiError::InvalidRequest(
                "background /v1/responses jobs are not supported by this gateway".to_string(),
            ));
        }
        if self.include.as_ref().is_some_and(non_empty_value) {
            return Err(ApiError::InvalidRequest(
                "include expansions are not supported by the /v1/responses bridge".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ResponsesInput {
    Text(String),
    Items(Vec<ResponsesInputItem>),
}

impl ResponsesInput {
    fn into_messages(self) -> ApiResult<Vec<Message>> {
        match self {
            ResponsesInput::Text(text) => Ok(vec![Message::User {
                content: MessageContent::Text(text),
                name: None,
            }]),
            ResponsesInput::Items(items) => items
                .into_iter()
                .map(ResponsesInputItem::into_message)
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ResponsesInputItem {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<ResponsesContent>,
    #[serde(rename = "type", default)]
    item_type: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
    #[serde(default)]
    output: Option<ResponsesToolOutput>,
}

impl ResponsesInputItem {
    fn into_message(self) -> ApiResult<Message> {
        match self.item_type.as_deref() {
            Some("function_call_output") => {
                let tool_call_id = self.call_id.or(self.id).ok_or_else(|| {
                    ApiError::InvalidRequest(
                        "function_call_output input requires call_id".to_string(),
                    )
                })?;
                let content = self
                    .output
                    .map(ResponsesToolOutput::into_string)
                    .ok_or_else(|| {
                        ApiError::InvalidRequest(
                            "function_call_output input requires output".to_string(),
                        )
                    })?;
                return Ok(Message::Tool {
                    content: MessageContent::Text(content),
                    tool_call_id,
                });
            }
            Some("function_call") => {
                let id = self.call_id.or(self.id).ok_or_else(|| {
                    ApiError::InvalidRequest("function_call input requires call_id".to_string())
                })?;
                let name = self.name.ok_or_else(|| {
                    ApiError::InvalidRequest("function_call input requires name".to_string())
                })?;
                return Ok(Message::Assistant {
                    content: None,
                    tool_calls: vec![ToolCall {
                        id,
                        r#type: "function".to_string(),
                        function: ToolCallFunction {
                            name,
                            arguments: self.arguments.unwrap_or_else(|| "{}".to_string()),
                        },
                    }],
                    name: None,
                });
            }
            Some("message") | None => {}
            Some(other) => {
                return Err(ApiError::InvalidRequest(format!(
                    "unsupported /v1/responses input item type: {other}"
                )));
            }
        }

        let role = self.role.as_deref().unwrap_or("user");
        match role {
            "developer" | "system" => Ok(Message::System {
                content: required_content(self.content, role)?,
            }),
            "user" => Ok(Message::User {
                content: required_content(self.content, role)?,
                name: None,
            }),
            "assistant" => Ok(Message::Assistant {
                content: self
                    .content
                    .map(ResponsesContent::into_message_content)
                    .transpose()?,
                tool_calls: Vec::new(),
                name: None,
            }),
            "tool" => {
                let tool_call_id = self.call_id.or(self.id).ok_or_else(|| {
                    ApiError::InvalidRequest("tool input requires call_id".to_string())
                })?;
                Ok(Message::Tool {
                    content: required_content(self.content, role)?,
                    tool_call_id,
                })
            }
            other => Err(ApiError::InvalidRequest(format!(
                "unsupported /v1/responses input role: {other}"
            ))),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ResponsesContent {
    Text(String),
    Parts(Vec<ResponsesContentPart>),
}

impl ResponsesContent {
    fn into_message_content(self) -> ApiResult<MessageContent> {
        match self {
            ResponsesContent::Text(text) => Ok(MessageContent::Text(text)),
            ResponsesContent::Parts(parts) => {
                let parts = parts
                    .into_iter()
                    .map(ResponsesContentPart::into_chat_part)
                    .collect::<ApiResult<Vec<_>>>()?;
                Ok(MessageContent::Parts(parts))
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct ResponsesContentPart {
    #[serde(rename = "type")]
    part_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    image_url: Option<ResponsesImageUrl>,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    input_audio: Option<InputAudio>,
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    format: Option<String>,
}

impl ResponsesContentPart {
    fn into_chat_part(self) -> ApiResult<ContentPart> {
        match self.part_type.as_str() {
            "input_text" | "output_text" | "text" => Ok(ContentPart::Text {
                text: self.text.ok_or_else(|| {
                    ApiError::InvalidRequest(format!(
                        "{} content part requires text",
                        self.part_type
                    ))
                })?,
            }),
            "input_image" | "image_url" => {
                let image_url = self.image_url.ok_or_else(|| {
                    ApiError::InvalidRequest(format!(
                        "{} content part requires image_url",
                        self.part_type
                    ))
                })?;
                Ok(ContentPart::ImageUrl {
                    image_url: image_url.into_chat_image_url(self.detail),
                })
            }
            "input_audio" => {
                let input_audio = self
                    .input_audio
                    .or_else(|| match (self.data, self.format) {
                        (Some(data), Some(format)) => Some(InputAudio { data, format }),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        ApiError::InvalidRequest(
                            "input_audio content part requires input_audio or data+format"
                                .to_string(),
                        )
                    })?;
                Ok(ContentPart::InputAudio { input_audio })
            }
            other => Err(ApiError::InvalidRequest(format!(
                "unsupported /v1/responses content part type: {other}"
            ))),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ResponsesImageUrl {
    Url(String),
    Object {
        url: String,
        #[serde(default)]
        detail: Option<String>,
    },
}

impl ResponsesImageUrl {
    fn into_chat_image_url(self, fallback_detail: Option<String>) -> ImageUrl {
        match self {
            ResponsesImageUrl::Url(url) => ImageUrl {
                url,
                detail: fallback_detail,
            },
            ResponsesImageUrl::Object { url, detail } => ImageUrl {
                url,
                detail: detail.or(fallback_detail),
            },
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ResponsesToolOutput {
    Text(String),
    Json(Value),
}

impl ResponsesToolOutput {
    fn into_string(self) -> String {
        match self {
            ResponsesToolOutput::Text(text) => text,
            ResponsesToolOutput::Json(value) => value.to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ResponsesTool {
    #[serde(rename = "type")]
    r#type: String,
    #[serde(default)]
    function: Option<ToolFunction>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: Option<Value>,
}

impl ResponsesTool {
    fn into_chat_tool(self) -> ApiResult<Tool> {
        if self.r#type != "function" {
            return Err(ApiError::InvalidRequest(format!(
                "unsupported /v1/responses tool type: {}",
                self.r#type
            )));
        }

        let function = match self.function {
            Some(function) => function,
            None => ToolFunction {
                name: self.name.ok_or_else(|| {
                    ApiError::InvalidRequest("function tool requires name".to_string())
                })?,
                description: self.description,
                parameters: self
                    .parameters
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            },
        };

        Ok(Tool {
            r#type: "function".to_string(),
            function,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ResponsesToolChoice {
    String(String),
    Function {
        #[serde(rename = "type")]
        r#type: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        function: Option<ToolChoiceFunction>,
    },
}

impl ResponsesToolChoice {
    fn into_chat_tool_choice(self) -> ApiResult<ToolChoice> {
        match self {
            ResponsesToolChoice::String(choice) => Ok(ToolChoice::Auto(choice)),
            ResponsesToolChoice::Function {
                r#type,
                name,
                function,
            } => {
                if r#type != "function" {
                    return Err(ApiError::InvalidRequest(format!(
                        "unsupported /v1/responses tool_choice type: {}",
                        r#type
                    )));
                }
                let name = function
                    .map(|function| function.name)
                    .or(name)
                    .ok_or_else(|| {
                        ApiError::InvalidRequest("function tool_choice requires name".to_string())
                    })?;
                Ok(ToolChoice::function(name))
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct ResponsesTextConfig {
    #[serde(default)]
    format: Option<Value>,
}

impl ResponsesTextConfig {
    fn into_chat_response_format(self) -> ApiResult<Option<ResponseFormat>> {
        let Some(mut format) = self.format else {
            return Ok(None);
        };
        let Some(format_type) = format.get("type").and_then(Value::as_str) else {
            return Err(ApiError::InvalidRequest(
                "text.format requires a type".to_string(),
            ));
        };

        match format_type {
            "text" => Ok(None),
            "json_object" => Ok(Some(ResponseFormat {
                r#type: "json_object".to_string(),
                json_schema: None,
            })),
            "json_schema" => {
                let json_schema = format.get("json_schema").cloned().or_else(|| {
                    format
                        .as_object_mut()
                        .map(|obj| {
                            obj.remove("type");
                            Value::Object(obj.clone())
                        })
                        .filter(non_empty_value)
                });
                Ok(Some(ResponseFormat {
                    r#type: "json_schema".to_string(),
                    json_schema,
                }))
            }
            other => Err(ApiError::InvalidRequest(format!(
                "unsupported text.format type: {other}"
            ))),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ResponsesStop {
    One(String),
    Many(Vec<String>),
}

impl ResponsesStop {
    fn into_vec(self) -> Vec<String> {
        match self {
            ResponsesStop::One(stop) => vec![stop],
            ResponsesStop::Many(stop) => stop,
        }
    }
}

async fn transcode_json_response(resp: Response) -> ApiResult<Response> {
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .map_err(|e| ApiError::Internal(format!("failed to read chat response body: {e}")))?;

    if !parts.status.is_success() || is_error_body(&bytes) {
        return Ok(Response::from_parts(parts, Body::from(bytes)));
    }

    let chat: ChatCompletionResponse = serde_json::from_slice(&bytes)
        .map_err(|e| ApiError::Internal(format!("failed to parse chat response body: {e}")))?;
    let responses = chat_response_to_responses_json(&chat);
    let new_body = serde_json::to_vec(&responses)
        .map_err(|e| ApiError::Internal(format!("failed to serialize Responses body: {e}")))?;

    let mut out = Response::from_parts(parts, Body::from(new_body));
    out.headers_mut().remove(header::CONTENT_LENGTH);
    if let Ok(ct) = "application/json".parse() {
        out.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    Ok(out)
}

fn chat_response_to_responses_json(chat: &ChatCompletionResponse) -> Value {
    let response_id = response_id(&chat.id);
    let mut output = Vec::new();
    let mut output_text = String::new();
    let mut finish_reason = None;

    for choice in &chat.choices {
        finish_reason = finish_reason.or_else(|| choice.finish_reason.clone());
        if let Message::Assistant {
            content,
            tool_calls,
            ..
        } = &choice.message
        {
            let text = content
                .as_ref()
                .map(message_content_text)
                .unwrap_or_default();
            if !text.is_empty() {
                if !output_text.is_empty() {
                    output_text.push('\n');
                }
                output_text.push_str(&text);
                output.push(json!({
                    "id": format!("msg_{}_{}", chat.id, choice.index),
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": text,
                        "annotations": []
                    }]
                }));
            }

            for call in tool_calls {
                output.push(json!({
                    "id": call.id,
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.function.name,
                    "arguments": call.function.arguments,
                    "status": "completed"
                }));
            }
        }
    }

    if output.is_empty() {
        output.push(json!({
            "id": format!("msg_{}_0", chat.id),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": []
        }));
    }

    let status = match finish_reason.as_deref() {
        Some("length") => "incomplete",
        Some("content_filter") => "incomplete",
        _ => "completed",
    };

    json!({
        "id": response_id,
        "object": "response",
        "created_at": chat.created,
        "status": status,
        "model": chat.model,
        "output": output,
        "output_text": output_text,
        "usage": responses_usage(&chat.usage),
    })
}

fn responses_usage(usage: &Usage) -> Value {
    json!({
        "input_tokens": usage.prompt_tokens,
        "output_tokens": usage.completion_tokens,
        "total_tokens": usage.total_tokens,
    })
}

fn response_id(chat_id: &str) -> String {
    if chat_id.starts_with("resp_") {
        chat_id.to_string()
    } else {
        format!("resp_{chat_id}")
    }
}

fn message_content_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                ContentPart::ImageUrl { .. } | ContentPart::InputAudio { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn required_content(content: Option<ResponsesContent>, role: &str) -> ApiResult<MessageContent> {
    content
        .ok_or_else(|| ApiError::InvalidRequest(format!("{role} input requires content")))?
        .into_message_content()
}

fn reasoning_effort(reasoning: Option<Value>) -> Option<String> {
    reasoning?
        .get("effort")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn is_chat_passthrough_field(key: &str) -> bool {
    matches!(
        key,
        "logit_bias" | "logprobs" | "top_logprobs" | "service_tier"
    )
}

fn is_error_body(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|v| v.get("error").cloned())
        .is_some()
}

fn non_null(value: &Value) -> bool {
    !value.is_null()
}

fn non_empty_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_input_maps_to_chat_messages() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-4o-mini",
            "instructions": "Be terse.",
            "input": "Summarize this",
            "max_output_tokens": 128,
            "reasoning": { "effort": "low" }
        }))
        .unwrap();

        let chat = req.into_chat_request().unwrap();
        assert_eq!(chat.model, "gpt-4o-mini");
        assert_eq!(chat.max_tokens, Some(128));
        assert_eq!(chat.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(chat.messages.len(), 2);
        assert!(matches!(
            &chat.messages[0],
            Message::System {
                content: MessageContent::Text(text)
            } if text == "Be terse."
        ));
        assert!(matches!(
            &chat.messages[1],
            Message::User {
                content: MessageContent::Text(text),
                name: None
            } if text == "Summarize this"
        ));
    }

    #[test]
    fn item_input_maps_text_images_and_tool_outputs() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-4o",
            "input": [
                { "role": "developer", "content": "Use metric units." },
                {
                    "role": "user",
                    "content": [
                        { "type": "input_text", "text": "Describe this." },
                        { "type": "input_image", "image_url": "https://example.com/a.png", "detail": "low" }
                    ]
                },
                { "type": "function_call_output", "call_id": "call_1", "output": { "ok": true } }
            ],
            "tools": [{
                "type": "function",
                "name": "lookup",
                "description": "Lookup a value",
                "parameters": { "type": "object", "properties": {} }
            }],
            "tool_choice": { "type": "function", "name": "lookup" }
        }))
        .unwrap();

        let chat = req.into_chat_request().unwrap();
        assert_eq!(chat.messages.len(), 3);
        assert_eq!(chat.tools[0].function.name, "lookup");
        assert!(matches!(
            &chat.tool_choice,
            Some(ToolChoice::Specific { .. })
        ));
        assert!(matches!(&chat.messages[0], Message::System { .. }));
        assert!(matches!(
            &chat.messages[1],
            Message::User {
                content: MessageContent::Parts(parts),
                ..
            } if parts.len() == 2
        ));
        assert!(matches!(
            &chat.messages[2],
            Message::Tool {
                tool_call_id,
                content: MessageContent::Text(text)
            } if tool_call_id == "call_1" && text == r#"{"ok":true}"#
        ));
    }

    #[test]
    fn chat_response_maps_to_responses_json() {
        let chat = ChatCompletionResponse {
            id: "chatcmpl_123".to_string(),
            object: "chat.completion".to_string(),
            created: 1_716_598_234,
            model: "gpt-4o-mini".to_string(),
            choices: vec![tt_shared::Choice {
                index: 0,
                message: Message::Assistant {
                    content: Some(MessageContent::Text("Hello there".to_string())),
                    tool_calls: Vec::new(),
                    name: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 3,
                total_tokens: 13,
                ..Usage::default()
            },
        };

        let response = chat_response_to_responses_json(&chat);
        assert_eq!(response["id"], "resp_chatcmpl_123");
        assert_eq!(response["object"], "response");
        assert_eq!(response["status"], "completed");
        assert_eq!(response["output_text"], "Hello there");
        assert_eq!(response["usage"]["input_tokens"], 10);
        assert_eq!(response["usage"]["output_tokens"], 3);
    }

    #[test]
    fn stateful_fields_are_rejected() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-4o-mini",
            "input": "hi",
            "previous_response_id": "resp_old"
        }))
        .unwrap();

        assert!(matches!(
            req.into_chat_request(),
            Err(ApiError::InvalidRequest(message))
                if message.contains("previous_response_id")
        ));
    }
}
