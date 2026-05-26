//! RequestContext carries authenticated identity, trace IDs, and credentials
//! through the request lifecycle. Adapters are stateless — every call gets a
//! fresh context.

use std::time::Duration;

use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct RequestContext {
    pub trace_id: Uuid,
    pub org_id: Uuid,
    pub api_key_id: Uuid,
    pub credentials: ProviderCredentials,
    /// Free-form cost-attribution tag from `X-TokenTrimmer-Tag` header.
    pub tag: Option<String>,
    /// Deadline for the entire request. Adapters should respect this.
    pub deadline: Option<Duration>,
}

#[derive(Debug, Clone)]
pub struct ProviderCredentials {
    pub api_key: SecretString,
    /// Self-hosted endpoint override (used for Ollama, vLLM, LM Studio, OpenRouter, etc.).
    pub base_url: Option<String>,
    pub extra_headers: Vec<(String, String)>,
}

/// String wrapper whose `Debug` impl never prints the secret.
#[derive(Clone)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretString(****)")
    }
}
