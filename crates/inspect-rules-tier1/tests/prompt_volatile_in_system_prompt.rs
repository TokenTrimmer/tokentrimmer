//! Focused behaviour tests for `prompt-volatile-in-system-prompt`.
//!
//! This rule flags a volatile token — a timestamp, `now()`/`Date.now()`/
//! `datetime.now()`, a UUID, or a long random hex/base64 token — embedded
//! ANYWHERE inside a system prompt / prompt template. Such a token busts the
//! provider prompt cache on every call. (It is broader than
//! `prompt-dynamic-prefix-breaks-cache`, which only fires when the volatile
//! value is the very first thing in the prompt.)

use tt_inspect_core::{Finding, Language, Rule};
use tt_inspect_rules_tier1::PromptVolatileInSystemPromptRule;

/// Run the rule against a single source line/snippet with a non-fixture path so
/// the `is_test_fixture` guard does not suppress findings.
fn scan(src: &str, lang: Language) -> Vec<Finding> {
    let ext = match lang {
        Language::Python => "py",
        Language::Typescript => "ts",
        Language::Javascript => "js",
        Language::Markdown => "md",
    };
    PromptVolatileInSystemPromptRule::new().check(src, lang, &format!("/src/app.{ext}"))
}

#[test]
fn fires_on_now_embedded_mid_system_prompt() {
    let src =
        r#"system_prompt = f"You are helpful. Time is {datetime.datetime.now()}. Follow rules.""#;
    let findings = scan(src, Language::Python);
    assert_eq!(
        findings.len(),
        1,
        "embedded now() in system prompt should fire"
    );
    assert_eq!(findings[0].rule_id, "prompt-volatile-in-system-prompt");
}

#[test]
fn fires_on_uuid_appended_to_system_prompt() {
    // Even at the END of the system prompt, a per-call uuid invalidates the
    // cached system block — the prefix-only rule misses this; this rule must
    // catch it.
    let src = r#"system_prompt = f"You are a careful agent. Session: {uuid.uuid4()}""#;
    let findings = scan(src, Language::Python);
    assert_eq!(
        findings.len(),
        1,
        "trailing uuid in system prompt should fire"
    );
}

#[test]
fn fires_on_literal_iso_timestamp_in_system_prompt() {
    let src =
        r#"system_prompt = "You are helpful. Knowledge as of 2026-06-10T14:23:55Z. Be precise.""#;
    let findings = scan(src, Language::Python);
    assert_eq!(
        findings.len(),
        1,
        "literal ISO timestamp in system prompt should fire"
    );
}

#[test]
fn fires_on_literal_uuid_in_system_prompt() {
    let src =
        r#"SYSTEM_INSTRUCTION = "You are an expert. Trace 550e8400-e29b-41d4-a716-446655440000.""#;
    let findings = scan(src, Language::Python);
    assert_eq!(
        findings.len(),
        1,
        "literal uuid in system prompt should fire"
    );
}

#[test]
fn fires_on_isostring_in_ts_template() {
    let src = r#"const systemPrompt = `You are helpful. Today is ${new Date().toISOString()}.`;"#;
    let findings = scan(src, Language::Typescript);
    assert_eq!(
        findings.len(),
        1,
        "toISOString in ts system template should fire"
    );
}

#[test]
fn does_not_fire_on_static_system_prompt() {
    let src = r#"system_prompt = "You are a helpful assistant. Follow the rules precisely.""#;
    assert!(
        scan(src, Language::Python).is_empty(),
        "clean static system prompt must not fire"
    );
}

#[test]
fn does_not_fire_on_log_line_with_timestamp() {
    // No prompt/system/instruction context → not a cached prefix.
    let src = r#"log_line = f"{datetime.datetime.now()} incoming request received""#;
    assert!(
        scan(src, Language::Python).is_empty(),
        "a log line with a timestamp must not fire"
    );
}

#[test]
fn does_not_fire_on_cache_key_uuid() {
    let src = r#"cache_key = f"{uuid.uuid4()}""#;
    assert!(
        scan(src, Language::Python).is_empty(),
        "a cache-key uuid (no prompt context) must not fire"
    );
}

#[test]
fn does_not_fire_on_version_string_in_system_prompt() {
    // A semantic version is stable across calls.
    let src = r#"system_prompt = "You are assistant v1.2.3. Keep answers short.""#;
    assert!(
        scan(src, Language::Python).is_empty(),
        "a semantic version in a system prompt must not be flagged"
    );
}

#[test]
fn does_not_fire_on_short_hex_color_in_system_prompt() {
    let src = r#"system_prompt = "You are a UI assistant. Brand color #ff8800. Be brief.""#;
    assert!(
        scan(src, Language::Python).is_empty(),
        "a short hex color must not trip the random-token detector"
    );
}

#[test]
fn finding_message_and_fix_hint_are_actionable() {
    let src = r#"system_prompt = f"You are helpful. Now: {datetime.datetime.now()}.""#;
    let findings = scan(src, Language::Python);
    assert_eq!(findings.len(), 1);
    let f = &findings[0];
    let msg = f.message.to_lowercase();
    assert!(
        msg.contains("cache"),
        "message must explain the cache impact: {}",
        f.message
    );
    let hint = f.fix_hint.as_ref().expect("fix hint present");
    assert!(
        hint.to_lowercase().contains("move") || hint.to_lowercase().contains("outside"),
        "fix hint must advise moving volatile content out of the cached prefix: {hint}"
    );
}
