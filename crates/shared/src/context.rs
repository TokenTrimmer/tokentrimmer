//! RequestContext carries authenticated identity, trace IDs, and credentials
//! through the request lifecycle. Adapters are stateless — every call gets a
//! fresh context.

use std::time::Duration;

use uuid::Uuid;

/// Subscription tier for the calling organisation, as surfaced by the cloud
/// tier-resolution layer.
///
/// The tier drives cache TTL selection per spec §8.4 (24h / 7d / 30d bands).
/// The tier is carried as `Option<CallerTier>` on `tt_auth::ApiKeyContext`
/// (NOT on [`crate::context::RequestContext`], which has no `tier` field): when
/// `None`, the gateway falls back to the conservative 24h default. The cloud
/// will inject the real tier once `rv-tier-limits-enforcement` is wired; until
/// then all requests run as if Free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallerTier {
    /// Free tier — 24h cache TTL (spec §8.4).
    Free,
    /// Pro tier — 7d cache TTL (spec §8.4).
    Pro,
    /// Team tier — 7d cache TTL (spec §8.4, same band as Pro).
    Team,
    /// Scale tier — 30d cache TTL (spec §8.4).
    Scale,
}

impl CallerTier {
    /// Cache TTL in seconds for this tier, per spec §8.4.
    ///
    /// | Tier        | TTL    |
    /// | ----------- | ------ |
    /// | Free        | 24h    |
    /// | Pro / Team  | 7d     |
    /// | Scale       | 30d    |
    #[must_use]
    pub fn ttl_secs(self) -> u64 {
        match self {
            CallerTier::Free => 24 * 60 * 60,
            CallerTier::Pro | CallerTier::Team => 7 * 24 * 60 * 60,
            CallerTier::Scale => 30 * 24 * 60 * 60,
        }
    }
}

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
    /// Agent run ID — set when this request belongs to an agent run loop
    /// (W0b durable-run-grain). `None` for standalone (non-agent) requests.
    pub run_id: Option<Uuid>,
    /// Agent node/step ID within the run — set alongside `run_id`.
    /// `None` for standalone requests.
    pub node_id: Option<Uuid>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_context_has_run_id_node_id_fields() {
        // construction itself is the gate; this asserts the fields exist + accept Option<Uuid>
        let ctx = RequestContext {
            trace_id: Uuid::nil(),
            org_id: Uuid::nil(),
            api_key_id: Uuid::nil(),
            credentials: ProviderCredentials {
                api_key: SecretString::new("test"),
                base_url: None,
                extra_headers: Vec::new(),
            },
            tag: None,
            deadline: None,
            run_id: None,
            node_id: None,
        };
        assert_eq!(ctx.run_id, None);
        assert_eq!(ctx.node_id, None);
    }
}
