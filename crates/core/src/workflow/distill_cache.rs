//! Per-organization document-distillation reuse cache.
//!
//! A cached extraction is reusable only when every semantic input matches:
//! decoded source bytes, normalized media type, caller key, extractor revision,
//! and TokenTrimmer cache-policy revision. This prevents identical bytes from
//! being interpreted under a different MIME type or stale extraction behavior
//! from surviving a sidecar/policy upgrade.
//!
//! Per-org isolation remains the store implementation's responsibility. The
//! cloud-backed [`FlowDocDistillCache`] binds every lookup/write to its
//! configured organization; [`NoCache`] remains the fail-open default when no
//! store is configured.
//!
//! # Fail-open
//! A cache error (unreachable DB, decode failure) → `None` on get (the node
//! distills fresh) + a no-op on upsert (the next run re-tries). The cache is a
//! pure optimization — it must never be able to break a workflow run.

use uuid::Uuid;

/// A cached distillation result (mirrors `document_lane::seam::DistillOutcome`'s
/// success shape + the `flow_doc_distill_cache` row). The engine stores the
/// distilled text + its provenance; a hit skips the sidecar call entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedDistill {
    /// The distilled text (all pages joined).
    pub text: String,
    /// Number of pages the original extraction saw.
    pub pages: u32,
    /// The engine that produced the cached text (`"pdf-extract"`, …).
    pub engine: String,
}

/// Revision for TokenTrimmer-side extraction/cache semantics. Bump whenever
/// normalization, extraction admission, or cached-output interpretation changes.
pub const DISTILL_CACHE_POLICY_REVISION: &str = "workflow-document-cache-v2";

/// Complete semantic identity of one cached document extraction.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DistillCacheKey {
    /// Blake3 digest of the decoded source bytes.
    pub content_hash: String,
    pub caller_key: Option<String>,
    /// Lowercase MIME essence, without parameters.
    pub media_type: String,
    /// Immutable sidecar build/config revision supplied by the operator.
    pub extractor_revision: String,
    /// TokenTrimmer-side extraction/cache policy revision.
    pub policy_revision: String,
}

impl DistillCacheKey {
    #[must_use]
    pub fn new(
        content_hash: String,
        caller_key: Option<String>,
        media_type: &str,
        extractor_revision: &str,
    ) -> Self {
        Self {
            content_hash,
            caller_key,
            media_type: normalize_media_type(media_type),
            extractor_revision: extractor_revision.to_string(),
            policy_revision: DISTILL_CACHE_POLICY_REVISION.to_string(),
        }
    }
}

#[must_use]
pub fn normalize_media_type(value: &str) -> String {
    let essence = value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if essence.is_empty() {
        "application/octet-stream".to_string()
    } else {
        essence
    }
}

/// The per-org distillation reuse cache. The engine holds `&dyn` of this; the
/// cloud's concrete impl backs `flow_doc_distill_cache` + scopes `get`/`upsert`
/// to the org it was constructed with. [`NoCache`] is the default (no-op) impl.
#[async_trait::async_trait]
pub trait DistillCacheStore: Send + Sync {
    /// Fetch a cached distillation for the key. `None` (fail-open) on a miss or
    /// any error — the node distills fresh.
    async fn get(&self, key: &DistillCacheKey) -> Option<CachedDistill>;

    /// Store a fresh distillation for the key. Fail-open: an error is swallowed
    /// (the next run re-tries the distillation + re-stores).
    async fn upsert(&self, key: &DistillCacheKey, doc: &CachedDistill);
}

/// The default no-op cache (the v1 posture). Every `get` misses; every `upsert`
/// is dropped. Used when no cloud store is threaded.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCache;

#[async_trait::async_trait]
impl DistillCacheStore for NoCache {
    async fn get(&self, _key: &DistillCacheKey) -> Option<CachedDistill> {
        None
    }
    async fn upsert(&self, _key: &DistillCacheKey, _doc: &CachedDistill) {}
}

