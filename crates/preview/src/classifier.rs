//! Cheap regex-based task classifier. Pattern-matches the last user message
//! against indicators of: classification, extraction, code, agent, generic chat.
//! The same patterns inform `output-no-max-tokens` Inspect rule fix hints.

use crate::types::Message;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskClass {
    Classification,
    Extraction,
    Code,
    Agent,
    Chat,
}

pub fn classify(messages: &[Message]) -> TaskClass {
    let last_user = messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .and_then(|m| m.content.as_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();

    if last_user.contains("classify")
        || last_user.contains("category")
        || last_user.contains("label as")
        || last_user.contains("is this")
        || last_user.contains("yes or no")
    {
        return TaskClass::Classification;
    }
    if last_user.contains("extract")
        || last_user.contains("parse")
        || last_user.contains("structured output")
        || last_user.contains("json schema")
        || last_user.contains("entity")
        || last_user.contains("pull out")
    {
        return TaskClass::Extraction;
    }
    if last_user.contains("function")
        || last_user.contains("code")
        || last_user.contains("```")
        || last_user.contains("refactor")
        || last_user.contains("implement")
    {
        return TaskClass::Code;
    }
    if messages.len() > 4 && messages.iter().any(|m| m.role == "tool") {
        return TaskClass::Agent;
    }
    TaskClass::Chat
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn u(t: &str) -> Message {
        Message {
            role: "user".into(),
            content: json!(t),
        }
    }

    #[test]
    fn classify_classification() {
        assert_eq!(
            classify(&[u("Is this email spam? yes or no")]),
            TaskClass::Classification
        );
    }

    #[test]
    fn classify_extraction() {
        assert_eq!(
            classify(&[u("Extract the names from this text")]),
            TaskClass::Extraction
        );
    }

    #[test]
    fn classify_code() {
        assert_eq!(
            classify(&[u("write a function that adds two numbers")]),
            TaskClass::Code
        );
    }

    #[test]
    fn classify_chat_default() {
        assert_eq!(classify(&[u("Hi how are you")]), TaskClass::Chat);
    }
}
