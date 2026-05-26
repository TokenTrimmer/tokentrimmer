//! Unit tests for Anthropic request/response translation.
//! Uses insta snapshots for golden output verification.

use std::collections::HashMap;

use tt_provider_anthropic::translate::{
    deserialize_response, translate_request, translate_usage, AnthropicUsage,
};
use tt_shared::{
    messages::{
        ContentPart, ImageUrl, Message, MessageContent, Tool, ToolCall, ToolCallFunction,
        ToolChoice, ToolChoiceFunction, ToolFunction,
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
        temperature: Some(0.7),
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
    let req = make_request("claude-sonnet-4-6", vec![user_text("What is 2+2?")]);
    let body = translate_request(req).expect("translate ok");

    // System should be absent for a pure user message.
    assert!(body.system.is_none());
    assert_eq!(body.messages.len(), 1);
    assert_eq!(body.messages[0].role, "user");

    insta::assert_json_snapshot!("text_only_single_turn", &body);
}

// ---------------------------------------------------------------------------
// 2. Multi-turn with system + user + assistant
// ---------------------------------------------------------------------------

#[test]
fn translate_multi_turn_with_system() {
    let messages = vec![
        system_text("You are a helpful assistant."),
        user_text("Hello!"),
        assistant_text("Hi there!"),
        user_text("How are you?"),
    ];

    let req = make_request("claude-sonnet-4-6", messages);
    let body = translate_request(req).expect("translate ok");

    // System extracted; only user/assistant in messages.
    assert!(body.system.is_some());
    assert_eq!(body.messages.len(), 3);
    assert_eq!(body.messages[0].role, "user");
    assert_eq!(body.messages[1].role, "assistant");
    assert_eq!(body.messages[2].role, "user");

    insta::assert_json_snapshot!("multi_turn_with_system", &body);
}

// ---------------------------------------------------------------------------
// 3. System message extraction — system field set, removed from messages
// ---------------------------------------------------------------------------

#[test]
fn translate_system_message_extraction() {
    let messages = vec![
        system_text("You are a pirate."),
        system_text("Speak in pirate dialect."),
        user_text("Hello!"),
    ];

    let req = make_request("claude-sonnet-4-6", messages);
    let body = translate_request(req).expect("translate ok");

    let system = body.system.as_ref().expect("system should be present");
    assert_eq!(system.len(), 2);
    assert_eq!(system[0].text, "You are a pirate.");
    assert_eq!(system[1].text, "Speak in pirate dialect.");

    // Conversation messages contain only the user message.
    assert_eq!(body.messages.len(), 1);
    assert_eq!(body.messages[0].role, "user");

    insta::assert_json_snapshot!("system_message_extraction", &body);
}

// ---------------------------------------------------------------------------
// 4. Auto cache_control on long system (~1500 tokens)
// ---------------------------------------------------------------------------

#[test]
fn translate_long_system_gets_cache_control() {
    // 1500 tokens × 4 chars/token ≈ 6000 characters.
    let long_text = "word ".repeat(1500);
    let messages = vec![
        Message::System {
            content: MessageContent::Text(long_text),
        },
        user_text("Hi"),
    ];

    let req = make_request("claude-sonnet-4-6", messages);
    let body = translate_request(req).expect("translate ok");

    let system = body.system.as_ref().expect("system should be present");
    assert!(
        system[0].cache_control.is_some(),
        "long system block should have cache_control"
    );
    let cc = system[0].cache_control.as_ref().unwrap();
    assert_eq!(cc.ctype, "ephemeral");

    insta::assert_json_snapshot!("long_system_cache_control", &body);
}

// ---------------------------------------------------------------------------
// 5. Short system — NO cache_control
// ---------------------------------------------------------------------------

#[test]
fn translate_short_system_no_cache_control() {
    let messages = vec![
        system_text("Be helpful."),
        user_text("Hi"),
    ];

    let req = make_request("claude-sonnet-4-6", messages);
    let body = translate_request(req).expect("translate ok");

    let system = body.system.as_ref().expect("system should be present");
    assert!(
        system[0].cache_control.is_none(),
        "short system block must not have cache_control"
    );

    insta::assert_json_snapshot!("short_system_no_cache_control", &body);
}

// ---------------------------------------------------------------------------
// 6. max_tokens default when omitted
// ---------------------------------------------------------------------------

#[test]
fn translate_max_tokens_defaults_to_4096() {
    let mut req = make_request("claude-sonnet-4-6", vec![user_text("Hello")]);
    req.max_tokens = None;

    let body = translate_request(req).expect("translate ok");
    assert_eq!(body.max_tokens, 4096);

    insta::assert_json_snapshot!("max_tokens_default", &body);
}

// ---------------------------------------------------------------------------
// 7. Tools translation: function → name + input_schema
// ---------------------------------------------------------------------------

#[test]
fn translate_tools_function_format() {
    let tool = Tool {
        r#type: "function".to_string(),
        function: ToolFunction {
            name: "get_weather".to_string(),
            description: Some("Get the current weather.".to_string()),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "location": { "type": "string" }
                },
                "required": ["location"]
            }),
        },
    };

    let mut req = make_request("claude-sonnet-4-6", vec![user_text("What's the weather?")]);
    req.tools = vec![tool];

    let body = translate_request(req).expect("translate ok");

    assert_eq!(body.tools.len(), 1);
    assert_eq!(body.tools[0].name, "get_weather");
    assert_eq!(
        body.tools[0].description.as_deref(),
        Some("Get the current weather.")
    );
    // `input_schema` holds what was `parameters`.
    assert_eq!(body.tools[0].input_schema["type"], "object");

    insta::assert_json_snapshot!("tools_translation", &body);
}

