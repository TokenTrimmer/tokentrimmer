//! Deterministic sliding-window agent tool turn compactor.
//!
//! Autonomous coding agent loops (e.g. Claude Code, Cursor, Windsurf, OMP)
//! rapidly accumulate bulky tool outputs across turns (such as large directory walks,
//! verbose bash stdout/stderr, repeated test runs, or full file reads).
//!
//! This module compacts stale tool outputs older than a configurable window (`keep_recent_turns`)
//! into structured, token-efficient outcome summaries, while leaving recent turns
//! completely verbatim.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Configuration options for agent tool turn window compaction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactorConfig {
    /// Number of most recent turns to leave completely untouched (verbatim).
    pub keep_recent_turns: usize,
    /// Max length in characters for stale tool outputs before compacting.
    pub stale_char_limit: usize,
    /// Whether to compact stdout/stderr blocks into truncated head/tail snippets.
    pub compact_stdout_tail: bool,
}

impl Default for CompactorConfig {
    fn default() -> Self {
        Self {
            keep_recent_turns: 3,
            stale_char_limit: 250,
            compact_stdout_tail: true,
        }
    }
}

/// A single message representation for multi-turn agent transcripts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranscriptTurn {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

/// Compaction outcome stats.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompactionStats {
    pub original_turns: usize,
    pub compacted_turns: usize,
    pub chars_before: usize,
    pub chars_after: usize,
    pub estimated_tokens_saved: u32,
}

/// Compact stale tool execution turns into concise summaries.
#[must_use]
pub fn compact_tool_turns(
    turns: &[TranscriptTurn],
    config: &CompactorConfig,
) -> (Vec<TranscriptTurn>, CompactionStats) {
    if turns.is_empty() {
        return (Vec::new(), CompactionStats::default());
    }

    let mut out = Vec::with_capacity(turns.len());
    let mut chars_before = 0;
    let mut chars_after = 0;
    let mut compacted_count = 0;

    let total = turns.len();
    let verbatim_cutoff = total.saturating_sub(config.keep_recent_turns);

    for (idx, turn) in turns.iter().enumerate() {
        let is_stale_tool =
            idx < verbatim_cutoff && (turn.role == "tool" || turn.tool_call_id.is_some());
        chars_before += turn.content.len();

        if is_stale_tool && turn.content.len() > config.stale_char_limit {
            let compacted_content =
                summarize_stale_tool_content(&turn.content, turn.tool_name.as_deref());
            chars_after += compacted_content.len();
            compacted_count += 1;

            out.push(TranscriptTurn {
                role: turn.role.clone(),
                content: compacted_content,
                tool_call_id: turn.tool_call_id.clone(),
                tool_name: turn.tool_name.clone(),
            });
        } else {
            chars_after += turn.content.len();
            out.push(turn.clone());
        }
    }

    let tokens_before = tt_tokenize::estimate_tokens(
        "openai",
        &turns
            .iter()
            .map(|t| t.content.as_str())
            .collect::<Vec<_>>()
            .join(" "),
    );
    let tokens_after = tt_tokenize::estimate_tokens(
        "openai",
        &out.iter()
            .map(|t| t.content.as_str())
            .collect::<Vec<_>>()
            .join(" "),
    );
    let estimated_tokens_saved = tokens_before.saturating_sub(tokens_after);

    let stats = CompactionStats {
        original_turns: total,
        compacted_turns: compacted_count,
        chars_before,
        chars_after,
        estimated_tokens_saved,
    };

    (out, stats)
}

