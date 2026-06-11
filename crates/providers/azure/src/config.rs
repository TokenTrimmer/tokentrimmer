//! Azure OpenAI endpoint configuration — resource + deployment resolution and
//! URL construction.
//!
//! Azure OpenAI does **not** use a single shared base URL like the public
//! OpenAI API. Each call targets a *deployment* on a *resource*:
//!
//! ```text
//! https://{resource}.openai.azure.com/openai/deployments/{deployment}/chat/completions?api-version={ver}
//! ```
//!
//! Both the resource and the deployment name come from the caller's
//! [`tt_shared::context::ProviderCredentials`] — specifically from
//! `credentials.base_url`, which an Azure customer sets to their resource
//! endpoint (`https://my-resource.openai.azure.com`) optionally with a
//! deployment path. Because the shared credential struct has no dedicated
//! `deployment` field (and we deliberately avoid rippling it across the whole
//! workspace), the deployment is taken from the request `model` id: with Azure,
//! the "model" the caller asks for is the deployment name.

use tt_shared::ProviderError;

/// API version pinned for the Azure OpenAI data-plane. Azure requires an
/// explicit `api-version` query param on every call; a stable GA version is
/// used so behavior does not drift under us.
///
/// **To update:** bump this to a newer GA `api-version` from
/// learn.microsoft.com/azure/ai-services/openai/reference once validated. Do
/// not point at a `-preview` version in production.
pub const AZURE_API_VERSION: &str = "2024-10-21";

/// The host suffix every Azure OpenAI resource endpoint carries. Used to
/// recognise (and lightly validate) a resource base URL.
const AZURE_HOST_SUFFIX: &str = ".openai.azure.com";

/// Resolved Azure endpoint coordinates for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AzureEndpoint {
    /// Resource base origin, e.g. `https://my-resource.openai.azure.com`
    /// (no trailing slash, no `/openai/...` path).
    pub resource_base: String,
    /// Deployment name (the customer's deployed model), e.g. `gpt-4o-prod`.
    pub deployment: String,
}

impl AzureEndpoint {
    /// Resolve the endpoint from the credential `base_url` and the request
    /// `model` (which, for Azure, names the deployment).
    ///
    /// Validation (all errors are [`ProviderError::InvalidRequest`], surfaced at
    /// request time before any network call):
    /// - `base_url` must be present, non-empty, and host an `*.openai.azure.com`
    ///   origin (a missing or non-Azure URL is rejected rather than silently
    ///   hitting the public OpenAI endpoint).
    /// - `deployment` (the model id) must be non-empty.
    ///
    /// Any `/openai/...` path the customer appended to their endpoint is
    /// stripped so the canonical deployment path is constructed cleanly.
    pub fn resolve(base_url: Option<&str>, model: &str) -> Result<Self, ProviderError> {
        Self::resolve_inner(base_url, model, true)
    }

    /// Like [`resolve`](Self::resolve) but skips the `*.openai.azure.com` host
    /// check, so tests can target a local mock server (e.g. `127.0.0.1:PORT`).
    /// The full SSRF guard (`validate_provider_url`) still applies in the
    /// adapter. **Do not use in production.**
    #[doc(hidden)]
    pub fn resolve_allow_any_host(
        base_url: Option<&str>,
        model: &str,
    ) -> Result<Self, ProviderError> {
        Self::resolve_inner(base_url, model, false)
    }

