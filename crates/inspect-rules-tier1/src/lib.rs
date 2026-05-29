//! The 19 P0 rules that ship at launch. See `docs/01-inspect-rule-catalog.md`.

pub mod rules;

pub use rules::AgentNoTerminationConditionRule;
pub use rules::CacheAnthropicPromptCacheMissingRule;
pub use rules::CacheAnthropicToolsNotCachedRule;
pub use rules::CacheOpenaiPromptCacheEligibleRule;
pub use rules::ConfigAgentsMdContainsSecretsRule;
pub use rules::ConfigAgentsMdTooLongRule;
pub use rules::ConfigNoAgentsMdRule;
pub use rules::ConversationUnboundedHistoryRule;
pub use rules::LibAnthropicSdkNoCacheControlRule;
pub use rules::ModelDeprecatedRule;
pub use rules::ModelFlagshipForClassificationRule;
pub use rules::ModelFlagshipForExtractionRule;
pub use rules::ModelReasoningEffortDefaultHighRule;
pub use rules::OutputNGreaterThanOneRule;
pub use rules::OutputNoMaxTokensRule;
pub use rules::PromptBloatedSystemRule;
pub use rules::PromptDynamicPrefixBreaksCacheRule;
pub use rules::PromptNoOutputConstraintRule;
pub use rules::PromptVerboseFewShotRule;

use tt_inspect_core::{Finding, Language, Rule, Severity};

/// Return every production Tier-1 rule.
///
/// The CLI registers these at startup via [`tt_inspect_core::Engine::add_rule`].
/// [`TestRule`] is **not** included — it is test-only.
pub fn all_rules() -> Vec<Box<dyn Rule>> {
    vec![
        Box::new(CacheAnthropicPromptCacheMissingRule::new()),
        Box::new(CacheOpenaiPromptCacheEligibleRule::new()),
        Box::new(LibAnthropicSdkNoCacheControlRule::new()),
        Box::new(ModelFlagshipForClassificationRule::new()),
        Box::new(ModelFlagshipForExtractionRule::new()),
        Box::new(ModelDeprecatedRule::new()),
        Box::new(OutputNoMaxTokensRule::new()),
        Box::new(PromptNoOutputConstraintRule::new()),
        Box::new(PromptBloatedSystemRule::new()),
        Box::new(PromptVerboseFewShotRule::new()),
        Box::new(ConversationUnboundedHistoryRule::new()),
        Box::new(AgentNoTerminationConditionRule::new()),
        Box::new(ConfigNoAgentsMdRule::new()),
        Box::new(ConfigAgentsMdContainsSecretsRule::new()),
        Box::new(ConfigAgentsMdTooLongRule::new()),
        // Cost/cache rules added in inspect-new-rules.
        Box::new(CacheAnthropicToolsNotCachedRule::new()),
        Box::new(OutputNGreaterThanOneRule::new()),
        Box::new(ModelReasoningEffortDefaultHighRule::new()),
        Box::new(PromptDynamicPrefixBreaksCacheRule::new()),
    ]
}

// ---------------------------------------------------------------------------
// TestRule — exists only to prove the engine end-to-end in tests.
// ---------------------------------------------------------------------------

/// Trivial rule that fires whenever source contains the literal token
/// `// TT_TEST_FIRE`.
///
/// This rule is **not** registered in [`all_rules()`] — it is test-only.
pub struct TestRule;

impl Rule for TestRule {
    fn id(&self) -> &'static str {
        "test-fire"
    }

    fn severity(&self) -> Severity {
        Severity::Low
    }

    fn supported_languages(&self) -> &'static [Language] {
        &[Language::Python, Language::Typescript, Language::Javascript]
    }

    fn check(&self, source: &str, language: Language, path: &str) -> Vec<Finding> {
        source
            .lines()
            .enumerate()
            .filter(|(_, l)| l.contains("// TT_TEST_FIRE"))
            .map(|(idx, l)| Finding {
                rule_id: "test-fire".into(),
                severity: Severity::Low,
                file: path.to_string(),
                line: (idx + 1) as u32,
                message: format!(
                    "TT_TEST_FIRE marker found in {language:?} file: {}",
                    l.trim()
                ),
                confidence: 1.0,
                fix_hint: Some("Remove the // TT_TEST_FIRE marker.".into()),
            })
            .collect()
    }
}
