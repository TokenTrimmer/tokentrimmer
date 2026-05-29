//! L2 semantic cache — trait and implementations.
//!
//! The L2 cache stores compressed chat-completion responses alongside their
//! prompt embeddings and retrieves them by cosine similarity. This lets the
//! gateway serve cached responses for semantically equivalent queries even
//! when the exact request bytes differ.
//!
//! # Architecture
//!
//! ```text
//! Gateway request
//!   │
//!   ├─▶ L1 (Redis, exact SHA-256 key)  → hit: return cached bytes
//!   │
//!   └─▶ L2 (this module, cosine sim)   → hit: return if sim ≥ threshold
//!                                       → miss: call provider, write both L1 + L2
//! ```
//!
//! # Implementations
//!
//! - [`InMemoryL2Cache`] — `Vec` + `Mutex`, naïve O(n) cosine scan. Suitable
//!   for tests and local development only.
//! - [`PostgresL2Cache`] — HNSW pgvector index via `sqlx`. Production path.
//!   Its integration test is gated behind `#[ignore]` and requires a
//!   `DATABASE_URL` env var pointing at a Postgres instance with the `vector`
//!   extension installed.
//!
//! # Similarity threshold
//!
//! The default per-org threshold is **0.92** (ADR-008). Callers may override
//! it on a per-request basis by passing a different `threshold` value to
//! [`L2Cache::lookup`].

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::CacheError;

// ---------------------------------------------------------------------------
// CacheEntry
// ---------------------------------------------------------------------------

/// A single entry in the L2 semantic cache.
///
/// Stored in Postgres (`cache_entries` table) and represented in memory when
/// working with [`InMemoryL2Cache`].
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// Unique row identifier.
    pub id: Uuid,
    /// Organization that owns this entry. Used for tenant isolation.
    pub org_id: Uuid,
    /// Dense embedding vector for the cached prompt. Dimensionality matches
    /// the embedding model (e.g. 1536 for `text-embedding-3-small`).
    pub embedding: Vec<f32>,
    /// Serialized [`tt_shared::ChatCompletionResponse`] bytes (JSON).
    pub response: Vec<u8>,
    /// The **chat** model used to produce the response (e.g. `"gpt-4o"`),
    /// *not* the embedding model.
    pub model: String,
    /// Number of prompt tokens consumed.
    pub input_tokens: u64,
    /// Number of completion tokens produced.
    pub output_tokens: u64,
    /// How many times this entry has been served from cache.
    pub hit_count: u64,
    /// Wall-clock time the entry was created.
    pub created_at: DateTime<Utc>,
    /// Wall-clock time after which the entry must not be served.
    pub expires_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// L2Cache trait
// ---------------------------------------------------------------------------

/// Semantic L2 cache contract.
///
/// Implementations must be `Send + Sync` so they can be shared across async
/// tasks via `Arc<dyn L2Cache>`.
#[async_trait]
pub trait L2Cache: Send + Sync {
    /// Insert a new [`CacheEntry`].
    ///
    /// Implementors should replace or ignore duplicate `id` values; callers
    /// generate a fresh UUID per insert.
    async fn insert(&self, entry: CacheEntry) -> Result<(), CacheError>;

    /// Find the nearest entry to `query_embedding` for `org_id`.
    ///
    /// Returns `Some((entry, similarity))` if the best match has a cosine
    /// similarity ≥ `threshold` **and** its `expires_at` is in the future.
    /// Returns `None` if no qualifying entry is found.
    async fn lookup(
        &self,
        org_id: Uuid,
        query_embedding: &[f32],
        threshold: f32,
    ) -> Result<Option<(CacheEntry, f32)>, CacheError>;

    /// Increment `hit_count` for the entry identified by `id`.
    ///
    /// This is a best-effort, idempotent operation. Implementations may
    /// silently swallow errors to avoid blocking the hot path.
    async fn bump_hit_count(&self, id: Uuid) -> Result<(), CacheError>;
}

// ---------------------------------------------------------------------------
// Cosine similarity
// ---------------------------------------------------------------------------

/// Compute the cosine similarity between two vectors.
///
/// Returns a value in `[-1.0, 1.0]`. Returns `0.0` if either vector has zero
/// magnitude so that zero-length vectors never match.
///
/// OpenAI `text-embedding-3` models return L2-normalized vectors, so for those
/// `dot(a, b) == cosine(a, b)`. We use the general form here so that
/// [`MockEmbedder`] vectors in tests do not need to be normalized.
///
/// [`MockEmbedder`]: crate::embed::MockEmbedder
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}

// ---------------------------------------------------------------------------
// InMemoryL2Cache
// ---------------------------------------------------------------------------

