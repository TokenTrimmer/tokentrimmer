//! Unit tests for Gemini request/response translation.
//! Uses insta snapshots for golden output verification.

use std::collections::HashMap;

use tt_provider_gemini::translate::{
    deserialize_response, translate_request, translate_usage, GeminiUsageMetadata,
};
use tt_shared::{
    messages::{
        ContentPart, DocumentPart, DocumentSource, ImageUrl, Message, MessageContent,
        ResponseFormat, Tool, ToolCall, ToolCallFunction, ToolChoice, ToolChoiceFunction,
        ToolFunction,
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
        ..Default::default()
    }
}

#[test]
fn data_url_image_becomes_inline_data_not_file_data() {
    let req = make_request(
        "gemini-3.1-pro",
        vec![Message::User {
            content: MessageContent::Parts(vec![ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "data:image/png;base64,iVBORw0KGgo=".into(),
                    detail: None,
                },
            }]),
            name: None,
        }],
    );
    let body = translate_request(req).expect("translate ok");
    let s = serde_json::to_string(&body).unwrap();
    assert!(s.contains(r#""inlineData""#), "expected inlineData: {s}");
    assert!(s.contains(r#""mimeType":"image/png""#), "{s}");
    assert!(s.contains(r#""data":"iVBORw0KGgo=""#), "{s}");
    assert!(!s.contains(r#""fileData""#), "must not be fileData: {s}");
}

#[test]
fn remote_url_image_stays_file_data() {
    let req = make_request(
        "gemini-3.1-pro",
        vec![Message::User {
            content: MessageContent::Parts(vec![ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "https://example.com/cat.png".into(),
                    detail: None,
                },
            }]),
            name: None,
        }],
    );
    let body = translate_request(req).expect("translate ok");
    let s = serde_json::to_string(&body).unwrap();
    assert!(s.contains(r#""fileData""#), "expected fileData: {s}");
    assert!(s.contains("https://example.com/cat.png"), "{s}");
}

#[test]
fn base64_document_becomes_inline_data() {
    // Document Lane D4a: a base64 document part maps to Gemini inline_data with
    // the document's mime type (e.g. application/pdf).
    let req = make_request(
        "gemini-3.1-pro",
        vec![Message::User {
            content: MessageContent::Parts(vec![ContentPart::Document {
                document: DocumentPart {
                    source: DocumentSource::Base64 {
                        media_type: "application/pdf".into(),
                        data: "JVBERi0xLjQK".into(),
                    },
                    filename: Some("report.pdf".into()),
                },
            }]),
            name: None,
        }],
    );
    let body = translate_request(req).expect("translate ok");
    let s = serde_json::to_string(&body).unwrap();
    assert!(s.contains(r#""inlineData""#), "expected inlineData: {s}");
    assert!(s.contains(r#""mimeType":"application/pdf""#), "{s}");
    assert!(s.contains(r#""data":"JVBERi0xLjQK""#), "{s}");
}

#[test]
fn remote_url_document_becomes_file_data() {
    let req = make_request(
        "gemini-3.1-pro",
        vec![Message::User {
            content: MessageContent::Parts(vec![ContentPart::Document {
                document: DocumentPart {
                    source: DocumentSource::Url {
                        url: "https://example.com/report.pdf".into(),
                    },
                    filename: None,
                },
            }]),
            name: None,
        }],
    );
    let body = translate_request(req).expect("translate ok");
    let s = serde_json::to_string(&body).unwrap();
    assert!(s.contains(r#""fileData""#), "expected fileData: {s}");
    assert!(s.contains("https://example.com/report.pdf"), "{s}");
}

#[test]
fn validate_model_id_accepts_real_ids_rejects_injection() {
    use tt_provider_gemini::translate::validate_model_id;
    // Real Gemini ids pass.
    assert!(validate_model_id("gemini-3.1-pro").is_ok());
    assert!(validate_model_id("gemini-3.5-flash").is_ok());
    assert!(validate_model_id("gemini-1.5-pro-002").is_ok());
    // Path/query/fragment/whitespace injection is rejected.
    for bad in [
        "",
        "../models/x",
        "a/b",
        "a?x=1",
        "a#frag",
        "a b",
        "a:generateContent",
    ] {
        assert!(validate_model_id(bad).is_err(), "should reject {bad:?}");
    }
}

#[test]
fn gemini_supports_response_schema() {
    use tt_provider_gemini::{ClientConfig, GeminiProvider};
    use tt_shared::Provider;
    assert!(GeminiProvider::new(ClientConfig::default()).supports_response_schema());
}

#[test]
fn gemini_temperature_range_is_default_wide() {
    use tt_provider_gemini::{ClientConfig, GeminiProvider};
    use tt_shared::Provider;
    assert_eq!(
        GeminiProvider::new(ClientConfig::default()).temperature_range(),
        (0.0, 2.0)
    );
}

fn user_text(text: &str) -> Message {
    Message::User {
        content: MessageContent::Text(text.to_string()),
        name: None,
    }
}

#[test]
fn dropped_params_reports_present_fields_but_not_response_format() {
    use tt_provider_gemini::{ClientConfig, GeminiProvider};
    use tt_shared::Provider;

    let provider = GeminiProvider::new(ClientConfig::default());
    let mut req = make_request("gemini-3.1-pro", vec![]);
    // Gemini TRANSLATES response_format, so it must NOT be reported.
    req.response_format = Some(ResponseFormat {
        r#type: "json_object".into(),
        json_schema: None,
    });
    req.frequency_penalty = Some(0.2);
    req.seed = Some(7);
    req.user = Some("u1".into());
    let mut got = provider.dropped_params(&req);
    got.sort();
    assert_eq!(
        got,
        vec![
            "frequency_penalty".to_string(),
            "seed".to_string(),
            "user".to_string()
        ]
    );
}

#[test]
fn dropped_params_reports_unsupported_typed_compat_fields() {
    use tt_provider_gemini::{ClientConfig, GeminiProvider};
    use tt_shared::Provider;

    let provider = GeminiProvider::new(ClientConfig::default());
    let mut req = make_request("gemini-3.1-pro", vec![]);
    req.parallel_tool_calls = Some(true);
    req.reasoning_effort = Some("high".into());
    req.stream_options = Some(serde_json::json!({ "include_usage": true }));
    let mut got = provider.dropped_params(&req);
    got.sort();
    assert_eq!(
        got,
        vec![
            "parallel_tool_calls".to_string(),
            "reasoning_effort".to_string(),
            "stream_options".to_string(),
        ]
    );
}

#[test]
fn max_completion_tokens_maps_to_max_output_tokens() {
    use tt_provider_gemini::{ClientConfig, GeminiProvider};
    use tt_shared::Provider;

    // The spend cap must reach Gemini's `maxOutputTokens`, not vanish.
    let mut req = make_request("gemini-3.1-pro", vec![]);
    req.max_tokens = None;
    req.max_completion_tokens = Some(777);
    let body = translate_request(req.clone()).expect("translate ok");
    let gen_cfg = body.generation_config.as_ref().expect("generationConfig");
    assert_eq!(gen_cfg.max_output_tokens, Some(777));

    // And it must NOT be reported as dropped.
    let provider = GeminiProvider::new(ClientConfig::default());
    assert!(!provider
        .dropped_params(&req)
        .contains(&"max_completion_tokens".to_string()));
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
// 1. Text-only single-turn (request body)
// ---------------------------------------------------------------------------

#[test]
fn translate_text_only_single_turn() {
    let req = make_request("gemini-3.1-pro", vec![user_text("What is 2+2?")]);
    let body = translate_request(req).expect("translate ok");

    // System should be absent for a pure user message.
    assert!(body.system_instruction.is_none());
    assert_eq!(body.contents.len(), 1);
    assert_eq!(body.contents[0].role, "user");

    insta::assert_json_snapshot!("text_only_single_turn", &body);
}

// ---------------------------------------------------------------------------
// 2. Multi-turn with system extraction → systemInstruction
// ---------------------------------------------------------------------------

#[test]
fn translate_multi_turn_with_system() {
    let messages = vec![
        system_text("You are a helpful assistant."),
        user_text("Hello!"),
        assistant_text("Hi there!"),
        user_text("How are you?"),
    ];

    let req = make_request("gemini-3.1-pro", messages);
    let body = translate_request(req).expect("translate ok");

    // System extracted; only user/model in contents.
    assert!(body.system_instruction.is_some());
    assert_eq!(body.contents.len(), 3);
    assert_eq!(body.contents[0].role, "user");
    assert_eq!(body.contents[1].role, "model"); // assistant → model
    assert_eq!(body.contents[2].role, "user");

    insta::assert_json_snapshot!("multi_turn_with_system", &body);
}

// ---------------------------------------------------------------------------
// 3. Assistant role rename → model
// ---------------------------------------------------------------------------

#[test]
fn translate_assistant_role_renamed_to_model() {
    let messages = vec![
        user_text("Hello!"),
        assistant_text("Hi there!"),
        user_text("How are you?"),
    ];

    let req = make_request("gemini-3.1-pro", messages);
    let body = translate_request(req).expect("translate ok");

    assert_eq!(body.contents[0].role, "user");
    assert_eq!(body.contents[1].role, "model");
    assert_eq!(body.contents[2].role, "user");

    insta::assert_json_snapshot!("assistant_role_renamed", &body);
}

// ---------------------------------------------------------------------------
// 4. Multimodal image_url → fileData
// ---------------------------------------------------------------------------

#[test]
fn translate_multimodal_image_url_to_file_data() {
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

    let req = make_request("gemini-3.1-pro", messages);
    let body = translate_request(req).expect("translate ok");

    // Should have two parts: text + fileData
    assert_eq!(body.contents[0].parts.len(), 2);

    insta::assert_json_snapshot!("multimodal_image_url_to_file_data", &body);
}

// ---------------------------------------------------------------------------
// 5. Generation config (temperature, top_p, max_tokens, stop, responseSchema)
// ---------------------------------------------------------------------------

#[test]
fn translate_generation_config() {
    let mut req = make_request("gemini-3.5-flash", vec![user_text("Generate JSON.")]);
    req.temperature = Some(0.9);
    req.top_p = Some(0.95);
    req.max_tokens = Some(2048);
    req.stop = vec!["STOP".to_string(), "END".to_string()];
    req.response_format = Some(ResponseFormat {
        r#type: "json_schema".to_string(),
        json_schema: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            }
        })),
    });

    let body = translate_request(req).expect("translate ok");

    let gen_cfg = body.generation_config.as_ref().expect("generationConfig");
    assert_eq!(gen_cfg.temperature, Some(0.9));
    assert_eq!(gen_cfg.top_p, Some(0.95));
    assert_eq!(gen_cfg.max_output_tokens, Some(2048));
    assert_eq!(gen_cfg.stop_sequences, vec!["STOP", "END"]);
    assert_eq!(
        gen_cfg.response_mime_type.as_deref(),
        Some("application/json")
    );
    assert!(gen_cfg.response_schema.is_some());

    insta::assert_json_snapshot!("generation_config", &body);
}

// ---------------------------------------------------------------------------
// 6. Tools translation → functionDeclarations wrapper
// ---------------------------------------------------------------------------

#[test]
fn translate_tools_to_function_declarations() {
    let tools = vec![
        Tool {
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
        },
        Tool {
            r#type: "function".to_string(),
            function: ToolFunction {
                name: "search_web".to_string(),
                description: Some("Search the web.".to_string()),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" }
                    }
                }),
            },
        },
    ];

    let mut req = make_request("gemini-3.1-pro", vec![user_text("What's the weather?")]);
    req.tools = tools;

    let body = translate_request(req).expect("translate ok");

    // All functions should be in a single Gemini tool block.
    assert_eq!(body.tools.len(), 1);
    assert_eq!(body.tools[0].function_declarations.len(), 2);
    assert_eq!(body.tools[0].function_declarations[0].name, "get_weather");
    assert_eq!(body.tools[0].function_declarations[1].name, "search_web");

    insta::assert_json_snapshot!("tools_translation", &body);
}

// ---------------------------------------------------------------------------
// 7. tool_choice → toolConfig (all 3 modes: auto/specific/none)
// ---------------------------------------------------------------------------

#[test]
fn translate_tool_choice_auto() {
    let mut req = make_request("gemini-3.1-pro", vec![user_text("Help me.")]);
    req.tool_choice = Some(ToolChoice::Auto("auto".to_string()));

    let body = translate_request(req).expect("translate ok");

    let tc = body
        .tool_config
        .as_ref()
        .expect("toolConfig should be present");
    assert_eq!(tc.function_calling_config.mode, "AUTO");
    assert!(tc.function_calling_config.allowed_function_names.is_empty());

    insta::assert_json_snapshot!("tool_choice_auto", &body);
}

#[test]
fn translate_tool_choice_specific_function() {
    let mut req = make_request("gemini-3.1-pro", vec![user_text("Search for me.")]);
    req.tool_choice = Some(ToolChoice::Specific {
        r#type: "function".to_string(),
        function: ToolChoiceFunction {
            name: "search_web".to_string(),
        },
    });

    let body = translate_request(req).expect("translate ok");

    let tc = body
        .tool_config
        .as_ref()
        .expect("toolConfig should be present");
    assert_eq!(tc.function_calling_config.mode, "ANY");
    assert_eq!(
        tc.function_calling_config.allowed_function_names,
        vec!["search_web"]
    );

    insta::assert_json_snapshot!("tool_choice_specific", &body);
}

#[test]
fn translate_tool_choice_none() {
    let mut req = make_request("gemini-3.1-pro", vec![user_text("Don't use tools.")]);
    req.tool_choice = Some(ToolChoice::Auto("none".to_string()));

    let body = translate_request(req).expect("translate ok");

    let tc = body
        .tool_config
        .as_ref()
        .expect("toolConfig should be present");
    assert_eq!(tc.function_calling_config.mode, "NONE");

    insta::assert_json_snapshot!("tool_choice_none", &body);
}

#[test]
fn translate_tool_choice_required() {
    let mut req = make_request("gemini-3.1-pro", vec![user_text("You must use a tool.")]);
    req.tool_choice = Some(ToolChoice::Auto("required".to_string()));

    let body = translate_request(req).expect("translate ok");

    let tc = body
        .tool_config
        .as_ref()
        .expect("toolConfig should be present");
    assert_eq!(tc.function_calling_config.mode, "ANY");
    assert!(tc.function_calling_config.allowed_function_names.is_empty());
}

// ---------------------------------------------------------------------------
// 8. Assistant tool_call → functionCall in content
// ---------------------------------------------------------------------------

#[test]
fn translate_assistant_tool_call_to_function_call() {
    let messages = vec![
        user_text("What's the weather in London?"),
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
    ];

    let req = make_request("gemini-3.1-pro", messages);
    let body = translate_request(req).expect("translate ok");

    // Assistant message should become model role with functionCall part.
    assert_eq!(body.contents.len(), 2);
    assert_eq!(body.contents[1].role, "model");
    assert_eq!(body.contents[1].parts.len(), 1);

    // Verify the part is a functionCall.
    let serialized = serde_json::to_value(&body).expect("serialize ok");
    let fn_call = &serialized["contents"][1]["parts"][0]["functionCall"];
    assert_eq!(fn_call["name"], "get_weather");
    assert_eq!(fn_call["args"]["location"], "London");

    insta::assert_json_snapshot!("assistant_tool_call", &body);
}

// ---------------------------------------------------------------------------
// 9. Tool result → function response in content
// ---------------------------------------------------------------------------

#[test]
fn translate_tool_result_to_function_response() {
    let messages = vec![
        user_text("What's the weather in London?"),
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
            content: MessageContent::Text(r#"{"temperature":15,"unit":"C"}"#.to_string()),
            tool_call_id: "call_abc123".to_string(),
        },
        user_text("Thanks!"),
    ];

    let req = make_request("gemini-3.1-pro", messages);
    let body = translate_request(req).expect("translate ok");

    // Tool result becomes function role with functionResponse.
    assert_eq!(body.contents[2].role, "function");

    let serialized = serde_json::to_value(&body).expect("serialize ok");
    let fn_resp = &serialized["contents"][2]["parts"][0]["functionResponse"];
    assert_eq!(fn_resp["name"], "get_weather");

    insta::assert_json_snapshot!("tool_result_to_function_response", &body);
}

// ---------------------------------------------------------------------------
// 10. Generation config strips OpenAI-only fields
// ---------------------------------------------------------------------------

#[test]
fn translate_strips_openai_only_fields() {
    let mut req = make_request("gemini-3.1-pro", vec![user_text("Hello.")]);
    req.presence_penalty = Some(0.5);
    req.frequency_penalty = Some(0.3);
    req.n = Some(3);
    req.seed = Some(42);
    req.user = Some("user-123".to_string());
    req.tt_extras
        .insert("route_hint".to_string(), serde_json::json!("us-east-1"));

    let body = translate_request(req).expect("translate ok");
    let serialized = serde_json::to_string(&body).expect("serialize ok");

    // These OpenAI-specific fields should not appear.
    assert!(!serialized.contains("presence_penalty"));
    assert!(!serialized.contains("frequency_penalty"));
    assert!(!serialized.contains("\"n\""));
    assert!(!serialized.contains("seed"));
    assert!(!serialized.contains("tt_extras"));
    assert!(!serialized.contains("route_hint"));

    insta::assert_json_snapshot!("openai_fields_stripped", &body);
}

// ---------------------------------------------------------------------------
// 11. Response with functionCall → tool_calls translation
// ---------------------------------------------------------------------------

#[test]
fn translate_response_function_call_to_tool_calls() {
    let response_body = r#"{
        "candidates": [
            {
                "content": {
                    "role": "model",
                    "parts": [
                        {
                            "functionCall": {
                                "name": "get_weather",
                                "args": {"location": "London", "units": "celsius"}
                            }
                        }
                    ]
                },
                "finishReason": "STOP",
                "index": 0
            }
        ],
        "usageMetadata": {
            "promptTokenCount": 50,
            "candidatesTokenCount": 20,
            "totalTokenCount": 70,
            "cachedContentTokenCount": 0
        },
        "modelVersion": "gemini-3.1-pro-002"
    }"#;

    let resp = deserialize_response(response_body, "gemini-3.1-pro").expect("deserialize ok");

    // finish_reason should be overridden to "tool_calls" since functionCall is present.
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("tool_calls"));

    // Message should have tool_calls in OpenAI format.
    match &resp.choices[0].message {
        tt_shared::messages::Message::Assistant { tool_calls, .. } => {
            assert_eq!(tool_calls.len(), 1);
            assert_eq!(tool_calls[0].function.name, "get_weather");
            let args: serde_json::Value =
                serde_json::from_str(&tool_calls[0].function.arguments).expect("valid json");
            assert_eq!(args["location"], "London");
        }
        other => panic!("expected Assistant message, got {other:?}"),
    }

    // The model version from response should be used.
    assert_eq!(resp.model, "gemini-3.1-pro-002");

    insta::assert_json_snapshot!(
        "response_function_call_to_tool_calls",
        &resp,
        { ".created" => "[timestamp]", ".id" => "[id]",
          ".choices[0].message.tool_calls[0].id" => "[call_id]" }
    );
}

// ---------------------------------------------------------------------------
// 12. Usage round-trip with cachedContentTokenCount
// ---------------------------------------------------------------------------

#[test]
fn usage_round_trip_with_cached_content_token_count() {
    let u = GeminiUsageMetadata {
        prompt_token_count: 200,
        candidates_token_count: 80,
        total_token_count: 280,
        cached_content_token_count: Some(50),
    };

    let usage = translate_usage(u);

    assert_eq!(usage.prompt_tokens, 200);
    assert_eq!(usage.completion_tokens, 80);
    assert_eq!(usage.total_tokens, 280);
    assert_eq!(usage.cached_tokens, 50);
    assert!(usage.cache_creation_input_tokens.is_none());
    // Raw cache-read preserved unfolded (telemetry NULL-vs-0).
    assert_eq!(usage.cache_read_input_tokens, Some(50));
}

#[test]
fn usage_without_cached_content_token_count_keeps_raw_none() {
    // Gemini omitted cachedContentTokenCount entirely → folded 0, raw None.
    let u = GeminiUsageMetadata {
        prompt_token_count: 200,
        candidates_token_count: 80,
        total_token_count: 280,
        cached_content_token_count: None,
    };

    let usage = translate_usage(u);

    assert_eq!(usage.cached_tokens, 0);
    assert_eq!(usage.cache_read_input_tokens, None);
}

// ---------------------------------------------------------------------------
// 13. finishReason mapping (STOP/MAX_TOKENS/SAFETY)
// ---------------------------------------------------------------------------

#[test]
fn translate_finish_reason_stop() {
    use tt_provider_gemini::translate::map_finish_reason;
    assert_eq!(map_finish_reason("STOP"), "stop");
    assert_eq!(map_finish_reason("MAX_TOKENS"), "length");
    assert_eq!(map_finish_reason("SAFETY"), "content_filter");
    assert_eq!(map_finish_reason("RECITATION"), "content_filter");
    assert_eq!(map_finish_reason("OTHER"), "stop");
    // Unknown values fall back to stop
    assert_eq!(map_finish_reason("UNKNOWN_REASON"), "stop");
}

#[test]
fn translate_response_stop_reason_max_tokens() {
    let response_body = r#"{
        "candidates": [
            {
                "content": {
                    "role": "model",
                    "parts": [{ "text": "Truncated output..." }]
                },
                "finishReason": "MAX_TOKENS",
                "index": 0
            }
        ],
        "usageMetadata": {
            "promptTokenCount": 10,
            "candidatesTokenCount": 1024,
            "totalTokenCount": 1034
        }
    }"#;

    let resp = deserialize_response(response_body, "gemini-3.5-flash").expect("deserialize ok");
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("length"));

    insta::assert_json_snapshot!(
        "response_max_tokens_finish_reason",
        &resp,
        { ".created" => "[timestamp]", ".id" => "[id]" }
    );
}

#[test]
fn translate_response_safety_finish_reason() {
    let response_body = r#"{
        "candidates": [
            {
                "content": {
                    "role": "model",
                    "parts": [{ "text": "" }]
                },
                "finishReason": "SAFETY",
                "index": 0
            }
        ],
        "usageMetadata": {
            "promptTokenCount": 5,
            "candidatesTokenCount": 0,
            "totalTokenCount": 5
        }
    }"#;

    let resp = deserialize_response(response_body, "gemini-3.1-pro").expect("deserialize ok");
    assert_eq!(
        resp.choices[0].finish_reason.as_deref(),
        Some("content_filter")
    );

    insta::assert_json_snapshot!(
        "response_safety_finish_reason",
        &resp,
        { ".created" => "[timestamp]", ".id" => "[id]" }
    );
}

// ---------------------------------------------------------------------------
// 14. Full response snapshot with cached token usage
// ---------------------------------------------------------------------------

#[test]
fn translate_full_response_with_cached_tokens() {
    let response_body = r#"{
        "candidates": [
            {
                "content": {
                    "role": "model",
                    "parts": [{ "text": "Hello! How can I help you today?" }]
                },
                "finishReason": "STOP",
                "index": 0
            }
        ],
        "usageMetadata": {
            "promptTokenCount": 100,
            "candidatesTokenCount": 10,
            "totalTokenCount": 110,
            "cachedContentTokenCount": 80
        },
        "modelVersion": "gemini-3.1-pro-002"
    }"#;

    let resp = deserialize_response(response_body, "gemini-3.1-pro").expect("deserialize ok");

    assert_eq!(resp.usage.prompt_tokens, 100);
    assert_eq!(resp.usage.completion_tokens, 10);
    assert_eq!(resp.usage.total_tokens, 110);
    assert_eq!(resp.usage.cached_tokens, 80);
    assert_eq!(resp.model, "gemini-3.1-pro-002");

    insta::assert_json_snapshot!(
        "full_response_with_cached_tokens",
        &resp,
        { ".created" => "[timestamp]", ".id" => "[id]" }
    );
}
