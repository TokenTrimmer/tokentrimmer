//! Rule: `agent-runaway-loop-tripwire`
//!
//! Detects agent tool execution loops, retry loops, and unconstrained error cycles
//! that repeat identical tool dispatches or alternate between failing states without
//! backoff, jitter, or explicit max iteration ceilings.

use tt_inspect_core::ast::infinite_loops_with_bodies;
use tt_inspect_core::parse::parse_cached;
use tt_inspect_core::{Finding, Language, Rule, Severity};

/// Fires when an agent loop or tool-execution loop lacks progress guards or retry limits.
pub struct AgentRunawayLoopTripwireRule;

impl AgentRunawayLoopTripwireRule {
    /// Create a new instance of this rule.
    pub fn new() -> Self {
        Self
    }
}

impl Default for AgentRunawayLoopTripwireRule {
    fn default() -> Self {
        Self::new()
    }
}

const AGENT_LOOP_INDICATORS: &[&str] = &[
    "tool_call",
    "execute_tool",
    "call_tool",
    "run_command",
    "step_loop",
    "agent_loop",
    "dispatch_tool",
    "handle_tool_call",
];

fn is_test_fixture(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/should-detect/")
        || path.contains("/should-not-detect/")
}

impl Rule for AgentRunawayLoopTripwireRule {
    fn id(&self) -> &'static str {
        "agent-runaway-loop-tripwire"
    }

    fn severity(&self) -> Severity {
        Severity::High
    }

    fn supported_languages(&self) -> &'static [Language] {
        &[Language::Python, Language::Typescript, Language::Javascript]
    }

    fn check(&self, source: &str, language: Language, path: &str) -> Vec<Finding> {
        if is_test_fixture(path) {
            return vec![];
        }

        let has_indicator = AGENT_LOOP_INDICATORS.iter().any(|ind| source.contains(ind));
        if !has_indicator {
            return vec![];
        }

        let Ok(tree) = parse_cached(source, language) else {
            return vec![];
        };

        let loops = infinite_loops_with_bodies(&tree, source);
        let mut findings = Vec::new();
        let lines: Vec<&str> = source.lines().collect();

        for l in loops {
            let start_idx = (l.body_start_line.saturating_sub(1)) as usize;
            let end_idx = (l.body_end_line as usize).min(lines.len());
            let body_text = lines[start_idx..end_idx].join("\n");

            let calls_tools = AGENT_LOOP_INDICATORS
                .iter()
                .any(|ind| body_text.contains(ind));
            // Budget identifiers appear in many casings (`budget`,
            // `remaining_budget_usd`, `MAX_TURNS`, `BudgetUsd`) — compare on a
            // lowercased copy so every one of them disarms the tripwire.
            let body_lower = body_text.to_lowercase();
            let has_max_turns = [
                "max_turns",
                "max_turn",
                "turn_limit",
                "turn_count",
                "turn >=",
                "turns >=",
                "max_steps",
                "step_limit",
                "step >=",
                "steps >=",
                "max_iterations",
                "max_iteration",
                "iteration_limit",
                "iteration <",
                "iteration >",
                "iterations <",
                "iterations >",
                "budget",
            ]
            .iter()
            .any(|needle| body_lower.contains(needle));

            if calls_tools && !has_max_turns {
                findings.push(Finding {
                    rule_id: self.id().into(),
                    severity: self.severity(),
                    file: path.to_string(),
                    line: l.line,
                    message: "Agent tool loop detected without explicit turn budget, runaway tripwire, or max-step limit.".into(),
                    confidence: 0.92,
                    fix_hint: Some("Guard the tool loop with a max_turns budget or circuit breaker: e.g. `if turn > MAX_TURNS { break; }`".into()),
                });
            }
        }
        findings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_unbounded_agent_tool_loop() {
        let py = r#"
while True:
    tool_call = agent.get_action()
    execute_tool(tool_call)
"#;
        let rule = AgentRunawayLoopTripwireRule::new();
        let findings = rule.check(py, Language::Python, "agent_loop.py");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule_id, "agent-runaway-loop-tripwire");
    }

    #[test]
    fn passes_bounded_agent_tool_loop() {
        let py = r#"
while True:
    if turn_count >= max_turns:
        break
    tool_call = agent.get_action()
    execute_tool(tool_call)
    turn_count += 1
"#;
        let rule = AgentRunawayLoopTripwireRule::new();
        let findings = rule.check(py, Language::Python, "agent_loop.py");
        assert!(findings.is_empty());
    }
}
