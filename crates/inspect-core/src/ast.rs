//! AST helpers shared by the structural rules.
//!
//! [`call_sites`] extracts every function/method call in a parsed [`Tree`] as a
//! [`CallSite`] — the callee's dotted text, the call's full source span, and
//! its line. Rules match on the callee (e.g. `…messages.create`) and inspect
//! the *span text* for arguments. Because the span comes from a real `call`
//! node, a `messages.create` (or `max_tokens`) appearing in a comment or an
//! unrelated string literal no longer triggers — the headline robustness win
//! over the previous line-window substring scans.

use tree_sitter::Tree;

/// A single call expression found in the AST.
#[derive(Debug, Clone)]
pub struct CallSite {
    /// The callee text, e.g. `client.chat.completions.create` or
    /// `model.generate_content`. Empty if the callee text could not be read.
    pub callee: String,
    /// The full source span of the call, including its argument list.
    pub text: String,
    /// 1-based line of the call's start.
    pub line: u32,
}

impl CallSite {
    /// `true` if the call's argument span contains `needle` (an AST-scoped
    /// substring check — only matches text inside this call).
    pub fn arg_contains(&self, needle: &str) -> bool {
        self.text.contains(needle)
    }
}

/// `true` when `callee` is an LLM chat/generation create call, matched on the
/// dotted callee text from the AST (anchored at the end), e.g.
/// `client.chat.completions.create`, `client.messages.create`,
/// `client.responses.create`, or `model.generate_content`.
pub fn is_llm_create_callee(callee: &str) -> bool {
    callee.ends_with("chat.completions.create")
        || callee.ends_with("completions.create")
        // Legacy OpenAI text-completions endpoint (`openai.Completion.create`).
        || callee.ends_with("Completion.create")
        || callee.ends_with("messages.create")
        || callee.ends_with("responses.create")
        || callee.ends_with("generate_content")
        || callee.ends_with("generateContent")
}

/// Lines (1-based, ascending) of every *unbounded* loop construct in `tree`:
/// a `while` whose condition is the literal `true`/`True`, a C-style
/// `for (;;)`, or a Python `for … in itertools.count(…)`. Detection is on real
/// loop nodes, so a `while True` written in a comment or string is ignored.
pub fn infinite_loop_lines(tree: &Tree, source: &str) -> Vec<u32> {
    let src = source.as_bytes();
    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "while_statement" => {
                if let Some(cond) = node.child_by_field_name("condition") {
                    let norm = cond
                        .utf8_text(src)
                        .unwrap_or("")
                        .trim()
                        .trim_start_matches('(')
                        .trim_end_matches(')')
                        .trim()
                        .to_ascii_lowercase();
                    if norm == "true" {
                        out.push(node.start_position().row as u32 + 1);
                    }
                }
            }
            "for_statement" => {
                let text = node.utf8_text(src).unwrap_or("");
                let header = text.lines().next().unwrap_or("");
                let compact: String = header.chars().filter(|c| !c.is_whitespace()).collect();
                // Python `itertools.count()` iterable, or C-style empty `for(;;)`.
                if text.contains("itertools.count(") || compact.contains("(;;)") {
                    out.push(node.start_position().row as u32 + 1);
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Walk `tree` and return every call site (Python `call`, JS/TS
/// `call_expression`). Order follows a depth-first traversal.
pub fn call_sites(tree: &Tree, source: &str) -> Vec<CallSite> {
    let src = source.as_bytes();
    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "call" || node.kind() == "call_expression" {
            if let Some(func) = node.child_by_field_name("function") {
                out.push(CallSite {
                    callee: func.utf8_text(src).unwrap_or("").to_string(),
                    text: node.utf8_text(src).unwrap_or("").to_string(),
                    line: (node.start_position().row + 1) as u32,
                });
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_cached;
    use crate::Language;

    #[test]
    fn finds_python_method_call_and_callee() {
        let src = "resp = client.chat.completions.create(model='gpt-4o', max_tokens=10)\n";
        let tree = parse_cached(src, Language::Python).unwrap();
        let calls = call_sites(&tree, src);
        let create = calls
            .iter()
            .find(|c| c.callee.ends_with(".create"))
            .expect("should find the create call");
        assert_eq!(create.callee, "client.chat.completions.create");
        assert!(create.arg_contains("max_tokens"));
        assert_eq!(create.line, 1);
    }

    #[test]
    fn finds_ts_call_expression() {
        let src = "const r = await client.messages.create({ model: 'claude' });\n";
        let tree = parse_cached(src, Language::Typescript).unwrap();
        let calls = call_sites(&tree, src);
        assert!(calls.iter().any(|c| c.callee.ends_with("messages.create")));
    }

    #[test]
    fn detects_infinite_loops_only() {
        let py = "while True:\n    work()\nfor i in range(3):\n    pass\nwhile done:\n    pass\n";
        let tree = parse_cached(py, Language::Python).unwrap();
        // Only the `while True:` on line 1 is unbounded.
        assert_eq!(infinite_loop_lines(&tree, py), vec![1]);
    }

    #[test]
    fn detects_itertools_count_and_ignores_comment() {
        let py = "# while True is fine in a comment\nfor _ in itertools.count():\n    step()\n";
        let tree = parse_cached(py, Language::Python).unwrap();
        assert_eq!(infinite_loop_lines(&tree, py), vec![2]);
    }

    #[test]
    fn detects_js_while_true() {
        let js = "while (true) {\n  tick();\n}\n";
        let tree = parse_cached(js, Language::Javascript).unwrap();
        assert_eq!(infinite_loop_lines(&tree, js), vec![1]);
    }

    #[test]
    fn ignores_calls_mentioned_in_comments_and_strings() {
        // A comment and a string both mention messages.create; neither is a
        // real call node, so neither appears as a CallSite.
        let src = "# client.messages.create(...) is what we used to do\n\
                   doc = \"call client.messages.create here\"\n\
                   real = other.thing()\n";
        let tree = parse_cached(src, Language::Python).unwrap();
        let calls = call_sites(&tree, src);
        assert!(
            !calls.iter().any(|c| c.callee.contains("messages.create")),
            "comment/string mentions must not be reported as call sites"
        );
        assert!(calls.iter().any(|c| c.callee == "other.thing"));
    }
}
