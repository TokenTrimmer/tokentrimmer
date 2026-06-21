//! Shared types, errors, and traits for the TokenTrimmer workspace.
//!
//! No business logic lives here — only the contracts that other crates implement.
//! See `docs/02-provider-adapter-guide.md` for the Provider trait.

pub mod batch_advisor;
pub mod capability_check;
pub mod context;
pub mod dns_guard;
pub mod error;
pub mod messages;
pub mod model_aliases;
pub mod model_catalog;
pub mod pricing;
pub mod provider;
pub mod providers;
pub mod url_guard;
pub mod usage;

pub use batch_advisor::{
    project_batch_savings, project_batch_savings_with_tags, BatchFinding, RequestAggregate,
    DEFAULT_BATCH_ELIGIBLE_TAGS,
};
pub use capability_check::{message_text_for_estimation, RequiredCapabilities};
pub use context::{CallerTier, RequestContext};
pub use dns_guard::{with_guarded_dns, GuardedResolveError, GuardedResolver};
pub use error::ProviderError;
pub use messages::{
    parse_cache_control, parse_panel_extras, CacheControlConfig, CacheMode, ChatCompletionChunk,
    ChatCompletionRequest, ChatCompletionResponse, Choice, ContentPart, EmbeddingsRequest,
    EmbeddingsResponse, Message, MessageContent, PanelExtras, Tool, ToolCall, ToolChoice,
};
pub use model_catalog::{model_catalog, ModelCatalog};
pub use pricing::{CacheWriteTier, ModelInfo, ModelPricing};
pub use provider::Provider;
pub use url_guard::{
    filter_extra_headers, find_denied_header, validate_provider_url, UrlGuardError,
};
pub use usage::Usage;