    fn resolve_inner(
        base_url: Option<&str>,
        model: &str,
        check_azure_host: bool,
    ) -> Result<Self, ProviderError> {
        let deployment = model.trim();
        if deployment.is_empty() {
            return Err(ProviderError::InvalidRequest(
                "azure: deployment name (request model) must not be empty".to_string(),
            ));
        }

        let raw = base_url.map(str::trim).unwrap_or_default();
        if raw.is_empty() {
            return Err(ProviderError::InvalidRequest(
                "azure: credentials.base_url must be set to the resource endpoint \
                 (https://<resource>.openai.azure.com)"
                    .to_string(),
            ));
        }

        // Strip a trailing slash and any `/openai...` path the customer included,
        // leaving the bare `scheme://host[:port]` origin.
        let trimmed = raw.trim_end_matches('/');
        let origin = match trimmed.find("/openai") {
            Some(idx) => &trimmed[..idx],
            None => trimmed,
        };
        let origin = origin.trim_end_matches('/');

        // Light host validation: must look like an Azure OpenAI resource origin.
        // (Full SSRF validation still runs later via `validate_provider_url`.)
        if check_azure_host {
            let host_ok = origin
                .strip_prefix("https://")
                .or_else(|| origin.strip_prefix("http://"))
                .map(|h| {
                    let host = h.split(['/', ':']).next().unwrap_or(h);
                    host.ends_with(AZURE_HOST_SUFFIX) && host.len() > AZURE_HOST_SUFFIX.len()
                })
                .unwrap_or(false);
            if !host_ok {
                return Err(ProviderError::InvalidRequest(format!(
                    "azure: base_url must be an https://<resource>{AZURE_HOST_SUFFIX} endpoint, got: {raw}"
                )));
            }
        }

        Ok(Self {
            resource_base: origin.to_string(),
            deployment: deployment.to_string(),
        })
    }

    /// Full chat-completions URL with the `api-version` query param.
    pub fn chat_completions_url(&self) -> String {
        format!(
            "{}/openai/deployments/{}/chat/completions?api-version={}",
            self.resource_base, self.deployment, AZURE_API_VERSION
        )
    }

    /// Full embeddings URL with the `api-version` query param.
    pub fn embeddings_url(&self) -> String {
        format!(
            "{}/openai/deployments/{}/embeddings?api-version={}",
            self.resource_base, self.deployment, AZURE_API_VERSION
        )
    }

    /// The origin used for SSRF validation (scheme://host[:port]).
    pub fn origin(&self) -> &str {
        &self.resource_base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_builds_deployment_url_with_api_version() {
        let ep =
            AzureEndpoint::resolve(Some("https://my-resource.openai.azure.com"), "gpt-4o-prod")
                .expect("valid");
        assert_eq!(ep.resource_base, "https://my-resource.openai.azure.com");
        assert_eq!(ep.deployment, "gpt-4o-prod");
        assert_eq!(
            ep.chat_completions_url(),
            format!(
                "https://my-resource.openai.azure.com/openai/deployments/gpt-4o-prod/chat/completions?api-version={AZURE_API_VERSION}"
            )
        );
        assert_eq!(
            ep.embeddings_url(),
            format!(
                "https://my-resource.openai.azure.com/openai/deployments/gpt-4o-prod/embeddings?api-version={AZURE_API_VERSION}"
            )
        );
    }

    #[test]
    fn resolve_strips_trailing_slash_and_openai_path() {
        let ep = AzureEndpoint::resolve(Some("https://r.openai.azure.com/openai/"), "dep")
            .expect("valid");
        assert_eq!(ep.resource_base, "https://r.openai.azure.com");
        assert!(ep.chat_completions_url().starts_with(
            "https://r.openai.azure.com/openai/deployments/dep/chat/completions?api-version="
        ));
    }

    #[test]
    fn resolve_rejects_empty_deployment() {
        let err = AzureEndpoint::resolve(Some("https://r.openai.azure.com"), "  ").unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn resolve_rejects_missing_base_url() {
        let err = AzureEndpoint::resolve(None, "dep").unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
        let err2 = AzureEndpoint::resolve(Some("   "), "dep").unwrap_err();
        assert!(matches!(err2, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn resolve_rejects_non_azure_host() {
        // A public OpenAI URL must be rejected, not silently accepted.
        let err = AzureEndpoint::resolve(Some("https://api.openai.com/v1"), "gpt-4o").unwrap_err();
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
        // A bare suffix with no resource name is rejected.
        let err2 = AzureEndpoint::resolve(Some("https://.openai.azure.com"), "dep").unwrap_err();
        assert!(matches!(err2, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn api_version_is_a_stable_ga_version() {
        // Guardrail: production must not pin a preview api-version.
        assert!(
            !AZURE_API_VERSION.contains("preview"),
            "AZURE_API_VERSION must be a GA (non-preview) version"
        );
    }
}
