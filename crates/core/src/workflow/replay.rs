//! Workflow node replay — offline re-execution of captured node inputs.
//!
//! The engine records a bounded, value-free snapshot of what each executed
//! node consumed: the template it evaluated (`expr` / `prompt` / `cond`) and
//! the resolved reference bindings that drove that evaluation. Secret refs are
//! already redacted to `"***"` by the engine's secret-free resolver, so the
//! capture is safe to persist in the node journal.
//!
//! [`replay_transform`] re-executes a deterministic node's template purely from
//! that recorded snapshot — no provider, no I/O — so a debugger can reproduce
//! a node's output (or diagnose why it differed) without re-running the
//! workflow. It is deliberately stricter than the live substitution path: a
//! `{{ref}}` absent from the captured bindings is reported as [`ReplayError::MissingRef`]
//! rather than silently blanked, because at record time every referenced
//! binding existed — a miss therefore means the capture was truncated and the
//! debugger should know instead of trusting an empty expansion.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Hard bound on the number of captured reference bindings per node. A template
/// referencing more refs is truncated at this count (first-occurrence order).
pub(crate) const MAX_CAPTURED_REFS: usize = 64;
/// Hard bound on the length of any single captured reference value (chars).
pub(crate) const MAX_CAPTURED_REF_LEN: usize = 2048;
/// Hard bound on the captured template length (chars).
pub(crate) const MAX_CAPTURED_TEMPLATE_LEN: usize = 4096;

/// Bounded, value-free capture of the references a node resolved at run time.
///
/// Serializes to the `input` JSONB column of the node journal and is the input
/// a replay re-executes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CapturedNodeInput {
    /// The template the node evaluated, truncated to [`MAX_CAPTURED_TEMPLATE_LEN`].
    pub template: String,
    /// Resolved reference bindings keyed by the trimmed ref string as it
    /// appears between `{{` and `}}` (e.g. `"input"`, `"m1.score"`,
    /// `"variables.REGION"`), each truncated to [`MAX_CAPTURED_REF_LEN`].
    /// Duplicate refs collapse to their first resolution; secret refs are
    /// already `"***"`.
    pub refs: BTreeMap<String, String>,
}

/// Why a replay could not reproduce a captured node's transform. Returned by
/// [`replay_transform`]; awaited by the same future debugger API consumer.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReplayError {
    /// A `{{ref}}` referenced by the template is absent from the captured
    /// bindings — the capture was truncated at record time (see
    /// [`MAX_CAPTURED_REFS`]) or otherwise lost data.
    MissingRef { name: String },
}

/// Truncate a string to at most `max` characters without splitting a UTF-8
/// code point.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// Enumerate the `{{ref}}` tokens in `template` in first-occurrence order and
/// resolve each through `resolve`. Returns a bounded map keyed by the trimmed
/// ref string; duplicate refs resolve once (first occurrence). Tokens are
/// trimmed exactly as the engine's live substitution trims them, so the keys
/// line up with `{{ input }}` and `{{input}}` alike. An unclosed `{{` is
/// passed through as-is and stops the scan (mirroring live substitution).
pub(crate) fn capture_refs(
    template: &str,
    mut resolve: impl FnMut(&str) -> String,
) -> BTreeMap<String, String> {
    let mut refs = BTreeMap::new();
    let mut remaining = template;
    while refs.len() < MAX_CAPTURED_REFS {
        let Some(open) = remaining.find("{{") else {
            break;
        };
        remaining = &remaining[open + 2..];
        let Some(close) = remaining.find("}}") else {
            break; // unclosed `{{` — emitted literally by substitution.
        };
        let ref_str = remaining[..close].trim();
        let value = truncate_chars(&resolve(ref_str), MAX_CAPTURED_REF_LEN);
        refs.entry(ref_str.to_string()).or_insert(value);
        remaining = &remaining[close + 2..];
    }
    refs
}

/// Build a bounded input capture for one template evaluation. `resolve` must be
/// the engine's secret-free resolver so captured values exactly equal what live
/// substitution produced (secret refs arrive as `"***"`).
pub(crate) fn capture_node_input(
    template: &str,
    resolve: impl FnMut(&str) -> String,
) -> Option<CapturedNodeInput> {
    let refs = capture_refs(template, resolve);
    if refs.is_empty() {
        // A literal-only template consumes nothing worth replaying.
        return None;
    }
    Some(CapturedNodeInput {
        template: truncate_chars(template, MAX_CAPTURED_TEMPLATE_LEN),
        refs,
    })
}