// ---------------------------------------------------------------------------
// 8. tool_choice translation: specific function → {type: "tool", name}
// ---------------------------------------------------------------------------

#[test]
fn translate_tool_choice_specific() {
    let mut req = make_request("claude-sonnet-4-6", vec![user_text("Search for me.")]);
    req.tool_choice = Some(ToolChoice::Specific {
        r#type: "function".to_string(),
        function: ToolChoiceFunction {
            name: "search_web".to_string(),
        },
    });

    let body = translate_request(req).expect("translate ok");

    // Verify the JSON representation includes type: "tool" and the name.
    let serialized = serde_json::to_value(&body).expect("serialize ok");
    let tc = &serialized["tool_choice"];
    assert_eq!(tc["type"], "tool");
    assert_eq!(tc["name"], "search_web");

    insta::assert_json_snapshot!("tool_choice_specific", &body);
}

// ---------------------------------------------------------------------------
// 9. tool_use response translation: Anthropic content blocks → OpenAI tool_calls
// ---------------------------------------------------------------------------

#[test]
fn translate_tool_use_response() {
    let body = r#"{
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6-20251022",
        "content": [
            {
                "type": "tool_use",
                "id": "toolu_01",
                "name": "get_weather",
                "input": {"location": "London"}
            }
        ],
        "stop_reason": "tool_use",
        "stop_sequence": null,
        "usage": {
            "input_tokens": 50,
            "output_tokens": 20,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0
        }
    }"#;

    let resp = deserialize_response(body).expect("deserialize ok");

    assert_eq!(resp.id, "msg_01");
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("tool_calls"));

    // The message should have tool_calls in OpenAI format.
    match &resp.choices[0].message {
        tt_shared::messages::Message::Assistant { tool_calls, .. } => {
            assert_eq!(tool_calls.len(), 1);
            assert_eq!(tool_calls[0].id, "toolu_01");
            assert_eq!(tool_calls[0].function.name, "get_weather");
            // Arguments must be stringified JSON.
            let args: serde_json::Value =
                serde_json::from_str(&tool_calls[0].function.arguments).expect("valid json");
            assert_eq!(args["location"], "London");
        }
        other => panic!("expected Assistant message, got {other:?}"),
    }

    insta::assert_json_snapshot!(
        "tool_use_response",
        &resp,
        { ".created" => "[timestamp]" }
    );
}

// ---------------------------------------------------------------------------
// 10. tool_result message translation: Tool → Anthropic user message
// ---------------------------------------------------------------------------

#[test]
fn translate_tool_result_message() {
    let messages = vec![
        user_text("What's the weather?"),
        Message::Assistant {
            content: None,
            tool_calls: vec![ToolCall {
                id: "toolu_01".to_string(),
                r#type: "function".to_string(),
                function: ToolCallFunction {
                    name: "get_weather".to_string(),
                    arguments: r#"{"location":"London"}"#.to_string(),
                },
            }],
            name: None,
        },
        Message::Tool {
            content: MessageContent::Text(r#"{"temperature":15,"unit":"C"}"#.to_string()),
            tool_call_id: "toolu_01".to_string(),
        },
        user_text("Thank you!"),
    ];

    let req = make_request("claude-sonnet-4-6", messages);
    let body = translate_request(req).expect("translate ok");

    // Tool result becomes a user message with tool_result content block.
    // Message order: user, assistant, user (tool_result), user.
    assert_eq!(body.messages.len(), 4);
    assert_eq!(body.messages[2].role, "user");

    insta::assert_json_snapshot!("tool_result_message", &body);
}

// ---------------------------------------------------------------------------
// 11. Usage round-trip with cache_read_input_tokens
// ---------------------------------------------------------------------------

#[test]
fn usage_round_trip_with_cache_fields() {
    let u = AnthropicUsage {
        input_tokens: 200,
        output_tokens: 80,
        cache_creation_input_tokens: Some(50),
        cache_read_input_tokens: Some(150),
    };

    let usage = translate_usage(u);

    assert_eq!(usage.prompt_tokens, 200);
    assert_eq!(usage.completion_tokens, 80);
    assert_eq!(usage.total_tokens, 280);
    assert_eq!(usage.cached_tokens, 150);
    assert_eq!(usage.cache_creation_input_tokens, Some(50));
}

// ---------------------------------------------------------------------------
// 12. tt_extras stripping
// ---------------------------------------------------------------------------

#[test]
fn translate_tt_extras_stripped() {
    let mut req = make_request("claude-sonnet-4-6", vec![user_text("Hello.")]);
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
// 13. (Bonus) Multimodal image_url → Anthropic image source
// ---------------------------------------------------------------------------

#[test]
fn translate_multimodal_image_url() {
    let messages = vec![Message::User {
        content: MessageContent::Parts(vec![
            ContentPart::Text {
                text: "Describe this image.".to_string(),
            },
            ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "https://example.com/cat.jpg".to_string(),
                    detail: None,
                },
            },
        ]),
        name: None,
    }];

    let req = make_request("claude-sonnet-4-6", messages);
    let body = translate_request(req).expect("translate ok");

    // Should have two content blocks: text + image.
    assert_eq!(body.messages[0].content.len(), 2);

    insta::assert_json_snapshot!("multimodal_image_url", &body);
}
