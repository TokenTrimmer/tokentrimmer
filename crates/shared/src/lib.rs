//! Shared types, errors, and traits for the TokenTrimmer workspace.
//!
//! This crate owns cross-crate contracts and small, pure calculations that are
//! part of those contracts. It does not own request orchestration or I/O.
//! See `docs/02-provider-adapter-guide.md` for the Provider trait.

pub mod agent_cost;
pub mod batch_advisor;
pub mod capability_check;
pub mod content_kind;
pub mod context;
pub mod dns_guard;
pub mod error;
pub mod gateway_capabilities;
pub mod messages;
pub mod model_aliases;
pub mod model_catalog;
pub mod pricing;
pub mod provider;
pub mod providers;
pub mod request_delta;
pub mod request_delta_backfill;
pub mod request_preflight;
pub mod url_guard;
pub mod usage;

pub use agent_cost::{
    AgentCostBasis, AgentCostComponent, AgentCostPurpose, AgentCostValidationError,
    AgentRunCostEvidence, ApiMeteredEvidence, ExpectedAgentCostBasis, SubscriptionQuotaEvidence,
    SubscriptionQuotaUnit, UnmeasuredCostReason, UnmeasuredCostReasonCode,
    AGENT_COST_COMPONENTS_MAX, AGENT_COST_REASONS_MAX, AGENT_COST_SCHEMA_ID,
    AGENT_COST_SCHEMA_VERSION,
};
pub use batch_advisor::{
    project_batch_savings, project_batch_savings_with_tags, BatchFinding, RequestAggregate,
    DEFAULT_BATCH_ELIGIBLE_TAGS,
};
pub use capability_check::{message_text_for_estimation, RequiredCapabilities};
pub use content_kind::{classify as classify_content, ContentKind};
pub use context::{CallerTier, RequestContext};
pub use dns_guard::{with_guarded_dns, GuardedResolveError, GuardedResolver};
pub use error::ProviderError;
pub use gateway_capabilities::{
    AccessEvidence, CapabilityReason, EnabledEvidence, FusionCapability, FusionLimits,
    GatewayCapabilitiesDocument, GatewayFeatures, NumericLimit, SchemaVersionEvidence,
    SchemaVersions, TierEvidence, UnknownEvidence, CAPABILITIES_SCHEMA_VERSION, CAPABILITIES_SCOPE,
    CAPABILITIES_SNAPSHOT_SCOPE,
};
pub use messages::{
    parse_cache_control, parse_panel_extras, CacheControlConfig, CacheMode, CachePrunePolicy,
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, Choice, ContentPart,
    EmbeddingsRequest, EmbeddingsResponse, Message, MessageContent, PanelExtras, Tool, ToolCall,
    ToolChoice,
};
pub use model_catalog::{
    model_catalog, ModelCatalog, ModelCatalogLimitations, ModelEntry, ModelTokenTrimmerMeta,
    ModelsDocumentMeta, ModelsResponse, MODELS_FLEET_CONSISTENCY, MODELS_PROVIDER_CREDENTIALS,
    MODELS_PROVIDER_HEALTH, MODELS_REQUEST_ACCEPTANCE, MODELS_SCHEMA_VERSION,
    MODELS_SNAPSHOT_SCOPE, MODELS_SOURCE,
};
pub use pricing::{CacheWriteTier, Capability, ModelInfo, ModelPricing};
pub use provider::Provider;
pub use request_delta::{
    classify_request_delta_evidence_v1, estimate_request_delta_v1, RequestDeltaEstimate,
    RequestDeltaEvidenceState, RequestDeltaInput, RequestDeltaReceiptError,
    RequestDeltaReceiptFields, REQUEST_DELTA_ESTIMATE_V1,
};
pub use request_delta_backfill::{
    backfill_request_delta_evidence_v1, row_evidence_v1, BackfillCoverage, BackfillRun,
    BackfilledRow, PricingProvenance, RetainedRequestRow, REQUEST_DELTA_BACKFILL_V1,
};
pub use request_preflight::{
    PreflightAction, PreflightCostEvidence, PreflightCredentialEvidence, PreflightLimitEvidence,
    PreflightModelSupportEvidence, PreflightProviderResolution, RequestPreflightBatchRequest,
    RequestPreflightBatchResponse, RequestPreflightRequest, RequestPreflightResponse,
    REQUEST_PREFLIGHT_BATCH_MAX_REQUESTS, REQUEST_PREFLIGHT_BATCH_SCHEMA_VERSION,
    REQUEST_PREFLIGHT_BATCH_SCOPE, REQUEST_PREFLIGHT_SCHEMA_VERSION, REQUEST_PREFLIGHT_SCOPE,
    REQUEST_PREFLIGHT_TOKEN_VALUE_MAX,
};
pub use url_guard::{
    filter_extra_headers, filter_outbound_headers, find_denied_header, find_outbound_denied_header,
    validate_provider_url, UrlGuardError,
};
pub use usage::Usage;
