//! Unit tests for request/response translation.
//! Uses insta snapshots for golden output verification.

use std::collections::HashMap;

use tt_provider_compat::translate::{extract_usage, translate_request};
use tt_shared::{
    messages::{
        ContentPart, ImageUrl, Message, MessageContent, ResponseFormat, Tool, ToolCall,
        ToolCallFunction, ToolChoice, ToolChoiceFunction, ToolFunction,
    },
    ChatCompletionRequest,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_request(model: &str, messages: Vec<Message>) -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: model.to_string(),
        messages,
        temperature: Some(1.0),
        top_p: None,
        max_tokens: Some(1024),
        stream: false,
        tools: vec![],
        tool_choice: None,
        response_format: None,
        stop: vec![],
        presence_penalty: None,
        frequency_penalty: None,
        n: None,
        seed: None,
        user: None,
        tt_extras: HashMap::new(),
    }
}

fn user_text(text: &str) -> Message {
    Message::User {
        content: MessageContent::Text(text.to_string()),
        name: None,
    }
}

fn assistant_text(text: &str) -> Message {
    Message::Assistant {
        content: Some(MessageContent::Text(text.to_string())),
        tool_calls: vec![],
        name: None,
    }
}

fn system_text(text: &str) -> Message {
    Message::System {
        content: MessageContent::Text(text.to_string()),
    }
}

// ---------------------------------------------------------------------------
// 1. Text-only single-turn
// ---------------------------------------------------------------------------

#[test]
fn translate_text_only_single_turn() {
    let req = make_request("gpt-4o", vec![user_text("What is 2+2?")]);
    let body = translate_request(req).expect("translate ok");

    insta::assert_json_snapshot!("text_only_single_turn", &body);
}

// ---------------------------------------------------------------------------
// 2. Multi-turn with system + user + assistant + tool messages
// ---------------------------------------------------------------------------

#[test]
fn translate_multi_turn_with_system() {
    let messages = vec![
        system_text("You are a helpful assistant."),
        user_text("Call the weather tool for London."),
        Message::Assistant {
            content: None,
            tool_calls: vec![ToolCall {
                id: "call_abc123".to_string(),
                r#type: "function".to_string(),
                function: ToolCallFunction {
                    name: "get_weather".to_string(),
                    arguments: r#"{"location":"London"}"#.to_string(),
                },
            }],
            name: None,
        },
        Message::Tool {
            content: MessageContent::Text(r#"{"temp":15,"unit":"C"}"#.to_string()),
            tool_call_id: "call_abc123".to_string(),
        },
        assistant_text("The weather in London is 15°C."),
        user_text("Thanks!"),
    ];

    let req = make_request("gpt-4o", messages);
    let body = translate_request(req).expect("translate ok");

    insta::assert_json_snapshot!("multi_turn_with_system", &body);
}

// ---------------------------------------------------------------------------
// 3. Tools / function-calling shape
// ---------------------------------------------------------------------------

#[test]
fn translate_tools_function_calling() {
    let tool = Tool {
        r#type: "function".to_string(),
        function: ToolFunction {
            name: "search_web".to_string(),
            description: Some("Search the web for current information.".to_string()),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query" }
                },
                "required": ["query"]
            }),
        },
    };

    let mut req = make_request("gpt-4o", vec![user_text("Search for Rust news.")]);
    req.tools = vec![tool];
    req.tool_choice = Some(ToolChoice::Specific {
        r#type: "function".to_string(),
        function: ToolChoiceFunction {
            name: "search_web".to_string(),
        },
    });

    let body = translate_request(req).expect("translate ok");
    insta::assert_json_snapshot!("tools_function_calling", &body);
}

// ---------------------------------------------------------------------------
// 4. response_format json_schema
// ---------------------------------------------------------------------------

#[test]
fn translate_response_format_json_schema() {
    let mut req = make_request("gpt-4o", vec![user_text("Give me a JSON object.")]);
    req.response_format = Some(ResponseFormat {
        r#type: "json_schema".to_string(),
        json_schema: Some(serde_json::json!({
            "name": "answer",
            "schema": {
                "type": "object",
                "properties": { "value": { "type": "string" } },
                "required": ["value"]
            }
        })),
    });

    let body = translate_request(req).expect("translate ok");
    insta::assert_json_snapshot!("response_format_json_schema", &body);
}

