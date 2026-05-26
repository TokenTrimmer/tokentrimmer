//! Rule engine: walks a directory, feeds each file to every applicable rule,
//! and aggregates the resulting [`Finding`]s.

use std::path::Path;

use crate::{walk, Finding, Rule};

/// The central analysis engine.
///
/// Build an `Engine`, register rules with [`add_rule`][Engine::add_rule] or
/// the builder-style [`with_rule`][Engine::with_rule], then call
/// [`scan`][Engine::scan] to run all registered rules against every eligible
/// source file under a given path.
///
/// # Example
///
/// ```rust,no_run
/// use tt_inspect_core::{Engine, Language, Rule, Finding, Severity};
///
/// struct NoOpRule;
/// impl Rule for NoOpRule {
///     fn id(&self) -> &'static str { "no-op" }
///     fn severity(&self) -> Severity { Severity::Low }
///     fn supported_languages(&self) -> &'static [Language] { &[Language::Python] }
///     fn check(&self, _src: &str, _lang: Language, _path: &str) -> Vec<Finding> { vec![] }
/// }
///
/// let findings = Engine::new()
///     .with_rule(Box::new(NoOpRule))
///     .scan(std::path::Path::new("."));
/// ```
pub struct Engine {
    rules: Vec<Box<dyn Rule>>,
}

impl Engine {
    /// Create an engine with no rules registered.
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// Builder-style method: add a rule and return `self`.
    pub fn with_rule(mut self, rule: Box<dyn Rule>) -> Self {
        self.rules.push(rule);
        self
    }

    /// Mutating method: add a rule to an already-constructed engine.
    pub fn add_rule(&mut self, rule: Box<dyn Rule>) {
        self.rules.push(rule);
    }

    /// Scan all eligible source files under `path`, running every registered
    /// rule against each file whose language is supported by that rule.
    ///
    /// Errors at the individual-file level (read errors, binary content) are
    /// silently skipped so that a single problem file never aborts the scan.
    /// Parse errors inside rules are similarly the rule's responsibility to
    /// handle gracefully.
    pub fn scan(&self, path: &Path) -> Vec<Finding> {
        let mut all: Vec<Finding> = Vec::new();

        for (file_path, lang) in walk::walk(path) {
            // Skip files we cannot read as UTF-8 — they're likely binary or
            // have an encoding issue. Not a fatal error.
            let source = match std::fs::read_to_string(&file_path) {
                Ok(s) => s,
                Err(_) => continue,
            };

            let path_str = file_path.to_string_lossy();

            for rule in &self.rules {
                // Only run rules that advertise support for this file's language.
                if !rule.supported_languages().contains(&lang) {
                    continue;
                }
                let mut findings = rule.check(&source, lang, &path_str);
                all.append(&mut findings);
            }
        }

        all
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}
