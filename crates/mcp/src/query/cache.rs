//! Result cache — FILE handles ONLY, exact by construction.
//!
//! Keyed on `(blake3 of file content, canonical serialization of the typed
//! AggregationSpec)`: any byte change to the file changes the key, and two
//! JSON spellings of the same spec (field order) produce the same key.
//!
//! **Postgres results are NEVER cached**: with no honest content fingerprint
//! for a live database, a cache is a stale-wrong-number machine. Deferred
//! until a real fingerprint exists (recorded follow-up).
//!
//! The content hash is computed immediately before execution; a write
//! landing between the hash and the read is the same accepted residual
//! TOCTOU window documented for the root check in [`crate::query`].

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;

use crate::error::McpError;

use super::spec::AggregationSpec;
use super::QueryOutcome;

/// Default cache capacity (entries).
pub const DEFAULT_CACHE_CAPACITY: usize = 256;

/// Cache key: exact content fingerprint + canonical query spec.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    content_hash: [u8; 32],
    spec_canonical: String,
}

impl CacheKey {
    /// Build a key by streaming the file at `path` (already canonicalized +
    /// root-checked by the caller) through blake3.
    pub(crate) fn for_file(path: &Path, spec: &AggregationSpec) -> Result<Self, McpError> {
        let mut hasher = blake3::Hasher::new();
        let mut f = std::fs::File::open(path)
            .map_err(|e| McpError::Internal(format!("dataset file is unavailable ({})", e.kind())))?;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = f
                .read(&mut buf)
                .map_err(|e| McpError::Internal(format!("dataset file read failed ({})", e.kind())))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(Self {
            content_hash: *hasher.finalize().as_bytes(),
            spec_canonical: spec.canonical(),
        })
    }
}

struct Entry {
    outcome: QueryOutcome,
    last_used: u64,
}

struct Inner {
    map: HashMap<CacheKey, Entry>,
    clock: u64,
}

/// In-process LRU result cache for file-handle aggregations.
pub struct QueryCache {
    inner: Mutex<Inner>,
    capacity: usize,
}

impl QueryCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                clock: 0,
            }),
            capacity: capacity.max(1),
        }
    }

    pub fn get(&self, key: &CacheKey) -> Option<QueryOutcome> {
        let mut inner = self.inner.lock().expect("query cache lock");
        inner.clock += 1;
        let clock = inner.clock;
        let entry = inner.map.get_mut(key)?;
        entry.last_used = clock;
        Some(entry.outcome.clone())
    }

    /// Insert (or refresh) an entry, evicting the least-recently-used one
    /// when at capacity.
    pub fn insert(&self, key: CacheKey, outcome: QueryOutcome) {
        let mut inner = self.inner.lock().expect("query cache lock");
        inner.clock += 1;
        let clock = inner.clock;
        if !inner.map.contains_key(&key) && inner.map.len() >= self.capacity {
            if let Some(lru) = inner
                .map
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| k.clone())
            {
                inner.map.remove(&lru);
            }
        }
        inner.map.insert(
            key,
            Entry {
                outcome,
                last_used: clock,
            },
        );
    }

    /// Drop an entry (verify-mismatch invalidation).
    pub fn remove(&self, key: &CacheKey) {
        self.inner.lock().expect("query cache lock").map.remove(key);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().map.len()
    }
}

impl Default for QueryCache {
    fn default() -> Self {
        Self::new(DEFAULT_CACHE_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(v: serde_json::Value) -> AggregationSpec {
        serde_json::from_value(v).unwrap()
    }

    fn outcome(tag: u64) -> QueryOutcome {
        QueryOutcome {
            columns: vec!["count".into()],
            rows: vec![vec![json!(tag)]],
            skipped_rows: 0,
            rows_scanned: Some(tag),
        }
    }

    #[test]
    fn same_content_same_spec_hits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.csv");
        std::fs::write(&path, "a,b\n1,2\n").unwrap();
        let s = spec(json!({"op":"count"}));
        let k1 = CacheKey::for_file(&path, &s).unwrap();
        let k2 = CacheKey::for_file(&path, &s).unwrap();
        assert_eq!(k1, k2);

        let cache = QueryCache::new(4);
        assert!(cache.get(&k1).is_none(), "cold cache misses");
        cache.insert(k1, outcome(1));
        assert_eq!(cache.get(&k2).unwrap().rows_scanned, Some(1));
    }

    #[test]
    fn any_byte_change_changes_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.csv");
        std::fs::write(&path, "a,b\n1,2\n").unwrap();
        let s = spec(json!({"op":"count"}));
        let k1 = CacheKey::for_file(&path, &s).unwrap();
        std::fs::write(&path, "a,b\n1,3\n").unwrap(); // one byte differs
        let k2 = CacheKey::for_file(&path, &s).unwrap();
        assert_ne!(k1, k2);
    }

    #[test]
    fn key_is_spec_field_order_insensitive_but_spec_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.csv");
        std::fs::write(&path, "a,b\n1,2\n").unwrap();
        let s1 = spec(json!({"op":"sum","column":"a","group_by":["b"]}));
        let s2 = spec(json!({"group_by":["b"],"column":"a","op":"sum"}));
        let s3 = spec(json!({"op":"sum","column":"b"}));
        assert_eq!(
            CacheKey::for_file(&path, &s1).unwrap(),
            CacheKey::for_file(&path, &s2).unwrap()
        );
        assert_ne!(
            CacheKey::for_file(&path, &s1).unwrap(),
            CacheKey::for_file(&path, &s3).unwrap()
        );
    }

    #[test]
    fn lru_evicts_least_recently_used_at_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let mut keys = Vec::new();
        for i in 0..3 {
            let path = dir.path().join(format!("{i}.csv"));
            std::fs::write(&path, format!("a\n{i}\n")).unwrap();
            keys.push(CacheKey::for_file(&path, &spec(json!({"op":"count"}))).unwrap());
        }
        let cache = QueryCache::new(2);
        cache.insert(keys[0].clone(), outcome(0));
        cache.insert(keys[1].clone(), outcome(1));
        // Touch key 0 so key 1 is the LRU.
        assert!(cache.get(&keys[0]).is_some());
        cache.insert(keys[2].clone(), outcome(2));
        assert_eq!(cache.len(), 2);
        assert!(cache.get(&keys[1]).is_none(), "LRU entry evicted");
        assert!(cache.get(&keys[0]).is_some());
        assert!(cache.get(&keys[2]).is_some());
    }

    #[test]
    fn remove_invalidates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.csv");
        std::fs::write(&path, "a\n1\n").unwrap();
        let k = CacheKey::for_file(&path, &spec(json!({"op":"count"}))).unwrap();
        let cache = QueryCache::default();
        cache.insert(k.clone(), outcome(7));
        cache.remove(&k);
        assert!(cache.get(&k).is_none());
    }
}
