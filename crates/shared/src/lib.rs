//! Shared types, errors, and traits for the TokenTrimmer workspace.
//!
//! No business logic lives here — only the contracts that other crates implement.
//! See `docs/02-provider-adapter-guide.md` for the Provider trait.

pub mod context;
pub mod error;
pub mod messages;
pub mod pricing;
pub mod provider;
pub mod usage;

pub use context::RequestContext;
pub use error::ProviderError;
pub use messages::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, Choice, ContentPart,
    EmbeddingsRequest, EmbeddingsResponse, Message, MessageContent, Tool, ToolCall, ToolChoice,
};
pub use pricing::{ModelInfo, ModelPricing};
pub use provider::Provider;
pub use usage::Usage;
