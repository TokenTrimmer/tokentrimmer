//! In-process cache for `RepoIndex`, keyed on repo root + a max-mtime fingerprint.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use tt_inspect_core::walk::walk;

use crate::index::RepoIndex;

/// Collision-free cache key derived from the directory walk.
///
/// Using a struct with two distinct fields avoids the hash-collision risk of
/// the old `count.wrapping_mul(...).wrapping_add(max_mtime)` combinator: two
/// different `(file_count, max_mtime_ns)` pairs can never produce the same
/// `Fingerprint`.
#[derive(PartialEq, Eq, Clone)]
struct Fingerprint {
    file_count: u64,
    max_mtime_ns: u128,
}

#[derive(Default)]
pub struct IndexCache {
    inner: Mutex<HashMap<PathBuf, (Fingerprint, Arc<RepoIndex>)>>,
}

impl IndexCache {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Return a cached `RepoIndex` if the fingerprint is unchanged, otherwise
    /// rebuild and cache it.
    ///
    /// **Locking note:** `RepoIndex::build` runs while the mutex is held, so
    /// concurrent first-builds for different roots serialize.  This is
    /// acceptable for v1 (single coding-agent session).  A drop-lock →
    /// build → reacquire pattern would allow true parallelism but adds
    /// complexity; defer if high concurrency becomes a concern.
    #[must_use]
    pub fn get_or_build(&self, root: &Path) -> Arc<RepoIndex> {
        let fp = fingerprint(root);
        let mut guard = self.inner.lock().expect("index cache poisoned");
        if let Some((cached_fp, idx)) = guard.get(root) {
            if *cached_fp == fp {
                return Arc::clone(idx);
            }
        }
        let idx = Arc::new(RepoIndex::build(root));
        guard.insert(root.to_path_buf(), (fp, Arc::clone(&idx)));
        idx
    }
}

fn fingerprint(root: &Path) -> Fingerprint {
    let mut file_count: u64 = 0;
    let mut max_mtime_ns: u128 = 0;
    for (path, _lang) in walk(root) {
        file_count += 1;
        if let Ok(meta) = std::fs::metadata(&path) {
            if let Ok(modified) = meta.modified() {
                if let Ok(dur) = modified.duration_since(SystemTime::UNIX_EPOCH) {
                    max_mtime_ns = max_mtime_ns.max(dur.as_nanos());
                }
            }
        }
    }
    Fingerprint {
        file_count,
        max_mtime_ns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn cache_reuses_until_mtime_changes() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.py"), "def a():\n    pass\n").unwrap();
        let cache = IndexCache::new();
        let i1 = cache.get_or_build(root);
        let i2 = cache.get_or_build(root);
        assert!(
            std::sync::Arc::ptr_eq(&i1, &i2),
            "second call should return the SAME cached Arc"
        );
        assert_eq!(i1.files().len(), i2.files().len());
        fs::write(root.join("b.py"), "def b():\n    pass\n").unwrap();
        let i3 = cache.get_or_build(root);
        assert_eq!(
            i3.files().len(),
            2,
            "new file should be picked up after mtime change"
        );
    }
}