// ---------------------------------------------------------------------------
// 5. Reasoning model (o3): max_tokens → max_completion_tokens + temperature drop
// ---------------------------------------------------------------------------

#[test]
fn translate_reasoning_model_o3() {
    let mut req = make_request("o3", vec![user_text("Prove P=NP.")]);
    req.temperature = Some(0.9); // should be dropped
    req.max_tokens = Some(8192); // should become max_completion_tokens

    let body = translate_request(req).expect("translate ok");

    // temperature must be absent
    assert!(
        body.temperature.is_none(),
        "temperature should be stripped for reasoning models"
    );
    // max_tokens must be absent
    assert!(
        body.max_tokens.is_none(),
        "max_tokens should be absent for reasoning models"
    );
    // max_completion_tokens must be set
    assert_eq!(
        body.max_completion_tokens,
        Some(8192),
        "max_completion_tokens should carry the value"
    );

    insta::assert_json_snapshot!("reasoning_model_o3", &body);
}

// ---------------------------------------------------------------------------
// 6. tt_extras stripping
// ---------------------------------------------------------------------------

#[test]
fn translate_tt_extras_stripped() {
    let mut req = make_request("gpt-4o", vec![user_text("Hello.")]);
    req.tt_extras
        .insert("route_hint".to_string(), serde_json::json!("us-east-1"));
    req.tt_extras.insert(
        "cache_strategy".to_string(),
        serde_json::json!({"ttl": 3600}),
    );

    let body = translate_request(req).expect("translate ok");
    let serialized = serde_json::to_string(&body).expect("serialize ok");

    assert!(
        !serialized.contains("tt_extras"),
        "tt_extras key must not appear in serialized body"
    );
    assert!(
        !serialized.contains("route_hint"),
        "tt_extras values must not appear in serialized body"
    );
    assert!(
        !serialized.contains("cache_strategy"),
        "tt_extras values must not appear in serialized body"
    );

    insta::assert_json_snapshot!("tt_extras_stripped", &body);
}

// ---------------------------------------------------------------------------
// 7. o4-mini reasoning model
// ---------------------------------------------------------------------------

#[test]
fn translate_reasoning_model_o4_mini() {
    let mut req = make_request("o4-mini", vec![user_text("Solve this integral.")]);
    req.temperature = Some(0.5);
    req.max_tokens = Some(50_000);

    let body = translate_request(req).expect("translate ok");

    assert!(body.temperature.is_none());
    assert!(body.max_tokens.is_none());
    assert_eq!(body.max_completion_tokens, Some(50_000));

    insta::assert_json_snapshot!("reasoning_model_o4_mini", &body);
}

// ---------------------------------------------------------------------------
// 8. Multimodal — vision content parts
// ---------------------------------------------------------------------------

#[test]
fn translate_vision_content_parts() {
    let messages = vec![Message::User {
        content: MessageContent::Parts(vec![
            ContentPart::Text {
                text: "What's in this image?".to_string(),
            },
            ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "https://example.com/cat.jpg".to_string(),
                    detail: Some("high".to_string()),
                },
            },
        ]),
        name: None,
    }];

    let req = make_request("gpt-4o", messages);
    let body = translate_request(req).expect("translate ok");

    insta::assert_json_snapshot!("vision_content_parts", &body);
}

// ---------------------------------------------------------------------------
// 9. Usage extraction — cached_tokens present
// ---------------------------------------------------------------------------

#[test]
fn usage_with_cached_tokens() {
    let raw = serde_json::json!({
        "usage": {
            "prompt_tokens": 200,
            "completion_tokens": 100,
            "total_tokens": 300,
            "prompt_tokens_details": {
                "cached_tokens": 150
            }
        }
    });

    let usage = extract_usage(&raw).expect("extract ok");
    assert_eq!(usage.prompt_tokens, 200);
    assert_eq!(usage.completion_tokens, 100);
    assert_eq!(usage.total_tokens, 300);
    assert_eq!(usage.cached_tokens, 150);
}

// ---------------------------------------------------------------------------
// 10. Usage extraction — cached_tokens absent
// ---------------------------------------------------------------------------

#[test]
fn usage_without_cached_tokens_defaults_zero() {
    let raw = serde_json::json!({
        "usage": {
            "prompt_tokens": 50,
            "completion_tokens": 25,
            "total_tokens": 75
        }
    });

    let usage = extract_usage(&raw).expect("extract ok");
    assert_eq!(usage.cached_tokens, 0);
}
