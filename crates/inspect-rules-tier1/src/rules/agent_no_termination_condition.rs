//! Rule: `agent-no-termination-condition`
//!
//! Detects agent loops (while True / for _ in itertools.count() / equivalent)
//! that involve LLM tool calls but have no visible iteration cap or break
//! condition. Runaway loops are a critical cost risk.

use tt_inspect_core::ast::infinite_loop_lines;
use tt_inspect_core::parse::parse_cached;
use tt_inspect_core::{Finding, Language, Rule, Severity};

/// Fires when an agent loop pattern is found without a clear termination
/// condition or iteration cap.
pub struct AgentNoTerminationConditionRule;

impl AgentNoTerminationConditionRule {
    /// Create a new instance of this rule.
    pub fn new() -> Self {
        Self
    }
}

impl Default for AgentNoTerminationConditionRule {
    fn default() -> Self {
        Self::new()
    }
}

/// Patterns indicating agent / tool-call usage.
const AGENT_PATTERNS: &[&str] = &[
    "tool_call",
    "function_call",
    "tool_use",
    "crewai",
    "langgraph",
    "autogen",
    "langchain.agents",
    "AgentExecutor",
    "tool_calls",
    "use_tools",
    "execute_tool",
];

/// Patterns indicating a termination safeguard.
const TERMINATION_PATTERNS: &[&str] = &[
    "max_iterations",
    "max_steps",
    "iteration_limit",
    "step_limit",
    "max_turns",
    "iterations >=",
    "iterations >",
    "step_count >=",
    "step_count >",
    "iteration_count >=",
    "iteration_count >",
    "count >=",
    "count >",
    "if iteration",
    "if step",
    "budget",
    "timeout",
    "max_retries",
];

/// Return `true` when the file path indicates a test fixture.
fn is_test_fixture(path: &str) -> bool {
    path.contains("/tests/")
        || path.contains("/should-detect/")
        || path.contains("/should-not-detect/")
}

impl Rule for AgentNoTerminationConditionRule {
    fn id(&self) -> &'static str {
        "agent-no-termination-condition"
    }

    fn severity(&self) -> Severity {
        Severity::Critical
    }

    fn supported_languages(&self) -> &'static [Language] {
        &[Language::Python, Language::Typescript, Language::Javascript]
    }

    fn check(&self, source: &str, language: Language, path: &str) -> Vec<Finding> {
        if is_test_fixture(path) {
            return vec![];
        }

        // File must have an agent/tool-call pattern.
        let has_agent = AGENT_PATTERNS.iter().any(|p| source.contains(p));
        if !has_agent {
            return vec![];
        }

        // AST-backed loop detection: find real unbounded loops (`while True`,
        // `for(;;)`, `itertools.count()`) — a `while True` in a comment/string
        // no longer triggers, and the reported line is the actual loop.
        let Ok(tree) = parse_cached(source, language) else {
            return vec![];
        };
        let Some(&loop_line) = infinite_loop_lines(&tree, source).first() else {
            return vec![];
        };

        // Termination safeguards are lexical/semantic (counter names, budget /
        // timeout guards), so they stay text-based over the whole file. A bare
        // `break` does NOT count — a model-decided exit can still run away;
        // only a counter-based cap or explicit limit is a safeguard.
        let has_termination = TERMINATION_PATTERNS.iter().any(|p| source.contains(p));
        if has_termination {
            return vec![];
        }
        let has_break_with_count = source.lines().any(|l| {
            l.contains("break") && (l.contains("count") || l.contains("iter") || l.contains("step"))
        });
        if has_break_with_count {
            return vec![];
        }

        vec![Finding {
            rule_id: self.id().to_string(),
            severity: self.severity(),
            file: path.to_string(),
            line: loop_line,
            message: "Agent loop detected without a visible iteration cap or termination \
                       condition. An unbounded agent loop can run indefinitely and accumulate \
                       significant LLM costs."
                .to_string(),
            confidence: 0.7,
            fix_hint: Some(
                "Add a max_iterations / max_steps counter and break when exceeded. \
                 Also consider a per-invocation cost budget and timeout circuit breaker."
                    .to_string(),
            ),
        }]
    }
}
