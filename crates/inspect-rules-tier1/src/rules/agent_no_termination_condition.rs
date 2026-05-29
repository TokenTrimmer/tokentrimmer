//! Rule: `agent-no-termination-condition`
//!
//! Detects agent loops (while True / for _ in itertools.count() / equivalent)
//! that involve LLM tool calls but have no visible iteration cap or break
//! condition. Runaway loops are a critical cost risk.

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

/// Patterns indicating an unbounded agent loop.
const LOOP_PATTERNS: &[&str] = &[
    "while True:",
    "while true {",
    "while(true)",
    "for _ in itertools.count(",
    "for(;;)",
    "while not done:",
    "while not finished:",
    "while not complete:",
    "while not stop:",
    "while is_running",
    "while running:",
    "while agent_running",
];

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

    fn check(&self, source: &str, _language: Language, path: &str) -> Vec<Finding> {
        if is_test_fixture(path) {
            return vec![];
        }

        // File must have an agent/tool-call pattern.
        let has_agent = AGENT_PATTERNS.iter().any(|p| source.contains(p));
        if !has_agent {
            return vec![];
        }

        // Find the first unbounded loop pattern.
        let loop_line = LOOP_PATTERNS.iter().find_map(|p| {
            source
                .lines()
                .enumerate()
                .find(|(_, l)| l.contains(p))
                .map(|(i, _)| i)
        });
        let Some(loop_line_idx) = loop_line else {
            return vec![];
        };

        // Check for termination safeguards in the entire file.
        let has_termination = TERMINATION_PATTERNS.iter().any(|p| source.contains(p));
        if has_termination {
            return vec![];
        }

        // Also accept a `break` that has a counter reference nearby.
        // Simple heuristic: any `break` with a counter variable.
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
            line: (loop_line_idx + 1) as u32,
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
