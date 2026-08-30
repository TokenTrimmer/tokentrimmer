//! Deterministic prompt prefix normalizer pass.
//!
//! Enforces byte-identical ordering on static system prompt structures,
//! JSON tool definitions, and metadata before cache-key hashing.
//! Standardizes tool argument JSON schemas, sorting fields deterministically
//! to maximize provider KV-cache hits (OpenAI, Anthropic, Gemini).

use serde_json::Value;
use tt_shared::messages::{Message, MessageContent};
use tt_shared::ChatCompletionRequest;

/// Normalizes the static prefix components of a request for maximum KV cache reuse.
#[must_use]
pub fn normalize_request_prefix(mut req: ChatCompletionRequest) -> ChatCompletionRequest {
    // 1. Sort and canonicalize tools deterministically by name
    if !req.tools.is_empty() {
        req.tools
            .sort_by(|a, b| a.function.name.cmp(&b.function.name));
        for t in req.tools.iter_mut() {
            canonicalize_json_value(&mut t.function.parameters);
        }
    }

    // 2. Canonicalize system message text whitespace and structure
    for msg in &mut req.messages {
        if let Message::System { content, .. } = msg {
            if let MessageContent::Text(ref mut txt) = content {
                *txt = canonicalize_system_text(txt);
            }
        }
    }

    req
}

/// Recursively sort JSON object keys for canonical byte serialization.
pub fn canonicalize_json_value(val: &mut Value) {
    match val {
        Value::Object(map) => {
            let old_map = std::mem::take(map);
            let mut entries: Vec<(String, Value)> = old_map.into_iter().collect();
            entries.sort_by(|(k1, _), (k2, _)| k1.cmp(k2));
            for (k, mut v) in entries {
                canonicalize_json_value(&mut v);
                map.insert(k, v);
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                canonicalize_json_value(item);
            }
        }
        _ => {}
    }
}

/// Normalize CRLF -> LF, strip trailing whitespace per line, collapse 3+ consecutive newlines to 2.
fn canonicalize_system_text(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = Vec::new();
    for line in normalized.lines() {
        lines.push(line.trim_end());
    }
    let joined = lines.join("\n");

    // Collapse multi-newline runs
    let mut out = String::with_capacity(joined.len());
    let mut newline_count = 0;
    for ch in joined.chars() {
        if ch == '\n' {
            newline_count += 1;
            if newline_count <= 2 {
                out.push(ch);
            }
        } else {
            newline_count = 0;
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_shared::messages::{Tool, ToolFunction};

    #[test]
    fn test_canonicalize_tools_and_system_prompt() {
        let req = ChatCompletionRequest {
            model: "claude-sonnet-5".into(),
            messages: vec![
                Message::System {
                    content: MessageContent::Text(
                        "System prompt with trailing spaces.   \r\n\r\n\r\nLine 2.\n".into(),
                    ),
                },
                Message::User {
                    content: MessageContent::Text("Hello".into()),
                    name: None,
                },
            ],
            tools: vec![
                Tool {
                    r#type: "function".into(),
                    function: ToolFunction {
                        name: "z_tool".into(),
                        description: None,
                        parameters: serde_json::json!({
                            "z": 1,
                            "a": 2
                        }),
                    },
                },
                Tool {
                    r#type: "function".into(),
                    function: ToolFunction {
                        name: "a_tool".into(),
                        description: None,
                        parameters: serde_json::json!({
                            "type": "object"
                        }),
                    },
                },
            ],
            ..Default::default()
        };

        let norm = normalize_request_prefix(req);
        assert_eq!(norm.tools[0].function.name, "a_tool");
        assert_eq!(norm.tools[1].function.name, "z_tool");

        if let Message::System {
            content: MessageContent::Text(txt),
            ..
        } = &norm.messages[0]
        {
            assert!(!txt.contains("\r"));
            assert!(!txt.contains("   \n"));
            assert!(!txt.contains("\n\n\n"));
            assert!(txt.contains("Line 2."));
        } else {
            panic!("expected system message");
        }
    }
}