/// In-memory L2 cache for unit tests and local development.
///
/// Uses a `Vec<CacheEntry>` protected by a `Mutex`. Lookup is O(n) — not
/// suitable for production, where [`PostgresL2Cache`] provides an HNSW index.
///
/// # Example
///
/// ```rust
/// use tt_cache::l2::{InMemoryL2Cache, L2Cache, CacheEntry};
/// use uuid::Uuid;
/// use chrono::Utc;
///
/// # #[tokio::main]
/// # async fn main() {
/// let cache = InMemoryL2Cache::new();
/// let entry = CacheEntry {
///     id: Uuid::new_v4(),
///     org_id: Uuid::new_v4(),
///     embedding: vec![1.0, 0.0],
///     response: b"{}".to_vec(),
///     model: "gpt-4o".to_string(),
///     input_tokens: 10,
///     output_tokens: 5,
///     hit_count: 0,
///     created_at: Utc::now(),
///     expires_at: Utc::now() + chrono::Duration::seconds(3600),
/// };
/// cache.insert(entry).await.unwrap();
/// # }
/// ```
pub struct InMemoryL2Cache {
    entries: Arc<Mutex<Vec<CacheEntry>>>,
}

impl InMemoryL2Cache {
    /// Create a new, empty in-memory L2 cache.
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Default for InMemoryL2Cache {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl L2Cache for InMemoryL2Cache {
    /// Insert `entry` into the in-memory store.
    async fn insert(&self, entry: CacheEntry) -> Result<(), CacheError> {
        let mut guard = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        guard.push(entry);
        Ok(())
    }

    /// Scan all entries for `org_id`, filter by expiry, compute cosine
    /// similarity, and return the best match if it meets `threshold`.
    async fn lookup(
        &self,
        org_id: Uuid,
        query_embedding: &[f32],
        threshold: f32,
    ) -> Result<Option<(CacheEntry, f32)>, CacheError> {
        let now = Utc::now();
        let guard = self.entries.lock().unwrap_or_else(|p| p.into_inner());

        let best = guard
            .iter()
            .filter(|e| e.org_id == org_id && e.expires_at > now)
            .map(|e| {
                let sim = cosine(&e.embedding, query_embedding);
                (e, sim)
            })
            .filter(|(_, sim)| *sim >= threshold)
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        Ok(best.map(|(e, sim)| (e.clone(), sim)))
    }

    /// Increment `hit_count` for the entry with the given `id`.
    async fn bump_hit_count(&self, id: Uuid) -> Result<(), CacheError> {
        let mut guard = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = guard.iter_mut().find(|e| e.id == id) {
            entry.hit_count += 1;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PostgresL2Cache
// ---------------------------------------------------------------------------

/// Production L2 cache backed by Postgres + pgvector HNSW index.
///
/// Uses the `<=>` cosine-distance operator from the `vector` extension.
/// The SQL query orders by cosine distance ascending (nearest first) and
/// applies the similarity filter `1 - distance >= threshold`.
///
/// # Setup
///
/// 1. Run `crates/core/migrations/0002_cache_entries.up.sql` against your
///    Postgres instance.
/// 2. Pass a connected [`sqlx::PgPool`] to [`PostgresL2Cache::new`].
///
/// # Note on `sqlx::query!` macro
///
/// We use the un-macro `sqlx::query(...)` form throughout to avoid the
/// compile-time `DATABASE_URL` requirement. This means type-checking of SQL
/// happens at runtime rather than compile time.
/// HNSW `ef_search` applied to org-filtered lookups.
///
/// pgvector's default is 40. With a `WHERE org_id = $1` filter, the HNSW graph
/// walk explores neighbours by vector distance and only *then* discards rows
/// that belong to other tenants — so under multi-tenant load the 40-candidate
/// list fills with other orgs' near vectors and the querying org's own nearest
/// neighbour can fall outside it, producing a false cache miss (poor recall).
/// Raising `ef_search` widens the candidate list so the org's vectors reliably
/// make the cut. 100 restores high recall at a small latency cost; tune via
/// [`PostgresL2Cache::with_ef_search`].
pub const DEFAULT_EF_SEARCH: i64 = 100;

pub struct PostgresL2Cache {
    pool: sqlx::PgPool,
    ef_search: i64,
}

impl PostgresL2Cache {
    /// Wrap an existing connected pool, using [`DEFAULT_EF_SEARCH`].
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self {
            pool,
            ef_search: DEFAULT_EF_SEARCH,
        }
    }

    /// Builder-style override of the HNSW `ef_search` used per lookup. Clamped
    /// to at least 1. Higher = better recall under multi-tenant load, slightly
    /// more work per query.
    #[must_use]
    pub fn with_ef_search(mut self, ef_search: i64) -> Self {
        self.ef_search = ef_search.max(1);
        self
    }

    /// The HNSW `ef_search` this cache applies per lookup.
    pub fn ef_search(&self) -> i64 {
        self.ef_search
    }
}

#[async_trait]
impl L2Cache for PostgresL2Cache {
    /// Insert a [`CacheEntry`] into the `cache_entries` table.
    ///
    /// Uses `ON CONFLICT DO NOTHING` so duplicate `id` values are silently
    /// ignored (callers always generate a fresh UUID per insert).
    async fn insert(&self, entry: CacheEntry) -> Result<(), CacheError> {
        // Convert Vec<f32> to pgvector::Vector for the Postgres `vector` column.
        let vec = pgvector::Vector::from(entry.embedding);

        sqlx::query(
            r#"
            INSERT INTO cache_entries
                (id, org_id, embedding, response, model,
                 input_tokens, output_tokens, hit_count, created_at, expires_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(entry.id)
        .bind(entry.org_id)
        .bind(vec)
        .bind(
            serde_json::from_slice::<serde_json::Value>(&entry.response)
                .map_err(CacheError::Serde)?,
        )
        .bind(&entry.model)
        .bind(entry.input_tokens as i64)
        .bind(entry.output_tokens as i64)
        .bind(entry.hit_count as i64)
        .bind(entry.created_at)
        .bind(entry.expires_at)
        .execute(&self.pool)
        .await
        .map_err(CacheError::Sqlx)?;

        Ok(())
    }

    /// Query the `cache_entries` table for the nearest entry by cosine
    /// similarity, scoped to `org_id` and non-expired rows.
    ///
    /// The `<=>` operator is pgvector's cosine-distance operator (lower = more
    /// similar). `1 - distance` converts to cosine similarity.
    async fn lookup(
        &self,
        org_id: Uuid,
        query_embedding: &[f32],
        threshold: f32,
    ) -> Result<Option<(CacheEntry, f32)>, CacheError> {
        let vec = pgvector::Vector::from(query_embedding.to_vec());

        // Run inside a transaction so `SET LOCAL hnsw.ef_search` scopes to this
        // query only. The raised ef_search keeps recall high once the org
        // filter discards other tenants' candidates (see [`DEFAULT_EF_SEARCH`]).
        // `SET` does not accept bind parameters, so the value is formatted in —
        // it is an `i64` we own (never user input), so this is injection-safe.
        let mut tx = self.pool.begin().await.map_err(CacheError::Sqlx)?;
        sqlx::query(&format!("SET LOCAL hnsw.ef_search = {}", self.ef_search))
            .execute(&mut *tx)
            .await
            .map_err(CacheError::Sqlx)?;

        let row = sqlx::query(
            r#"
            SELECT id, org_id, embedding, response, model,
                   input_tokens, output_tokens, hit_count, created_at, expires_at,
                   CAST(1.0 - (embedding <=> $2) AS REAL) AS similarity
              FROM cache_entries
             WHERE org_id = $1
               AND expires_at > now()
               AND CAST(1.0 - (embedding <=> $2) AS REAL) >= $3
             ORDER BY embedding <=> $2
             LIMIT 1
            "#,
        )
        .bind(org_id)
        .bind(vec)
        .bind(threshold)
        .fetch_optional(&mut *tx)
        .await
        .map_err(CacheError::Sqlx)?;

        tx.commit().await.map_err(CacheError::Sqlx)?;

        let Some(row) = row else {
            return Ok(None);
        };

        use sqlx::Row;
        let id: Uuid = row.try_get("id").map_err(CacheError::Sqlx)?;
        let org_id: Uuid = row.try_get("org_id").map_err(CacheError::Sqlx)?;
        let embedding_vec: pgvector::Vector = row.try_get("embedding").map_err(CacheError::Sqlx)?;
        let response_json: serde_json::Value = row.try_get("response").map_err(CacheError::Sqlx)?;
        let model: String = row.try_get("model").map_err(CacheError::Sqlx)?;
        let input_tokens: i64 = row.try_get("input_tokens").map_err(CacheError::Sqlx)?;
        let output_tokens: i64 = row.try_get("output_tokens").map_err(CacheError::Sqlx)?;
        let hit_count: i64 = row.try_get("hit_count").map_err(CacheError::Sqlx)?;
        let created_at: DateTime<Utc> = row.try_get("created_at").map_err(CacheError::Sqlx)?;
        let expires_at: DateTime<Utc> = row.try_get("expires_at").map_err(CacheError::Sqlx)?;
        let similarity: f32 = row.try_get("similarity").map_err(CacheError::Sqlx)?;

        let response_bytes = serde_json::to_vec(&response_json).map_err(CacheError::Serde)?;

        let entry = CacheEntry {
            id,
            org_id,
            embedding: embedding_vec.to_vec(),
            response: response_bytes,
            model,
            input_tokens: input_tokens as u64,
            output_tokens: output_tokens as u64,
            hit_count: hit_count as u64,
            created_at,
            expires_at,
        };

        Ok(Some((entry, similarity)))
    }

    /// Increment `hit_count` by 1 for the entry with the given `id`.
    ///
    /// Silently ignores the case where `id` no longer exists.
    async fn bump_hit_count(&self, id: Uuid) -> Result<(), CacheError> {
        sqlx::query("UPDATE cache_entries SET hit_count = hit_count + 1 WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(CacheError::Sqlx)?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_identical_unit_vectors() {
        let a = vec![1.0_f32, 0.0, 0.0];
        let sim = cosine(&a, &a);
        assert!((sim - 1.0).abs() < 1e-6, "identical vectors → sim ≈ 1");
    }

    #[test]
    fn cosine_orthogonal_vectors() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![0.0_f32, 1.0];
        let sim = cosine(&a, &b);
        assert!(sim.abs() < 1e-6, "orthogonal vectors → sim ≈ 0");
    }

    #[test]
    fn cosine_zero_vector_returns_zero() {
        let a = vec![0.0_f32, 0.0];
        let b = vec![1.0_f32, 0.0];
        assert_eq!(cosine(&a, &b), 0.0);
        assert_eq!(cosine(&b, &a), 0.0);
    }

    fn entry_at(id: Uuid, org_id: Uuid, embedding: Vec<f32>, now: DateTime<Utc>) -> CacheEntry {
        CacheEntry {
            id,
            org_id,
            embedding,
            response: b"{}".to_vec(),
            model: "gpt-4o".into(),
            input_tokens: 1,
            output_tokens: 1,
            hit_count: 0,
            created_at: now,
            expires_at: now + chrono::Duration::seconds(3600),
        }
    }

    /// Recall regression under multi-tenant load: with N orgs each holding a
    /// planted near-duplicate of the query (plus noise), every org's lookup
    /// must recall ITS OWN planted entry — never another tenant's, even though
    /// every org stores an identical match vector. This is the contract the
    /// Postgres HNSW path must also uphold (which is why `PostgresL2Cache`
    /// raises `hnsw.ef_search` so the org filter doesn't starve recall).
    #[tokio::test]
    async fn multi_tenant_recall_each_org_finds_its_own_match() {
        let cache = InMemoryL2Cache::new();
        let now = Utc::now();
        let query = vec![1.0_f32, 0.0, 0.0, 0.0];
        let noise = vec![0.0_f32, 1.0, 0.0, 0.0]; // orthogonal → sim 0

        const N_ORGS: usize = 50;
        let mut orgs = Vec::new();
        let mut match_ids = Vec::new();
        for _ in 0..N_ORGS {
            let org = Uuid::new_v4();
            let match_id = Uuid::new_v4();
            // Planted near-duplicate of the query for this org.
            cache
                .insert(entry_at(match_id, org, query.clone(), now))
                .await
                .unwrap();
            // Noise entries for the same org that must never out-rank the match.
            for _ in 0..3 {
                cache
                    .insert(entry_at(Uuid::new_v4(), org, noise.clone(), now))
                    .await
                    .unwrap();
            }
            orgs.push(org);
            match_ids.push(match_id);
        }

        for (k, org) in orgs.iter().enumerate() {
            let (hit, sim) = cache
                .lookup(*org, &query, 0.9)
                .await
                .unwrap()
                .unwrap_or_else(|| panic!("org {k} should recall its planted match"));
            assert_eq!(hit.id, match_ids[k], "org {k} recalled the wrong entry");
            assert_eq!(hit.org_id, *org, "lookup must never cross tenants");
            assert!(sim > 0.99, "org {k} similarity too low: {sim}");
        }
    }

    /// Tenant isolation: an org with NO matching entry must miss, even when a
    /// different org holds a perfect match for the same vector.
    #[tokio::test]
    async fn lookup_does_not_leak_across_orgs() {
        let cache = InMemoryL2Cache::new();
        let now = Utc::now();
        let q = vec![1.0_f32, 0.0];
        let org_with_match = Uuid::new_v4();
        let org_without = Uuid::new_v4();
        cache
            .insert(entry_at(Uuid::new_v4(), org_with_match, q.clone(), now))
            .await
            .unwrap();

        assert!(cache
            .lookup(org_with_match, &q, 0.9)
            .await
            .unwrap()
            .is_some());
        assert!(
            cache.lookup(org_without, &q, 0.9).await.unwrap().is_none(),
            "an org must not see another org's cached entry"
        );
    }
}
