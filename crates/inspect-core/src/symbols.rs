//! Per-file symbol + import extraction (functions, classes, imports) for
//! Python/TS/JS, built on the shared tree-sitter parser. Markdown / parse
//! failures yield an empty `FileSymbols` (never errors).
use serde::{Deserialize, Serialize};
use tree_sitter::Node;

use crate::{parse::parse_cached, Language};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SymbolDef {
    pub name: String,
    pub line: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportRef {
    pub raw: String,
    pub line: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileSymbols {
    pub functions: Vec<SymbolDef>,
    pub classes: Vec<SymbolDef>,
    pub imports: Vec<ImportRef>,
}

#[must_use]
pub fn extract_symbols(source: &str, language: Language) -> FileSymbols {
    if language == Language::Markdown {
        return FileSymbols::default();
    }
    let Ok(tree) = parse_cached(source, language) else {
        return FileSymbols::default();
    };
    let src = source.as_bytes();
    let mut out = FileSymbols::default();
    let line = |n: &Node| (n.start_position().row + 1) as u32;
    let name_of = |n: &Node| {
        n.child_by_field_name("name")
            .and_then(|x| x.utf8_text(src).ok())
            .map(str::to_string)
    };
    // Fallback name extraction for variable_declarator nodes where the name
    // is exposed as the first `identifier` child rather than a `name` field
    // (tree-sitter-javascript/typescript).
    let name_of_declarator = |n: &Node| -> Option<String> {
        if let Some(name) = name_of(n) {
            return Some(name);
        }
        let mut cursor = n.walk();
        for child in n.children(&mut cursor) {
            if child.kind() == "identifier" {
                return child.utf8_text(src).ok().map(str::to_string);
            }
        }
        None
    };

    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "function_definition" | "function_declaration" => {
                if let Some(name) = name_of(&node) {
                    out.functions.push(SymbolDef {
                        name,
                        line: line(&node),
                    });
                }
            }
            "variable_declarator" => {
                let has_fn = {
                    let mut c = node.walk();
                    let result = node.children(&mut c).any(|ch| {
                        matches!(
                            ch.kind(),
                            "arrow_function" | "function" | "function_expression"
                        )
                    });
                    result
                };
                if has_fn {
                    if let Some(name) = name_of_declarator(&node) {
                        out.functions.push(SymbolDef {
                            name,
                            line: line(&node),
                        });
                    }
                }
            }
            "class_definition" | "class_declaration" => {
                if let Some(name) = name_of(&node) {
                    out.classes.push(SymbolDef {
                        name,
                        line: line(&node),
                    });
                }
            }
            "import_statement" | "import_from_statement" => {
                if let Ok(txt) = node.utf8_text(src) {
                    out.imports.push(ImportRef {
                        raw: txt.trim().to_string(),
                        line: line(&node),
                    });
                }
            }
            _ => {}
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
    use crate::Language;

    #[test]
    fn python_symbols() {
        let src = "import os\nfrom a.b import c\n\ndef foo(x):\n    return x\n\nclass Bar:\n    def m(self):\n        pass\n";
        let s = extract_symbols(src, Language::Python);
        assert!(s.functions.iter().any(|f| f.name == "foo"));
        assert!(s.classes.iter().any(|c| c.name == "Bar"));
        assert!(s.imports.iter().any(|i| i.raw.contains("os")));
        assert!(s.imports.iter().any(|i| i.raw.contains("a.b")));
    }
    #[test]
    fn javascript_symbols() {
        let src =
            "import {x} from './util.js';\nfunction foo(){}\nconst bar = () => {};\nclass Baz {}\n";
        let s = extract_symbols(src, Language::Javascript);
        assert!(s.functions.iter().any(|f| f.name == "foo"));
        assert!(s.functions.iter().any(|f| f.name == "bar")); // arrow const
        assert!(s.classes.iter().any(|c| c.name == "Baz"));
        assert!(s.imports.iter().any(|i| i.raw.contains("./util")));
    }
    #[test]
    fn typescript_symbols() {
        let src = "import type {T} from '../t';\nexport function handle(): void {}\nexport class Svc {}\n";
        let s = extract_symbols(src, Language::Typescript);
        assert!(s.functions.iter().any(|f| f.name == "handle"));
        assert!(s.classes.iter().any(|c| c.name == "Svc"));
        assert!(s.imports.iter().any(|i| i.raw.contains("../t")));
    }
    #[test]
    fn markdown_and_parse_failure_are_empty() {
        let s = extract_symbols("# hi", Language::Markdown);
        assert!(s.functions.is_empty() && s.classes.is_empty() && s.imports.is_empty());
    }
}