/// Summarize large stale tool responses (JSON, bash output, or file reads) into a compact representation.
fn summarize_stale_tool_content(content: &str, tool_name: Option<&str>) -> String {
    // If tool is a read_file tool or contains a path, attempt AST skeletonization
    if tool_name == Some("read_file") || tool_name == Some("read") {
        if let Ok(val) = serde_json::from_str::<Value>(content) {
            if let Some(obj) = val.as_object() {
                if let (Some(path_val), Some(content_val)) = (obj.get("path"), obj.get("content")) {
                    if let (Some(path_str), Some(src_str)) =
                        (path_val.as_str(), content_val.as_str())
                    {
                        let ext = path_str.rsplit('.').next().unwrap_or("");
                        if let Some(lang) = tt_inspect_core::Language::from_extension(ext) {
                            let skel = crate::skeletonizer::skeletonize_source(src_str, lang);
                            let mut res_map = obj.clone();
                            res_map.insert("content".to_string(), Value::String(skel));
                            res_map.insert("tt_skeletonized".to_string(), Value::Bool(true));
                            return serde_json::to_string(&Value::Object(res_map))
                                .unwrap_or_else(|_| content.to_string());
                        }
                    }
                }
            }
        }
    }

    // If it's valid JSON, try to extract error or summary indicators
    if let Ok(val) = serde_json::from_str::<Value>(content) {
        if let Some(obj) = val.as_object() {
            let mut summary_map = serde_json::Map::new();
            summary_map.insert("tt_compacted".to_string(), Value::Bool(true));
            if let Some(name) = tool_name {
                summary_map.insert("tool".to_string(), Value::String(name.to_string()));
            }

            if let Some(err) = obj.get("error").or_else(|| obj.get("err")) {
                summary_map.insert("error".to_string(), err.clone());
            }
            if let Some(exit_code) = obj
                .get("exit_code")
                .or_else(|| obj.get("exitCode"))
                .or_else(|| obj.get("status"))
            {
                summary_map.insert("exit_code".to_string(), exit_code.clone());
            }
            if let Some(res) = obj
                .get("result")
                .or_else(|| obj.get("output"))
                .or_else(|| obj.get("stdout"))
            {
                if let Some(s) = res.as_str() {
                    let truncated = truncate_snippet(s, 100);
                    summary_map.insert("output_snippet".to_string(), Value::String(truncated));
                }
            }

            if summary_map.len() > 1 {
                return serde_json::to_string(&Value::Object(summary_map))
                    .unwrap_or_else(|_| content.to_string());
            }
        }
    }

    // Plaintext / log output fallback
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() > 6 {
        let first_two = lines[..2].join("\n");
        let last_two = lines[lines.len() - 2..].join("\n");
        format!(
            "{}\n[... {} lines compacted by TokenTrimmer ...]\n{}",
            first_two,
            lines.len().saturating_sub(4),
            last_two
        )
    } else {
        truncate_snippet(content, 180)
    }
}

fn truncate_snippet(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}[... truncated by TokenTrimmer]", &s[..max_len])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compact_tool_turns_leaves_recent_turns_verbatim() {
        let mut turns = Vec::new();
        for i in 0..6 {
            turns.push(TranscriptTurn {
                role: "tool".to_string(),
                content: format!(
                    "Tool output turn {i} with very long verbose text payload repeated: {}",
                    "x".repeat(500)
                ),
                tool_call_id: Some(format!("call_{i}")),
                tool_name: Some("bash".to_string()),
            });
        }

        let config = CompactorConfig {
            keep_recent_turns: 2,
            stale_char_limit: 100,
            compact_stdout_tail: true,
        };

        let (compacted, stats) = compact_tool_turns(&turns, &config);
        assert_eq!(compacted.len(), 6);
        assert_eq!(stats.compacted_turns, 4);
        assert!(stats.chars_after < stats.chars_before);
        assert!(stats.estimated_tokens_saved > 0);

        // Turn 0-3 must be compacted
        assert!(
            compacted[0].content.contains("compacted")
                || compacted[0].content.contains("truncated")
        );
        // Turn 4 and 5 must remain verbatim
        assert_eq!(compacted[4].content, turns[4].content);
        assert_eq!(compacted[5].content, turns[5].content);
    }

    #[test]
    fn test_compact_json_tool_output() {
        let json_body = serde_json::json!({
            "status": "error",
            "error": "File not found: src/unknown.rs",
            "stacktrace": "a".repeat(1000),
            "exit_code": 1
        })
        .to_string();

        let turns = vec![
            TranscriptTurn {
                role: "tool".to_string(),
                content: json_body,
                tool_call_id: Some("call_0".to_string()),
                tool_name: Some("read_file".to_string()),
            },
            TranscriptTurn {
                role: "assistant".to_string(),
                content: "I will retry.".to_string(),
                tool_call_id: None,
                tool_name: None,
            },
        ];

        let config = CompactorConfig {
            keep_recent_turns: 1,
            stale_char_limit: 50,
            compact_stdout_tail: true,
        };

        let (compacted, stats) = compact_tool_turns(&turns, &config);
        assert_eq!(stats.compacted_turns, 1);
        assert!(compacted[0]
            .content
            .contains("File not found: src/unknown.rs"));
        assert!(compacted[0].content.contains("tt_compacted"));
    }
}
