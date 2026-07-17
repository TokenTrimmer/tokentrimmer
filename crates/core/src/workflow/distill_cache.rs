//! D6 Slice 3 — the per-org content-hash distillation reuse cache.
//!
//! The workflow `Document` node ([`crate::workflow::types::NodeKind::Document`])
//! distills a document's text layer via the `document_lane` seam on every run.
//! A cache keyed by `blake3(content_bytes)` (+ an optional caller-supplied key)
//! makes the second+ run of the SAME document a free lookup (no sidecar call).
//! Content-addressed: the same bytes hit across runs.
//!
//! **Per-org isolation is the store impl's responsibility** — a concrete
//! [`DistillCacheStore`] is constructed per-run WITH the org_id (the cloud's
//! `FlowDocDistillCache { org_id, pool }`), so the [`DistillCacheKey`] carries
//! only `content_hash` + `caller_key` + the store's `get`/`upsert` scope to its
//! org. One org's distillation never leaks to another (the SQL `WHERE org_id =
//! $1` is the impl's contract).
//!
//! The trait is the seam the engine holds; the cloud provides the concrete impl
//! backed by `flow_doc_distill_cache`. The engine defaults to [`NoCache`] (a ZST
//! no-op) when no store is threaded — the node distills fresh on every run (the
//! v1 posture, `public #313`).
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

/// The cache key — `blake3(content_bytes)` + an optional caller-supplied key.
/// Constructed by the engine's Document node from the source bytes + the node's
/// optional `cache_key` template. `caller_key` lets a flow pin a stable logical
/// key (e.g. `"{{trigger.input_id}}"`) for reuse control independent of the
/// content hash. Per-org scoping is the store impl's responsibility.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DistillCacheKey {
    pub content_hash: String,
    pub caller_key: Option<String>,
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
/// `flow_doc_distill_cache` table (cloud migration `0044`). Constructed per-run
/// with the org_id + the gateway's Postgres pool; the `get`/`upsert` scope to
/// that org (`WHERE org_id = $1` — one org's distillation never served to
/// another). The cache fails OPEN: any DB error → `None` on get (the node
/// distills fresh) + a no-op on upsert (the next run re-tries + re-stores).
///
/// Rows expire after 30 days (the table has a normal `distilled_at` index for
/// the expiry sweep); a re-distillation after expiry is byte-identical (the
/// extraction is a deterministic function of the source bytes).
pub struct FlowDocDistillCache<'a> {
    pub org_id: Uuid,
    pub pool: &'a sqlx::PgPool,
}

const GET_SQL: &str = r#"
    SELECT distilled_text, pages, engine
      FROM flow_doc_distill_cache
     WHERE org_id = $1
       AND content_hash = $2
       AND COALESCE(caller_key, '') = COALESCE($3, '')
       AND distilled_at >= now() - INTERVAL '30 days'
     LIMIT 1
"#;

const UPSERT_SQL: &str = r#"
    INSERT INTO flow_doc_distill_cache
        (org_id, content_hash, caller_key, distilled_text, pages, engine)
    VALUES ($1, $2, $3, $4, $5, $6)
    ON CONFLICT (org_id, content_hash, COALESCE(caller_key, ''))
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
        let key = DistillCacheKey {
            content_hash: "abc".into(),
            caller_key: None,
        };
        assert!(NoCache.get(&key).await.is_none());
    }

    #[tokio::test]
    async fn no_cache_upsert_is_a_noop() {
        let key = DistillCacheKey {
            content_hash: "abc".into(),
            caller_key: None,
        };
        let doc = CachedDistill {
            text: "hi".into(),
            pages: 1,
            engine: "pdf-extract".into(),
        };
        NoCache.upsert(&key, &doc).await; // must not panic / error.
        assert!(NoCache.get(&key).await.is_none());
    }
}