/// Re-execute the captured transform template purely from the recorded
/// bindings (offline; no provider, no I/O).
///
/// Mirrors the engine's substitution scan: unclosed `{{` is passed through
/// as-is. Unlike live substitution, a referenced ref absent from the capture is
/// reported via [`ReplayError::MissingRef`] instead of silently blanked — the
/// engine only ever wrote a capture where the ref existed at record time, so a
/// miss is a capture-truncation signal a debugger must surface, not collapse.
///
/// Exercised by engine/replay tests; wired into a debugger API in a later
/// slice (kept on par with the `load_subworkflow` precedent while awaiting its
/// route consumer).
#[allow(dead_code)]
pub(crate) fn replay_transform(input: &CapturedNodeInput) -> Result<String, ReplayError> {
    let mut result = String::with_capacity(input.template.len() + 16);
    let mut remaining = input.template.as_str();
    while let Some(open) = remaining.find("{{") {
        result.push_str(&remaining[..open]);
        remaining = &remaining[open + 2..];
        let Some(close) = remaining.find("}}") else {
            result.push_str("{{");
            break;
        };
        let ref_str = remaining[..close].trim();
        let value = input.refs.get(ref_str).ok_or_else(|| ReplayError::MissingRef {
            name: ref_str.to_string(),
        })?;
        result.push_str(value);
        remaining = &remaining[close + 2..];
    }
    result.push_str(remaining);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_static(ref_str: &str) -> String {
        match ref_str {
            "input" => "hello".to_string(),
            "m1.score" => "0.98".to_string(),
            "m1" => r#"{"score":0.98}"#.to_string(),
            "variables.REGION" => "us-east".to_string(),
            "secrets.K" => "***".to_string(),
            other => format!("resolved:{other}"),
        }
    }

    #[test]
    fn capture_parses_trimmed_refs_and_collapses_duplicates() {
        let template = "{{ input }}t{{m1.score}}!!{{input}}";
        let refs = capture_refs(template, resolve_static);
        let mut expected = BTreeMap::new();
        expected.insert("input".to_string(), "hello".to_string());
        expected.insert("m1.score".to_string(), "0.98".to_string());
        assert_eq!(refs, expected, "keys trimmed; duplicates collapse to first resolution");
    }

    #[test]
    fn capture_resolves_fields_and_redacts_secrets() {
        // `{{m1}}` and `{{m1.score}}` are distinct refs; secrets come back as the
        // engine's redaction marker so a capture is value-free by construction.
        let refs = capture_refs("{{m1}} then {{m1.score}} and {{secrets.K}}", resolve_static);
        assert_eq!(refs.get("m1").map(String::as_str), Some(r#"{"score":0.98}"#));
        assert_eq!(refs.get("m1.score").map(String::as_str), Some("0.98"));
        assert_eq!(refs.get("secrets.K").map(String::as_str), Some("***"));
    }

    #[test]
    fn capture_empty_and_literal_only_templates() {
        assert!(capture_refs("", resolve_static).is_empty());
        assert!(capture_refs("plain text, no braces", resolve_static).is_empty());
        // Unclosed `{{` stops the scan — nothing is captured.
        assert!(capture_refs("oops {{input", resolve_static).is_empty());
    }

    #[test]
    fn capture_refs_is_bounded() {
        // 200 distinct refs → only MAX_CAPTURED_REFS are kept.
        let template = (0..200)
            .map(|i| format!("{{{{r{i}}}}}"))
            .collect::<Vec<_>>()
            .join(",");
        let refs = capture_refs(&template, resolve_static);
        assert_eq!(refs.len(), MAX_CAPTURED_REFS);
        // The first 64 distinct refs are retained, in trimmed-key order.
        assert!(refs.contains_key("r0"));
        assert!(!refs.contains_key("r100"));
    }

    #[test]
    fn captured_values_and_template_are_truncated() {
        let long = "x".repeat(MAX_CAPTURED_REF_LEN + 100);
        let template = "{{input}}";
        let capture = capture_node_input(template, |_| long.clone()).expect("has a ref");
        // Ref value truncated.
        assert!(capture.refs["input"].chars().count() <= MAX_CAPTURED_REF_LEN);
        assert_eq!(capture.refs["input"].chars().count(), MAX_CAPTURED_REF_LEN);
        // Template truncated at MAX_CAPTURED_TEMPLATE_LEN (ref placed first so
        // it survives the truncation window, mirroring in-order templates).
        let huge_template = "a".repeat(MAX_CAPTURED_TEMPLATE_LEN + 500);
        let capture2 = capture_node_input(&format!("{{{{input}}}} {huge_template}"), resolve_static)
            .expect("has a ref");
        assert!(capture2.template.chars().count() <= MAX_CAPTURED_TEMPLATE_LEN);
        // The truncation window keeps the leading ref + drops the trailing pad.
        assert_eq!(capture2.template.chars().count(), MAX_CAPTURED_TEMPLATE_LEN);
        assert!(capture2.refs.contains_key("input"));
    }

    #[test]
    fn replay_is_offline_and_exact_for_live_shapes() {
        // Reproduces the live-substitution contract for the engine's template
        // syntax, purely from recorded bindings.
        let input = CapturedNodeInput {
            template: "{{input}}|{{m1.score}}|{{variables.REGION}}".to_string(),
            refs: BTreeMap::from([
                ("input".to_string(), "hello".to_string()),
                ("m1.score".to_string(), "0.98".to_string()),
                ("variables.REGION".to_string(), "us-east".to_string()),
            ]),
        };
        assert_eq!(
            replay_transform(&input).unwrap(),
            "hello|0.98|us-east"
        );
    }

    #[test]
    fn replay_handles_unclosed_braces_and_repeated_refs() {
        let input = CapturedNodeInput {
            template: "{{input}}oooo{{input".to_string(),
            refs: BTreeMap::from([("input".to_string(), "hi".to_string())]),
        };
        // Unclosed `{{` is passed through as-is; the unclosed token is NOT a
        // missing ref (live substitution treats it literally too).
        assert_eq!(replay_transform(&input).unwrap(), "hioooo{{input");
    }

    #[test]
    fn replay_reports_missing_ref_instead_of_blanking() {
        let input = CapturedNodeInput {
            template: "{{m1.score}} done".to_string(),
            refs: BTreeMap::new(), // truncated at record time
        };
        assert_eq!(
            replay_transform(&input),
            Err(ReplayError::MissingRef {
                name: "m1.score".to_string()
            })
        );
    }

    #[test]
    fn capture_then_replay_is_identity_for_trimmed_tokens() {
        // `{{ input }}` has whitespace but captures keyed on the trimmed ref;
        // replay of that capture reproduces the documented value.
        let template = "value={{ input }}";
        let capture = capture_node_input(template, resolve_static).expect("has a ref");
        assert!(capture.refs.contains_key("input"));
        assert_eq!(replay_transform(&capture).unwrap(), "value=hello");
    }
}
