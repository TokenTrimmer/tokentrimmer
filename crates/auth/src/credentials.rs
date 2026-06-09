//! Per-org provider credential lookup.
//!
//! The gateway needs to know which upstream API key to send with each request:
//! a customer presents a TokenTrimmer key (`tt_live_*`); we look up the org
//! that key belongs to; the org has one credential row per provider
//! (`openai`, `anthropic`, …) carrying the API key plus optional `base_url`
//! and `extra_headers`. The middleware fetches that row and stamps it into
//! [`tt_shared::context::RequestContext::credentials`] so the provider
//! adapter authenticates upstream as the customer.
//!
//! This module ships two production-ready stores:
//!
//! * [`InMemoryProviderCredentialStore`] — process-local map, useful for tests
//!   and demo configs.
//! * [`EnvProviderCredentialStore`] — reads `OPENAI_API_KEY`,
//!   `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, etc. from process environment.
//!   Returns the same credential to **every** org, which is what you want
//!   for single-tenant dogfooding (one team, one set of upstream keys)
//!   until the cloud-repo dashboard lands a credential-entry UI.
//!
//! The DB-backed store (XChaCha20-Poly1305 encrypted, `TT_MASTER_KEY` derived
//! per-row) is `w7-auth-credentials-postgres`, blocked on the cloud repo.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use thiserror::Error;
use tt_shared::context::{ProviderCredentials, SecretString};
use uuid::Uuid;

/// Errors a [`ProviderCredentialStore`] may return.
#[derive(Debug, Error)]
pub enum CredentialError {
    /// Backing store failed (DB error, lock poisoned, etc.).
    #[error("credential store: {0}")]
    Store(String),
}

/// Lookup + persistence of upstream provider credentials.
///
/// Implementors must be `Send + Sync` so the gateway can share one instance
/// across all request-handler tasks.
///
/// `put` writes are admin/UI-driven: an operator (or the dashboard's
/// credential-entry page) calls it through `POST /v1/admin/credentials`.
/// Read-only stores (the env-var fallback) refuse writes with
/// [`CredentialError::Store`].
#[async_trait]
pub trait ProviderCredentialStore: Send + Sync {
    /// Return the credential, or `None` if the org has not configured one for
    /// this provider. `provider_id` is the lowercase string matching
    /// [`tt_shared::Provider::id`], e.g. `"openai"`, `"anthropic"`.
    async fn get(
        &self,
        org_id: Uuid,
        provider_id: &str,
    ) -> Result<Option<ProviderCredentials>, CredentialError>;

    /// Insert or upsert a credential for `(org_id, provider_id)`. Default impl
    /// rejects with [`CredentialError::Store`] — read-only stores like
    /// [`EnvProviderCredentialStore`] inherit that default. Stores that
    /// support writes (in-memory, Postgres) override it.
    async fn put(
        &self,
        _org_id: Uuid,
        _provider_id: &str,
        _label: &str,
        _credentials: ProviderCredentials,
    ) -> Result<(), CredentialError> {
        Err(CredentialError::Store("store is read-only".into()))
    }

    /// Delete the credential for `(org_id, provider_id)`. Returns `true` if a
    /// row existed and was removed, `false` if there was nothing to delete (so
    /// the caller can skip writing an audit row). Default impl rejects with
    /// [`CredentialError::Store`] — read-only stores inherit it; writable stores
    /// (in-memory, Postgres) override. (rv-credentials-delete)
    async fn delete(&self, _org_id: Uuid, _provider_id: &str) -> Result<bool, CredentialError> {
        Err(CredentialError::Store("store is read-only".into()))
    }