/// The concrete per-org distillation reuse cache backed by the
/// `flow_doc_distill_cache` table (cloud migrations `0044` and `0118`).
/// Constructed per-run with the org_id + the gateway's Postgres pool; the
/// `get`/`upsert` scope to that org (`WHERE org_id = $1` — one org's
/// distillation is never served to another). The cache fails open: any DB error
/// yields `None` on get (the node distills fresh) and a no-op on upsert.
///
/// Rows expire after 30 days. Re-distillation uses the same explicitly
/// versioned extractor and policy semantics represented by the key.
pub struct FlowDocDistillCache<'a> {
    pub org_id: Uuid,
    pub pool: &'a sqlx::PgPool,
}

const GET_SQL: &str = r#"
    SELECT distilled_text, pages, engine
      FROM public.flow_doc_distill_cache
     WHERE org_id = $1
       AND content_hash = $2
       AND COALESCE(caller_key, '') = COALESCE($3, '')
       AND media_type = $4
       AND extractor_revision = $5
       AND policy_revision = $6
       AND distilled_at >= now() - INTERVAL '30 days'
     LIMIT 1
"#;

const UPSERT_SQL: &str = r#"
    INSERT INTO public.flow_doc_distill_cache
        (org_id, content_hash, caller_key, media_type, extractor_revision,
         policy_revision, distilled_text, pages, engine)
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
    ON CONFLICT (
        org_id, content_hash, COALESCE(caller_key, ''), media_type,
        extractor_revision, policy_revision
    )
    DO UPDATE SET
        distilled_text = EXCLUDED.distilled_text,
        pages          = EXCLUDED.pages,
        engine         = EXCLUDED.engine,
        distilled_at   = now()
"#;

#[async_trait::async_trait]
impl<'a> DistillCacheStore for FlowDocDistillCache<'a> {
    async fn get(&self, key: &DistillCacheKey) -> Option<CachedDistill> {
        // Fail-open: any DB error (no table yet, unreachable, decode) → None.
        sqlx::query_as::<_, FlowDocDistillRow>(GET_SQL)
            .bind(self.org_id)
            .bind(&key.content_hash)
            .bind(&key.caller_key)
            .bind(&key.media_type)
            .bind(&key.extractor_revision)
            .bind(&key.policy_revision)
            .fetch_optional(self.pool)
            .await
            .ok()
            .flatten()
            .map(Into::into)
    }

    async fn upsert(&self, key: &DistillCacheKey, doc: &CachedDistill) {
        // Fail-open: an upsert error is swallowed (the next run re-tries).
        let _ = sqlx::query(UPSERT_SQL)
            .bind(self.org_id)
            .bind(&key.content_hash)
            .bind(&key.caller_key)
            .bind(&key.media_type)
            .bind(&key.extractor_revision)
            .bind(&key.policy_revision)
            .bind(&doc.text)
            .bind(doc.pages as i32)
            .bind(&doc.engine)
            .execute(self.pool)
            .await;
    }
}

#[derive(sqlx::FromRow)]
struct FlowDocDistillRow {
    distilled_text: String,
    pages: i32,
    engine: String,
}

impl From<FlowDocDistillRow> for CachedDistill {
    fn from(row: FlowDocDistillRow) -> Self {
        CachedDistill {
            text: row.distilled_text,
            pages: row.pages.max(0) as u32,
            engine: row.engine,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_cache_get_always_misses() {
        let key = DistillCacheKey::new("abc".into(), None, "application/pdf", "extractor-test");
        assert!(NoCache.get(&key).await.is_none());
    }

    #[tokio::test]
    async fn no_cache_upsert_is_a_noop() {
        let key = DistillCacheKey::new("abc".into(), None, "application/pdf", "extractor-test");
        let doc = CachedDistill {
            text: "hi".into(),
            pages: 1,
            engine: "pdf-extract".into(),
        };
        NoCache.upsert(&key, &doc).await; // must not panic / error.
        assert!(NoCache.get(&key).await.is_none());
    }

    #[test]
    fn media_type_normalization_uses_mime_essence() {
        assert_eq!(
            normalize_media_type(" Application/PDF ; charset=binary "),
            "application/pdf"
        );
        assert_eq!(normalize_media_type("  "), "application/octet-stream");
    }
}
