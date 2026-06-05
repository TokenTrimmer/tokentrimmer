//! Typed Rust client for the TokenTrimmer gateway. OpenAI-compatible chat that
//! returns a typed [`CostInfo`] parsed from the `x-tokentrimmer-*` headers, plus
//! the `X-TokenTrimmer-Tag`/`-Route` extensions as first-class builder options.

use reqwest::header::HeaderMap;
use serde_json::{json, Value};

pub use tt_shared::messages::Message;
use tt_shared::messages::MessageContent;

/// Build a `user` message.
#[must_use]
pub fn user(content: impl Into<String>) -> Message {
    Message::User {
        content: MessageContent::Text(content.into()),
        name: None,
    }
}

/// Build a `system` message.
#[must_use]
pub fn system(content: impl Into<String>) -> Message {
    Message::System {
        content: MessageContent::Text(content.into()),
    }
}

/// Build an `assistant` message.
#[must_use]
pub fn assistant(content: impl Into<String>) -> Message {
    Message::Assistant {
        content: Some(MessageContent::Text(content.into())),
        tool_calls: Vec::new(),
        name: None,
    }
}

/// Cost/savings + routing metadata parsed from the gateway's `x-tokentrimmer-*`
/// response headers. Each field is `None` when its header is absent/unparseable.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostInfo {
    pub cost_usd: Option<f64>,
    pub saved_usd: Option<f64>,
    pub baseline_cost_usd: Option<f64>,
    pub model_used: Option<String>,
    pub provider: Option<String>,
    pub trace_id: Option<String>,
    pub cache: Option<String>,
}

fn header_str(h: &HeaderMap, name: &str) -> Option<String> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(ToString::to_string)
}

fn header_f64(h: &HeaderMap, name: &str) -> Option<f64> {
    header_str(h, name).and_then(|s| s.parse().ok())
}

/// Parse the gateway's cost/savings headers.
#[must_use]
pub fn parse_cost(headers: &HeaderMap) -> CostInfo {
    CostInfo {
        cost_usd: header_f64(headers, "x-tokentrimmer-cost-usd"),
        saved_usd: header_f64(headers, "x-tokentrimmer-saved-usd"),
        baseline_cost_usd: header_f64(headers, "x-tokentrimmer-baseline-cost-usd"),
        model_used: header_str(headers, "x-tokentrimmer-model-used"),
        provider: header_str(headers, "x-tokentrimmer-provider"),
        trace_id: header_str(headers, "x-tokentrimmer-trace-id"),
        cache: header_str(headers, "x-tokentrimmer-cache"),
    }
}

/// The `/v1/chat/completions` request body (non-streamed).
#[must_use]
pub fn build_body(
    model: &str,
    messages: &[Message],
    max_tokens: Option<u32>,
    temperature: Option<f32>,
) -> Value {
    let mut body = json!({ "model": model, "messages": messages, "stream": false });
    if let Some(mt) = max_tokens {
        body["max_tokens"] = json!(mt);
    }
    if let Some(t) = temperature {
        body["temperature"] = json!(t);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_helpers() {
        assert!(matches!(user("hi"), Message::User { content: MessageContent::Text(t), .. } if t == "hi"));
        assert!(matches!(system("s"), Message::System { content: MessageContent::Text(t) } if t == "s"));
        assert!(matches!(assistant("a"), Message::Assistant { content: Some(MessageContent::Text(t)), .. } if t == "a"));
    }

    #[test]
    fn parse_cost_reads_headers() {
        let mut h = HeaderMap::new();
        h.insert("x-tokentrimmer-cost-usd", "0.0001".parse().unwrap());
        h.insert("x-tokentrimmer-saved-usd", "0.0003".parse().unwrap());
        h.insert("x-tokentrimmer-model-used", "gpt-4o-mini".parse().unwrap());
        h.insert("x-tokentrimmer-cache", "miss".parse().unwrap());
        let c = parse_cost(&h);
        assert_eq!(c.cost_usd, Some(0.0001));
        assert_eq!(c.saved_usd, Some(0.0003));
        assert_eq!(c.model_used.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(c.cache.as_deref(), Some("miss"));
        assert_eq!(c.provider, None); // absent
        assert_eq!(parse_cost(&HeaderMap::new()), CostInfo::default());
    }

    #[test]
    fn parse_cost_ignores_non_numeric() {
        let mut h = HeaderMap::new();
        h.insert("x-tokentrimmer-cost-usd", "n/a".parse().unwrap());
        assert_eq!(parse_cost(&h).cost_usd, None);
    }

    #[test]
    fn build_body_shape() {
        let b = build_body("gpt-4o", &[user("hi")], None, None);
        assert_eq!(b["model"], "gpt-4o");
        assert_eq!(b["stream"], false);
        assert!(b["messages"].is_array());
        assert!(b.get("max_tokens").is_none());
        let b2 = build_body("m", &[], Some(256), Some(0.2));
        assert_eq!(b2["max_tokens"], 256);
        assert!((b2["temperature"].as_f64().unwrap() - 0.2).abs() < 1e-6);
    }
}
