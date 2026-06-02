//! Rule: `agent-no-termination-condition`
//!
//! Detects agent loops (while True / for _ in itertools.count() / equivalent)
//! that involve LLM tool calls but have no visible iteration cap or break
//! condition. Runaway loops are a critical cost risk.

use tt_inspect_core::ast::infinite_loops_with_bodies;
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

        // AST-backed loop detection: find all unbounded loops (`while True`,
        // `for(;;)`, `itertools.count()`) with their body spans.
        let Ok(tree) = parse_cached(source, language) else {
            return vec![];
        };
        let loops = infinite_loops_with_bodies(&tree, source);
        if loops.is_empty() {
            return vec![];
        }

        let mut findings = vec![];
        let lines: Vec<&str> = source.lines().collect();

        for loop_info in loops {
            // Extract loop body text (scoped to this specific loop).
            let loop_body_text =
                extract_loop_body(&lines, loop_info.body_start_line, loop_info.body_end_line);

            // Check for termination safeguards *within this loop's body only*.
            if has_termination_in_body(&loop_body_text) {
                continue;
            }

            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: self.severity(),
                file: path.to_string(),
                line: loop_info.line,
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
            });
        }

        findings
    }
}

/// Extract text from a loop's body (spanning lines [start, end]).
fn extract_loop_body(lines: &[&str], start_line: u32, end_line: u32) -> String {
    let start_idx = (start_line.saturating_sub(1)) as usize;
    let end_idx = (end_line as usize).min(lines.len());
    lines[start_idx..end_idx].join("\n")
}

/// Check if a loop body contains termination safeguards (scoped to that body).
fn has_termination_in_body(body: &str) -> bool {
    // Strong patterns: explicit counters and iteration limits.
    const STRONG_PATTERNS: &[&str] = &[
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
        "max_retries",
    ];

    if STRONG_PATTERNS.iter().any(|p| body.contains(p)) {
        return true;
    }

    // Weak patterns: require closer context (assignment + comparison in same loop).
    // "budget" or "timeout" alone is too generic; look for assignments/comparisons.
    if body.contains("budget") {
        // Check if there's a pattern like `spent > budget` or `budget =` nearby.
        let has_budget_check = body.lines().any(|line| {
            (line.contains("spent") || line.contains("cost") || line.contains("expense"))
                && line.contains("budget")
                && (line.contains(">")
                    || line.contains("<")
                    || line.contains(">=")
                    || line.contains("<="))
        });
        if has_budget_check {
            return true;
        }
    }

    if body.contains("timeout") {
        // Check for actual timeout usage like `time.time() - start > timeout`.
        let has_timeout_check = body.lines().any(|line| {
            (line.contains("time.time()") || line.contains("time()") || line.contains("elapsed"))
                && line.contains("timeout")
                && (line.contains(">")
                    || line.contains("<")
                    || line.contains(">=")
                    || line.contains("<="))
        });
        if has_timeout_check {
            return true;
        }
    }

    false
}