    /// Number of distinct providers `org_id` has stored credentials for.
    /// Default `0` — read-only / env stores don't track per-org counts;
    /// InMemory and Postgres override. Used to enforce per-tier credential
    /// caps at write time.
    async fn count_for_org(&self, _org_id: Uuid) -> Result<u32, CredentialError> {
        Ok(0)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// In-memory store
// ─────────────────────────────────────────────────────────────────────────────

/// Process-local credential map. Cheap to clone — the inner map is
/// reference-counted.
///
/// Suitable for tests and demo configs. Production deployments either compose
/// with [`EnvProviderCredentialStore`] or wait for the DB-backed store.
#[derive(Clone, Default)]
pub struct InMemoryProviderCredentialStore {
    inner: Arc<Mutex<HashMap<(Uuid, String), ProviderCredentials>>>,
}

impl InMemoryProviderCredentialStore {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace the credential for `(org_id, provider_id)`.
    pub fn insert(
        &self,
        org_id: Uuid,
        provider_id: impl Into<String>,
        credentials: ProviderCredentials,
    ) {
        // unwrap is fine: poison can only happen if a previous holder panicked
        // mid-write, which only happens in tests where we'd want to surface.
        let mut g = self.inner.lock().expect("credential store mutex poisoned");
        g.insert((org_id, provider_id.into()), credentials);
    }
}

#[async_trait]
impl ProviderCredentialStore for InMemoryProviderCredentialStore {
    async fn get(
        &self,
        org_id: Uuid,
        provider_id: &str,
    ) -> Result<Option<ProviderCredentials>, CredentialError> {
        let g = self
            .inner
            .lock()
            .map_err(|e| CredentialError::Store(e.to_string()))?;
        Ok(g.get(&(org_id, provider_id.to_string())).cloned())
    }

    async fn put(
        &self,
        org_id: Uuid,
        provider_id: &str,
        _label: &str,
        credentials: ProviderCredentials,
    ) -> Result<(), CredentialError> {
        let mut g = self
            .inner
            .lock()
            .map_err(|e| CredentialError::Store(e.to_string()))?;
        g.insert((org_id, provider_id.to_string()), credentials);
        Ok(())
    }

    async fn delete(&self, org_id: Uuid, provider_id: &str) -> Result<bool, CredentialError> {
        let mut g = self
            .inner
            .lock()
            .map_err(|e| CredentialError::Store(e.to_string()))?;
        Ok(g.remove(&(org_id, provider_id.to_string())).is_some())
    }

    async fn count_for_org(&self, org_id: Uuid) -> Result<u32, CredentialError> {
        let g = self
            .inner
            .lock()
            .map_err(|e| CredentialError::Store(e.to_string()))?;
        Ok(g.keys().filter(|(o, _)| *o == org_id).count() as u32)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Env-var store
// ─────────────────────────────────────────────────────────────────────────────

/// Reads upstream API keys from process environment variables.
///
/// One credential set, shared across all orgs. This is the single-tenant
/// dogfooding fallback: you set `OPENAI_API_KEY` etc. in `fly secrets`
/// and the gateway forwards them on every request regardless of which
/// TokenTrimmer key was presented.
///
/// Recognized env vars (each one optional — the store returns `None` for
/// providers whose env var is unset):
///
/// | provider_id      | env var               |
/// | ---------------- | --------------------- |
/// | `openai`         | `OPENAI_API_KEY`      |
/// | `anthropic`      | `ANTHROPIC_API_KEY`   |
/// | `gemini`         | `GEMINI_API_KEY`      |
/// | `mistral`        | `MISTRAL_API_KEY`     |
/// | `groq`           | `GROQ_API_KEY`        |
/// | `together`       | `TOGETHER_API_KEY`    |
/// | `openrouter`     | `OPENROUTER_API_KEY`  |
///
/// Unknown providers always return `None`.
#[derive(Clone, Default)]
pub struct EnvProviderCredentialStore;

impl EnvProviderCredentialStore {
    /// New store. Env is read on every `get` call, so updates to process env
    /// (rare) are picked up without a restart.
    pub fn new() -> Self {
        Self
    }

    /// Trim surrounding whitespace/newlines from an env-sourced key and treat a
    /// blank value as absent. A trailing newline (common from
    /// `echo secret | fly secrets set`) would otherwise be forwarded verbatim and
    /// produce a confusing upstream 401.
    fn normalize_key(raw: &str) -> Option<String> {
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    fn env_var_for(provider_id: &str) -> Option<&'static str> {
        Some(match provider_id {
            "openai" => "OPENAI_API_KEY",
            "anthropic" => "ANTHROPIC_API_KEY",
            "gemini" => "GEMINI_API_KEY",
            "mistral" => "MISTRAL_API_KEY",
            "groq" => "GROQ_API_KEY",
            "together" => "TOGETHER_API_KEY",
            "openrouter" => "OPENROUTER_API_KEY",
            _ => return None,
        })
    }
}

#[async_trait]
impl ProviderCredentialStore for EnvProviderCredentialStore {
    async fn get(
        &self,
        _org_id: Uuid,
        provider_id: &str,
    ) -> Result<Option<ProviderCredentials>, CredentialError> {
        let Some(var) = Self::env_var_for(provider_id) else {
            return Ok(None);
        };
        match std::env::var(var) {
            Ok(v) => Ok(Self::normalize_key(&v).map(|key| ProviderCredentials {
                api_key: SecretString::new(key),
                base_url: None,
                extra_headers: Vec::new(),
            })),
            Err(_) => Ok(None),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Composite store
// ─────────────────────────────────────────────────────────────────────────────

/// Try a primary store first; fall back to a secondary if the primary returns
/// `None`. Useful for "DB if the org has set up creds, otherwise the
/// process env" without bolting policy into either concrete store.
pub struct ChainedProviderCredentialStore<A, B> {
    pub primary: A,
    pub fallback: B,
    /// Set once the fallback has served a credential, so the multi-tenant
    /// misconfiguration warning fires at most once per process, not per request.
    env_fallback_warned: std::sync::atomic::AtomicBool,
}

impl<A, B> ChainedProviderCredentialStore<A, B> {
    /// Try `primary` first; fall back to `secondary` on `None`.
    ///
    /// SECURITY: the fallback is intended for single-tenant / self-host
    /// dogfooding (e.g. [`EnvProviderCredentialStore`], which ignores `org_id`
    /// and returns one shared process-env key for ALL orgs). In a HOSTED /
    /// multi-tenant deployment do NOT chain a shared env store — use the
    /// Postgres store alone and fail closed (the gateway then falls back to the
    /// customer's own raw Bearer) so one org can't be billed against / exposed
    /// to another org's shared key. When the fallback serves a credential this
    /// store logs a one-time warning to surface such a misconfiguration. (§5.10)
    pub fn new(primary: A, fallback: B) -> Self {
        Self {
            primary,
            fallback,
            env_fallback_warned: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl<A, B> ProviderCredentialStore for ChainedProviderCredentialStore<A, B>
where
    A: ProviderCredentialStore,
    B: ProviderCredentialStore,
{
    async fn get(
        &self,
        org_id: Uuid,
        provider_id: &str,
    ) -> Result<Option<ProviderCredentials>, CredentialError> {
        if let Some(c) = self.primary.get(org_id, provider_id).await? {
            return Ok(Some(c));
        }
        let fallback = self.fallback.get(org_id, provider_id).await?;
        if fallback.is_some()
            && self
                .env_fallback_warned
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                )
                .is_ok()
        {
            tracing::warn!(
                %org_id,
                provider_id,
                "provider credential served from the SHARED fallback store (e.g. process env) — \
                 OK for single-tenant/self-host, but a misconfiguration in a multi-tenant \
                 deployment (an org billed against / exposed to a shared key). Use Postgres-only \
                 + fail-closed in hosted mode. (warns once per process)"
            );
        }
        Ok(fallback)
    }

    async fn count_for_org(&self, org_id: Uuid) -> Result<u32, CredentialError> {
        // The writable primary store is the source of truth for the count;
        // the fallback (env) holds no per-org rows.
        self.primary.count_for_org(org_id).await
    }

    async fn delete(&self, org_id: Uuid, provider_id: &str) -> Result<bool, CredentialError> {
        // Deletes target the writable primary store; the env fallback is
        // read-only and holds no per-org rows to remove.
        self.primary.delete(org_id, provider_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(key: &str) -> ProviderCredentials {
        ProviderCredentials {
            api_key: SecretString::new(key),
            base_url: None,
            extra_headers: Vec::new(),
        }
    }

    #[tokio::test]
    async fn in_memory_returns_inserted_credential() {
        let store = InMemoryProviderCredentialStore::new();
        let org = Uuid::now_v7();
        store.insert(org, "openai", creds("sk-abc"));
        let got = store.get(org, "openai").await.unwrap().unwrap();
        assert_eq!(got.api_key.expose(), "sk-abc");
    }

    #[tokio::test]
    async fn in_memory_delete_removes_and_reports_existence() {
        let store = InMemoryProviderCredentialStore::new();
        let org = Uuid::now_v7();
        store.insert(org, "openai", creds("sk-abc"));
        // Deleting a present credential reports true + removes it.
        assert!(store.delete(org, "openai").await.unwrap());
        assert!(store.get(org, "openai").await.unwrap().is_none());
        // Deleting again (or an unknown provider) reports false — caller skips audit.
        assert!(!store.delete(org, "openai").await.unwrap());
        // Another org's credential is untouched by a same-provider delete.
        let other = Uuid::now_v7();
        store.insert(other, "openai", creds("sk-other"));
        assert!(!store.delete(org, "openai").await.unwrap());
        assert!(store.get(other, "openai").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn env_store_delete_is_read_only() {
        let store = EnvProviderCredentialStore::new();
        assert!(store.delete(Uuid::now_v7(), "openai").await.is_err());
    }

    #[tokio::test]
    async fn in_memory_count_for_org_counts_distinct_providers() {
        let store = InMemoryProviderCredentialStore::new();
        let org = Uuid::now_v7();
        assert_eq!(store.count_for_org(org).await.unwrap(), 0);
        store.insert(org, "openai", creds("a"));
        store.insert(org, "anthropic", creds("b"));
        store.insert(org, "openai", creds("a2")); // upsert — still one provider
        store.insert(Uuid::now_v7(), "openai", creds("c")); // other org
        assert_eq!(store.count_for_org(org).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn in_memory_returns_none_for_unknown_org_or_provider() {
        let store = InMemoryProviderCredentialStore::new();
        let org = Uuid::now_v7();
        store.insert(org, "openai", creds("sk-abc"));
        assert!(store.get(org, "anthropic").await.unwrap().is_none());
        assert!(store.get(Uuid::now_v7(), "openai").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn env_store_reads_recognized_var() {
        // Use a UNIQUE var to avoid clobbering real env in concurrent tests.
        // We can't actually rename — the store reads OPENAI_API_KEY specifically.
        // Test runs are serialized via #[cfg(test)] in this file alone.
        std::env::set_var("OPENAI_API_KEY", "sk-env-test");
        let store = EnvProviderCredentialStore::new();
        let got = store.get(Uuid::nil(), "openai").await.unwrap().unwrap();
        assert_eq!(got.api_key.expose(), "sk-env-test");
        std::env::remove_var("OPENAI_API_KEY");
    }

    #[tokio::test]
    async fn env_store_returns_none_for_unknown_provider() {
        let store = EnvProviderCredentialStore::new();
        assert!(store
            .get(Uuid::nil(), "no-such-provider")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn env_store_returns_none_when_var_unset_or_empty() {
        std::env::remove_var("MISTRAL_API_KEY");
        let store = EnvProviderCredentialStore::new();
        assert!(store.get(Uuid::nil(), "mistral").await.unwrap().is_none());
        std::env::set_var("MISTRAL_API_KEY", "");
        assert!(store.get(Uuid::nil(), "mistral").await.unwrap().is_none());
        std::env::remove_var("MISTRAL_API_KEY");
    }

    #[tokio::test]
    async fn env_store_trims_surrounding_whitespace_and_newlines() {
        // `echo secret | fly secrets set` appends a trailing newline; forwarded
        // verbatim it produces a confusing upstream 401. The store must trim it.
        std::env::set_var("GROQ_API_KEY", "  gsk-padded\n");
        let store = EnvProviderCredentialStore::new();
        let got = store.get(Uuid::nil(), "groq").await.unwrap().unwrap();
        assert_eq!(got.api_key.expose(), "gsk-padded");
        // A whitespace-only value is treated as absent, not a blank key.
        std::env::set_var("GROQ_API_KEY", "  \n\t ");
        assert!(store.get(Uuid::nil(), "groq").await.unwrap().is_none());
        std::env::remove_var("GROQ_API_KEY");
    }

    #[tokio::test]
    async fn chained_prefers_primary_then_fallback() {
        let primary = InMemoryProviderCredentialStore::new();
        let fallback = InMemoryProviderCredentialStore::new();
        let org = Uuid::now_v7();
        primary.insert(org, "openai", creds("from-primary"));
        fallback.insert(org, "openai", creds("from-fallback"));
        fallback.insert(org, "anthropic", creds("only-fallback"));
        let chain = ChainedProviderCredentialStore::new(primary, fallback);

        // Primary hit wins.
        let g = chain.get(org, "openai").await.unwrap().unwrap();
        assert_eq!(g.api_key.expose(), "from-primary");

        // Falls through to fallback for keys only there.
        let g = chain.get(org, "anthropic").await.unwrap().unwrap();
        assert_eq!(g.api_key.expose(), "only-fallback");

        // Neither has it → None.
        assert!(chain.get(org, "gemini").await.unwrap().is_none());
    }
}
