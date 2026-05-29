//! All 15 P0 Inspect rules shipped at launch.
//!
//! Each rule lives in its own submodule. The public struct for each rule is
//! re-exported here for convenience.

pub mod agent_no_termination_condition;
pub mod cache_anthropic_prompt_cache_missing;
pub mod cache_openai_prompt_cache_eligible;
pub mod config_agents_md_contains_secrets;
pub mod config_agents_md_too_long;
pub mod config_no_agents_md;
pub mod conversation_unbounded_history;
pub mod lib_anthropic_sdk_no_cache_control;
pub mod model_deprecated;
pub mod model_flagship_for_classification;
pub mod model_flagship_for_extraction;
pub mod output_no_max_tokens;
pub mod prompt_bloated_system;
pub mod prompt_no_output_constraint;
pub mod prompt_verbose_few_shot;

pub use agent_no_termination_condition::AgentNoTerminationConditionRule;
pub use cache_anthropic_prompt_cache_missing::CacheAnthropicPromptCacheMissingRule;
pub use cache_openai_prompt_cache_eligible::CacheOpenaiPromptCacheEligibleRule;
pub use config_agents_md_contains_secrets::ConfigAgentsMdContainsSecretsRule;
pub use config_agents_md_too_long::ConfigAgentsMdTooLongRule;
pub use config_no_agents_md::ConfigNoAgentsMdRule;
pub use conversation_unbounded_history::ConversationUnboundedHistoryRule;
pub use lib_anthropic_sdk_no_cache_control::LibAnthropicSdkNoCacheControlRule;
pub use model_deprecated::ModelDeprecatedRule;
pub use model_flagship_for_classification::ModelFlagshipForClassificationRule;
pub use model_flagship_for_extraction::ModelFlagshipForExtractionRule;
pub use output_no_max_tokens::OutputNoMaxTokensRule;
pub use prompt_bloated_system::PromptBloatedSystemRule;
pub use prompt_no_output_constraint::PromptNoOutputConstraintRule;
pub use prompt_verbose_few_shot::PromptVerboseFewShotRule;
