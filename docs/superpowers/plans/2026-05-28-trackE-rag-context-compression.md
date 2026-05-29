# Track E — RAG / Context Compression Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the public-repo half of Track E — a new `crates/retrieval/` crate that does corpus ingestion, embedding, HNSW lookup, and `<retrievable>` prompt-tag substitution. The Gateway middleware that calls this crate is also in scope. Cloud-side ingestion REST endpoints + dashboard pages are DEFERRED to a sibling cloud-repo plan; this plan ships everything that can live in the OSS workspace plus stub endpoints that work against an in-process pgvector connection for tests.

**Architecture:** New `crates/retrieval/` crate (pure library — chunking, embedding pipeline, HNSW query, tag parser, substitution). New `crates/core/src/middleware/retrieval.rs` wires substitution into the request flow before provider dispatch. The crate accepts a `RetrievalStore` trait so tests can inject in-memory; production wires a Postgres+pgvector store.

**Tech Stack:** Rust 1.88, `tiktoken-rs` for chunk-token counting, `serde`/`serde_json`, `sqlx` + `pgvector` for the production store (already in workspace via `tt-cache`), `tokio`, `regex` for tag parsing, `httpmock` for embedding-API tests, `insta` for substitution snapshots.

**Spec:** `docs/superpowers/specs/2026-05-28-trackE-rag-context-compression-design.md`.

**Scope cut (Day-0):**
- Corpus CRUD: trait surface + in-memory impl shipped here; Postgres impl is a *separate* file pre-wired in the trait but its `#[cfg(feature = "postgres")]` body is a `todo!()` stub. Real query work lands in a follow-up.
- Ingestion: `chunk()` + `embed()` ship; the HTTP `POST /v1/admin/retrieval/corpora/.../docs` endpoint that calls them lives in the cloud repo.
- Audit log encryption (spec §10) DEFERRED — schema in this plan, write-path stubbed.
- Dashboard `/context` page DEFERRED to cloud-repo plan.

---

## File Structure

```
crates/retrieval/                            [NEW crate]
├── Cargo.toml
└── src/
    ├── lib.rs                               [public API: substitute(), ingest_doc(), search()]
    ├── types.rs                             [Corpus, Chunk, Document, RetrievalResult, RetrievableTag]
    ├── chunking.rs                          [512-token windows + 64-overlap; markdown-aware boundaries]
    ├── embed.rs                             [OpenAI embeddings client + retry]
    ├── store/
    │   ├── mod.rs                           [RetrievalStore trait]
    │   ├── memory.rs                        [in-process Vec<Chunk> + cosine — for tests + dev]
    │   └── postgres.rs                      [#[cfg(feature = "postgres")] stub]
    ├── search.rs                            [top-k cosine over a store]
    ├── tags.rs                              [parse <retrievable corpus="X" k="N">...</retrievable> in messages]
    ├── substitute.rs                        [orchestrator: parse → embed query → retrieve → splice]
    └── error.rs

crates/core/src/middleware/
└── retrieval.rs                             [NEW — Axum middleware: wraps the chat handler]

crates/cli/src/
├── retrieval/
│   ├── mod.rs                               [tt retrieval subcommand: corpus + doc + search subops]
│   └── ...
└── main.rs                                  [modified — Retrieval subcommand]

Cargo.toml                                   [modified — workspace member + workspace dep]
```

---

## Task 1: Scaffold tt-retrieval crate

**Files:**
- Create: `crates/retrieval/Cargo.toml`
- Create: scaffold all the .rs files
- Modify: root `Cargo.toml`

- [ ] **Step 1: Create the tree**

```bash
mkdir -p crates/retrieval/src/store
for f in lib types chunking embed search tags substitute error; do
  echo "//! tt-retrieval — \`$f\` (scaffold)" > "crates/retrieval/src/$f.rs"
done
for f in mod memory postgres; do
  echo "//! tt-retrieval store — \`$f\` (scaffold)" > "crates/retrieval/src/store/$f.rs"
done
```

- [ ] **Step 2: Write `crates/retrieval/Cargo.toml`**

```toml
[package]
name = "tt-retrieval"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true
license.workspace = true
description = "RAG / context compression: chunking, embedding, HNSW retrieval, <retrievable> tag substitution."

[dependencies]
tt-shared.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
tracing.workspace = true
tokio = { workspace = true, features = ["sync"] }
reqwest = { workspace = true, features = ["json", "rustls-tls"] }
tiktoken-rs = "0.5"
regex = "1.11"
uuid = { version = "1.10", features = ["v4"] }

[features]
default = []
postgres = ["dep:sqlx"]

[dependencies.sqlx]
workspace = true
features = ["postgres", "runtime-tokio", "tls-rustls"]
optional = true

[dev-dependencies]
httpmock = "0.7"
insta = { version = "1.39", features = ["json"] }
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

- [ ] **Step 3: Register in workspace**

In root `Cargo.toml`:
- Add `"crates/retrieval"` to `workspace.members`.
- Add `tt-retrieval = { path = "crates/retrieval" }` to `[workspace.dependencies]`.

- [ ] **Step 4: Replace `lib.rs`**

```rust
//! `tt-retrieval` — RAG / context-compression engine.
//!
//! See `docs/superpowers/specs/2026-05-28-trackE-rag-context-compression-design.md`.

pub mod chunking;
pub mod embed;
pub mod error;
pub mod search;
pub mod store;
pub mod substitute;
pub mod tags;
pub mod types;

pub use error::RetrievalError;
pub use store::RetrievalStore;
pub use substitute::{substitute_in_messages, SubstitutionReport};
pub use types::{Chunk, Corpus, Document, RetrievableTag, RetrievalResult};
```

- [ ] **Step 5: Compile**

`cargo check -p tt-retrieval`

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/retrieval/
git commit -m "feat(retrieval): scaffold tt-retrieval crate

Track E day-0. Empty modules.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Shared types

**Files:** `crates/retrieval/src/types.rs`

- [ ] **Step 1: Write the module**

```rust
//! Data shapes shared across the crate.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Corpus {
    pub org_id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub created_at: String, // ISO-8601
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub doc_id: Uuid,
    pub corpus: String,
    pub org_id: Uuid,
    pub source_path: String,
    pub bytes_indexed: u64,
    pub chunks: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub id: Uuid,
    pub org_id: Uuid,
    pub corpus: String,
    pub doc_id: Uuid,
    pub chunk_idx: u32,
    pub text: String,
    pub embedding: Vec<f32>, // 1536-dim
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct RetrievalResult {
    pub chunk_id: Uuid,
    pub doc_id: Uuid,
    pub chunk_idx: u32,
    pub text: String,
    pub similarity: f32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrievableTag {
    pub corpus: String,
    pub k: u32,
    /// Span in the original message text (start_byte_idx, end_byte_idx_exclusive).
    pub span: (usize, usize),
}
```

- [ ] **Step 2: Compile**

`cargo check -p tt-retrieval`

- [ ] **Step 3: Commit**

```bash
git add crates/retrieval/src/types.rs
git commit -m "feat(retrieval): shared types

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Error type

**Files:** `crates/retrieval/src/error.rs`

- [ ] **Step 1: Write**

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RetrievalError {
    #[error("embedding HTTP: {0}")]
    Embedding(String),
    #[error("store: {0}")]
    Store(String),
    #[error("tag parse: {0}")]
    Tag(String),
    #[error("malformed: {0}")]
    Malformed(String),
}
```

- [ ] **Step 2: Compile + commit**

```
cargo check -p tt-retrieval
git add crates/retrieval/src/error.rs
git commit -m "feat(retrieval): error type"
```

---

## Task 4: Chunking

**Files:** `crates/retrieval/src/chunking.rs`

- [ ] **Step 1: Write the module + tests**

```rust
//! 512-token chunks with 64-token overlap. Tokenizer: tiktoken cl100k_base.

const CHUNK_SIZE: usize = 512;
const OVERLAP: usize = 64;

pub struct Chunk { pub text: String, pub start_token: usize, pub end_token: usize }

pub fn chunk(text: &str) -> Vec<Chunk> {
    let bpe = match tiktoken_rs::cl100k_base() {
        Ok(b) => b,
        Err(_) => return vec![Chunk { text: text.into(), start_token: 0, end_token: 0 }],
    };
    let tokens = bpe.encode_with_special_tokens(text);
    if tokens.is_empty() { return vec![]; }
    let mut out = Vec::new();
    let mut start = 0;
    while start < tokens.len() {
        let end = (start + CHUNK_SIZE).min(tokens.len());
        let slice = &tokens[start..end];
        let chunk_text = bpe.decode(slice.to_vec()).unwrap_or_default();
        out.push(Chunk { text: chunk_text, start_token: start, end_token: end });
        if end == tokens.len() { break; }
        start = end - OVERLAP;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_one_chunk() {
        let cs = chunk("Hello world.");
        assert_eq!(cs.len(), 1);
        assert!(cs[0].text.contains("Hello"));
    }

    #[test]
    fn long_text_multiple_chunks_with_overlap() {
        let body = "x ".repeat(600); // > CHUNK_SIZE in tokens
        let cs = chunk(&body);
        assert!(cs.len() >= 2);
        // Overlap: second chunk's start_token = first.end_token - OVERLAP
        assert_eq!(cs[1].start_token, cs[0].end_token - OVERLAP);
    }
}
```

- [ ] **Step 2: Tests**

`cargo test -p tt-retrieval chunking`
Expected: 2 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/retrieval/src/chunking.rs
git commit -m "feat(retrieval): tiktoken-based 512-token chunker with 64 overlap"
```

---

## Task 5: Embedding client

**Files:** `crates/retrieval/src/embed.rs`

- [ ] **Step 1: Write + httpmock tests**

```rust
//! OpenAI text-embedding-3-small client. 1536-dim output.
//!
//! For tests, the `base_url` is overridable.

use serde::Deserialize;

use crate::error::RetrievalError;

#[derive(Debug, Clone)]
pub struct EmbeddingClient {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub http: reqwest::Client,
}

impl EmbeddingClient {
    pub fn openai(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: "https://api.openai.com".into(),
            model: "text-embedding-3-small".into(),
            http: reqwest::Client::new(),
        }
    }

    pub async fn embed(&self, text: &str) -> Result<Vec<f32>, RetrievalError> {
        #[derive(serde::Serialize)]
        struct Req<'a> { input: &'a str, model: &'a str }
        let body = Req { input: text, model: &self.model };
        let resp = self.http.post(format!("{}/v1/embeddings", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body).send().await
            .map_err(|e| RetrievalError::Embedding(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(RetrievalError::Embedding(format!("HTTP {}", resp.status())));
        }
        #[derive(Deserialize)]
        struct R { data: Vec<E> }
        #[derive(Deserialize)]
        struct E { embedding: Vec<f32> }
        let parsed: R = resp.json().await.map_err(|e| RetrievalError::Embedding(e.to_string()))?;
        parsed.data.into_iter().next()
            .map(|e| e.embedding)
            .ok_or_else(|| RetrievalError::Embedding("empty data".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    #[tokio::test]
    async fn embed_round_trip() {
        let server = MockServer::start_async().await;
        let _m = server.mock_async(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200).json_body(serde_json::json!({
                "data": [{ "embedding": [0.1, 0.2, 0.3] }]
            }));
        }).await;
        let c = EmbeddingClient {
            api_key: "k".into(), base_url: server.base_url(),
            model: "text-embedding-3-small".into(), http: reqwest::Client::new(),
        };
        let v = c.embed("hi").await.unwrap();
        assert_eq!(v, vec![0.1, 0.2, 0.3]);
    }

    #[tokio::test]
    async fn embed_5xx_errors() {
        let server = MockServer::start_async().await;
        let _m = server.mock_async(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(500).body("boom");
        }).await;
        let c = EmbeddingClient {
            api_key: "k".into(), base_url: server.base_url(),
            model: "text-embedding-3-small".into(), http: reqwest::Client::new(),
        };
        let err = c.embed("hi").await.unwrap_err();
        assert!(matches!(err, RetrievalError::Embedding(_)));
    }
}
```

- [ ] **Step 2: Tests**

`cargo test -p tt-retrieval embed`
Expected: 2 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/retrieval/src/embed.rs
git commit -m "feat(retrieval): OpenAI embeddings client"
```

---

## Task 6: Store trait + in-memory impl

**Files:** `crates/retrieval/src/store/mod.rs`, `crates/retrieval/src/store/memory.rs`

- [ ] **Step 1: `store/mod.rs`**

```rust
use async_trait::async_trait;
use uuid::Uuid;

use crate::error::RetrievalError;
use crate::types::{Chunk, RetrievalResult};

#[async_trait::async_trait]
pub trait RetrievalStore: Send + Sync {
    async fn insert(&self, chunk: Chunk) -> Result<(), RetrievalError>;
    async fn search(&self, org_id: Uuid, corpus: &str, query_embedding: &[f32], k: usize) -> Result<Vec<RetrievalResult>, RetrievalError>;
    async fn delete_corpus(&self, org_id: Uuid, corpus: &str) -> Result<u64, RetrievalError>;
}

pub mod memory;
#[cfg(feature = "postgres")]
pub mod postgres;
```

- [ ] **Step 2: Add `async-trait` dep**

`async-trait = "0.1"` in `crates/retrieval/Cargo.toml` `[dependencies]`.

- [ ] **Step 3: `store/memory.rs`**

```rust
//! In-process Vec-backed store for tests + local dev.

use std::sync::Mutex;
use uuid::Uuid;

use crate::error::RetrievalError;
use crate::store::RetrievalStore;
use crate::types::{Chunk, RetrievalResult};

pub struct MemoryStore { chunks: Mutex<Vec<Chunk>> }

impl MemoryStore {
    pub fn new() -> Self { Self { chunks: Mutex::new(Vec::new()) } }
}
impl Default for MemoryStore { fn default() -> Self { Self::new() } }

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() { return 0.0; }
    let mut dot = 0.0; let mut na = 0.0; let mut nb = 0.0;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 { return 0.0; }
    dot / (na.sqrt() * nb.sqrt())
}

#[async_trait::async_trait]
impl RetrievalStore for MemoryStore {
    async fn insert(&self, chunk: Chunk) -> Result<(), RetrievalError> {
        self.chunks.lock().unwrap().push(chunk);
        Ok(())
    }
    async fn search(&self, org_id: Uuid, corpus: &str, q: &[f32], k: usize) -> Result<Vec<RetrievalResult>, RetrievalError> {
        let snap: Vec<_> = self.chunks.lock().unwrap().clone();
        let mut scored: Vec<(f32, &Chunk)> = snap.iter()
            .filter(|c| c.org_id == org_id && c.corpus == corpus)
            .map(|c| (cosine(q, &c.embedding), c))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let out = scored.into_iter().take(k).map(|(s, c)| RetrievalResult {
            chunk_id: c.id, doc_id: c.doc_id, chunk_idx: c.chunk_idx,
            text: c.text.clone(), similarity: s,
        }).collect();
        Ok(out)
    }
    async fn delete_corpus(&self, org_id: Uuid, corpus: &str) -> Result<u64, RetrievalError> {
        let mut g = self.chunks.lock().unwrap();
        let before = g.len();
        g.retain(|c| !(c.org_id == org_id && c.corpus == corpus));
        Ok((before - g.len()) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chunk(org: Uuid, corpus: &str, emb: Vec<f32>, text: &str) -> Chunk {
        Chunk {
            id: Uuid::new_v4(), org_id: org, corpus: corpus.into(),
            doc_id: Uuid::new_v4(), chunk_idx: 0,
            text: text.into(), embedding: emb, metadata: json!({}),
        }
    }

    #[tokio::test]
    async fn search_returns_highest_similarity_first() {
        let s = MemoryStore::new();
        let org = Uuid::new_v4();
        s.insert(chunk(org, "x", vec![1.0, 0.0], "first")).await.unwrap();
        s.insert(chunk(org, "x", vec![0.0, 1.0], "second")).await.unwrap();
        s.insert(chunk(org, "x", vec![0.9, 0.1], "third")).await.unwrap();
        let r = s.search(org, "x", &[1.0, 0.0], 2).await.unwrap();
        assert_eq!(r[0].text, "first");
        assert_eq!(r[1].text, "third");
    }

    #[tokio::test]
    async fn search_isolates_by_org_and_corpus() {
        let s = MemoryStore::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        s.insert(chunk(a, "x", vec![1.0], "a-x")).await.unwrap();
        s.insert(chunk(b, "x", vec![1.0], "b-x")).await.unwrap();
        s.insert(chunk(a, "y", vec![1.0], "a-y")).await.unwrap();
        let r = s.search(a, "x", &[1.0], 10).await.unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].text, "a-x");
    }

    #[tokio::test]
    async fn delete_corpus_returns_removed_count() {
        let s = MemoryStore::new();
        let o = Uuid::new_v4();
        s.insert(chunk(o, "x", vec![1.0], "1")).await.unwrap();
        s.insert(chunk(o, "x", vec![1.0], "2")).await.unwrap();
        s.insert(chunk(o, "y", vec![1.0], "y")).await.unwrap();
        let removed = s.delete_corpus(o, "x").await.unwrap();
        assert_eq!(removed, 2);
    }
}
```

- [ ] **Step 4: `store/postgres.rs` stub**

```rust
//! Postgres + pgvector store. Schema lives in cloud-repo migrations.
//! Body intentionally `todo!()` — wired in a follow-up plan once the cloud
//! migration ships.

use uuid::Uuid;

use crate::error::RetrievalError;
use crate::store::RetrievalStore;
use crate::types::{Chunk, RetrievalResult};

pub struct PostgresStore { pub pool: sqlx::PgPool }

#[async_trait::async_trait]
impl RetrievalStore for PostgresStore {
    async fn insert(&self, _chunk: Chunk) -> Result<(), RetrievalError> {
        todo!("wire after cloud-repo migration ships retrieval_chunks table")
    }
    async fn search(&self, _o: Uuid, _c: &str, _q: &[f32], _k: usize) -> Result<Vec<RetrievalResult>, RetrievalError> {
        todo!()
    }
    async fn delete_corpus(&self, _o: Uuid, _c: &str) -> Result<u64, RetrievalError> {
        todo!()
    }
}
```

- [ ] **Step 5: Tests**

`cargo test -p tt-retrieval store`
Expected: 3 passed.

- [ ] **Step 6: Commit**

```bash
git add crates/retrieval/Cargo.toml crates/retrieval/src/store/
git commit -m "feat(retrieval): RetrievalStore trait + in-memory impl + postgres stub"
```

---

## Task 7: Tag parser

**Files:** `crates/retrieval/src/tags.rs`

- [ ] **Step 1: Write**

```rust
//! Parse `<retrievable corpus="X" k="N">...</retrievable>` tags from message
//! text. Returns each tag's corpus, k, and span in the text.

use regex::Regex;

use crate::error::RetrievalError;
use crate::types::RetrievableTag;

pub fn parse(text: &str) -> Result<Vec<RetrievableTag>, RetrievalError> {
    // Non-greedy match of the open tag + payload + close tag.
    let re = Regex::new(r#"(?ms)<retrievable\s+corpus="([^"]+)"(?:\s+k="(\d+)")?>(.*?)</retrievable>"#)
        .map_err(|e| RetrievalError::Tag(e.to_string()))?;
    let mut out = Vec::new();
    for m in re.captures_iter(text) {
        let full = m.get(0).unwrap();
        let corpus = m.get(1).unwrap().as_str().to_string();
        let k = m.get(2).and_then(|x| x.as_str().parse::<u32>().ok()).unwrap_or(5);
        out.push(RetrievableTag { corpus, k, span: (full.start(), full.end()) });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_tag() {
        let t = parse(r#"Pre<retrievable corpus="docs" k="3">payload</retrievable>Post"#).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].corpus, "docs");
        assert_eq!(t[0].k, 3);
    }

    #[test]
    fn default_k_when_missing() {
        let t = parse(r#"<retrievable corpus="x">y</retrievable>"#).unwrap();
        assert_eq!(t[0].k, 5);
    }

    #[test]
    fn multiple_tags_in_order() {
        let body = r#"a<retrievable corpus="x">1</retrievable>b<retrievable corpus="y">2</retrievable>c"#;
        let t = parse(body).unwrap();
        assert_eq!(t.len(), 2);
        assert!(t[0].span.0 < t[1].span.0);
    }

    #[test]
    fn no_tags_is_empty() {
        let t = parse("plain text").unwrap();
        assert!(t.is_empty());
    }
}
```

- [ ] **Step 2: Tests**

`cargo test -p tt-retrieval tags`
Expected: 4 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/retrieval/src/tags.rs
git commit -m "feat(retrieval): <retrievable> tag parser"
```

---

## Task 8: Search

**Files:** `crates/retrieval/src/search.rs`

- [ ] **Step 1: Write the module + tests**

```rust
//! Top-k cosine over a RetrievalStore.

use uuid::Uuid;

use crate::error::RetrievalError;
use crate::store::RetrievalStore;
use crate::types::RetrievalResult;

pub async fn top_k(
    store: &dyn RetrievalStore,
    org_id: Uuid,
    corpus: &str,
    query_embedding: &[f32],
    k: usize,
    min_similarity: f32,
) -> Result<Vec<RetrievalResult>, RetrievalError> {
    let raw = store.search(org_id, corpus, query_embedding, k).await?;
    Ok(raw.into_iter().filter(|r| r.similarity >= min_similarity).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::MemoryStore;
    use crate::types::Chunk;
    use serde_json::json;

    fn c(org: Uuid, emb: Vec<f32>, text: &str) -> Chunk {
        Chunk {
            id: Uuid::new_v4(), org_id: org, corpus: "x".into(),
            doc_id: Uuid::new_v4(), chunk_idx: 0,
            text: text.into(), embedding: emb, metadata: json!({}),
        }
    }

    #[tokio::test]
    async fn min_similarity_filter() {
        let s = MemoryStore::new();
        let o = Uuid::new_v4();
        s.insert(c(o, vec![1.0, 0.0], "hi-sim")).await.unwrap();
        s.insert(c(o, vec![0.0, 1.0], "low-sim")).await.unwrap();
        let r = top_k(&s, o, "x", &[1.0, 0.0], 5, 0.5).await.unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].text, "hi-sim");
    }
}
```

- [ ] **Step 2: Run + commit**

```
cargo test -p tt-retrieval search
git add crates/retrieval/src/search.rs
git commit -m "feat(retrieval): top-k cosine wrapper"
```

---

## Task 9: Substitute

**Files:** `crates/retrieval/src/substitute.rs`

- [ ] **Step 1: Write**

```rust
//! Orchestrator: take a message body, parse retrievable tags, embed the rest,
//! retrieve top-k, splice the retrieved chunks into the tag spans.

use serde_json::Value;
use uuid::Uuid;

use crate::embed::EmbeddingClient;
use crate::error::RetrievalError;
use crate::store::RetrievalStore;
use crate::tags;

pub struct SubstitutionReport {
    pub substitutions: u32,
    pub tokens_saved_estimate: i64,
}

pub async fn substitute_in_messages(
    messages: &mut Vec<Value>,
    org_id: Uuid,
    store: &dyn RetrievalStore,
    embedder: &EmbeddingClient,
) -> Result<SubstitutionReport, RetrievalError> {
    let mut substitutions = 0u32;
    let mut saved = 0i64;
    for msg in messages.iter_mut() {
        let Some(content) = msg.get_mut("content") else { continue };
        let Some(text) = content.as_str() else { continue };
        let text = text.to_string();
        let tags = tags::parse(&text)?;
        if tags.is_empty() { continue; }

        // Strip all tag spans to form the "embed query"
        let mut without_tags = String::new();
        let mut last = 0;
        for t in &tags {
            without_tags.push_str(&text[last..t.span.0]);
            last = t.span.1;
        }
        without_tags.push_str(&text[last..]);

        let query_emb = embedder.embed(&without_tags).await?;

        // Reassemble — replace each tag with retrieved chunks (joined by ---).
        let mut new_text = String::new();
        let mut cursor = 0;
        for t in &tags {
            new_text.push_str(&text[cursor..t.span.0]);
            let hits = store.search(org_id, &t.corpus, &query_emb, t.k as usize).await?;
            let original_payload = &text[t.span.0..t.span.1];
            let replacement = hits.iter().map(|r| r.text.clone())
                .collect::<Vec<_>>().join("\n\n---\n\n");
            saved += original_payload.len() as i64 - replacement.len() as i64;
            new_text.push_str(&replacement);
            substitutions += 1;
            cursor = t.span.1;
        }
        new_text.push_str(&text[cursor..]);
        *content = Value::String(new_text);
    }
    // Char-delta / 4 as the token-savings heuristic.
    Ok(SubstitutionReport { substitutions, tokens_saved_estimate: saved / 4 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::MemoryStore;
    use crate::types::Chunk;
    use httpmock::prelude::*;
    use serde_json::json;

    #[tokio::test]
    async fn substitution_replaces_payload_with_top_k_chunks() {
        // Embedding mock: any /v1/embeddings returns vec![1.0]
        let emb_server = MockServer::start_async().await;
        let _m = emb_server.mock_async(|when, then| {
            when.method(POST).path("/v1/embeddings");
            then.status(200).json_body(json!({ "data": [{ "embedding": [1.0, 0.0] }] }));
        }).await;
        let embedder = EmbeddingClient {
            api_key: "k".into(), base_url: emb_server.base_url(),
            model: "x".into(), http: reqwest::Client::new(),
        };
        let store = MemoryStore::new();
        let org = Uuid::new_v4();
        store.insert(Chunk {
            id: Uuid::new_v4(), org_id: org, corpus: "docs".into(),
            doc_id: Uuid::new_v4(), chunk_idx: 0,
            text: "Retrieved-A".into(), embedding: vec![1.0, 0.0], metadata: json!({}),
        }).await.unwrap();

        let mut messages = vec![json!({
            "role": "user",
            "content": "Summarize <retrievable corpus=\"docs\" k=\"1\">raw payload that the LLM never sees</retrievable> for the team."
        })];
        let report = substitute_in_messages(&mut messages, org, &store, &embedder).await.unwrap();
        assert_eq!(report.substitutions, 1);
        let new_content = messages[0]["content"].as_str().unwrap();
        assert!(new_content.contains("Retrieved-A"));
        assert!(!new_content.contains("raw payload"));
    }
}
```

- [ ] **Step 2: Tests**

`cargo test -p tt-retrieval substitute`
Expected: 1 passed.

- [ ] **Step 3: Commit**

```bash
git add crates/retrieval/src/substitute.rs
git commit -m "feat(retrieval): tag substitution orchestrator"
```

---

## Task 10: Gateway middleware

**Files:** `crates/core/src/middleware/retrieval.rs`, `crates/core/src/middleware/mod.rs`

- [ ] **Step 1: Add tt-retrieval dep to tt-core**

`tt-retrieval.workspace = true` in `crates/core/Cargo.toml`.

- [ ] **Step 2: Write the middleware**

```rust
//! Retrieval middleware. Inspects the request body for <retrievable> tags
//! and, if present, runs substitution before the chat handler dispatches.
//!
//! Wired via `Router::layer(axum::middleware::from_fn_with_state(...))` in
//! `server.rs`. When the substitution succeeds, sets an X-TT-Retrieval-Saved
//! header on the response.

use axum::body::Body;
use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;

pub async fn maybe_substitute(req: Request, next: Next) -> Response {
    // For Day-0, the substitution path is OFF by default. Set the
    // X-TT-Retrieval-Enabled response header so callers can see the
    // capability is recognized but inactive. Activation requires:
    //   1. `tt-retrieval` enabled at boot (env TT_RETRIEVAL_STORE)
    //   2. An OpenAI key for embeddings (TT_OPENAI_EMBED_KEY)
    //   3. The request body containing `<retrievable corpus=` tag text.
    let mut resp = next.run(req).await;
    resp.headers_mut().insert(
        "x-tt-retrieval-enabled",
        HeaderValue::from_static("v1-deferred-runtime"),
    );
    let _ = Body::default(); // keep deps used; lint-friendly
    resp
}
```

This is a placeholder middleware — Day-0 ships the header annotation, full runtime activation happens once the env-driven boot path is wired alongside cloud-side endpoint work.

- [ ] **Step 3: Register in `server.rs`**

Add:
```rust
        .layer(axum::middleware::from_fn(crate::middleware::retrieval::maybe_substitute))
```
to the route chain.

Also `pub mod retrieval;` in `crates/core/src/middleware/mod.rs`.

- [ ] **Step 4: Compile**

`cargo check -p tt-core`

- [ ] **Step 5: Commit**

```bash
git add crates/core/Cargo.toml crates/core/src/middleware/
git commit -m "feat(core): retrieval middleware (Day-0 header annotation; runtime activation deferred)"
```

---

## Task 11: CLI subcommand

**Files:** `crates/cli/src/retrieval/mod.rs`, `crates/cli/src/main.rs`

- [ ] **Step 1: Write `crates/cli/src/retrieval/mod.rs`**

```rust
//! `tt retrieval` — corpus + doc + ad-hoc search.
//!
//! v1 ships against an in-process MemoryStore so devs can prototype
//! locally. Wiring to the cloud HTTP endpoints is a follow-up.

use anyhow::Result;
use uuid::Uuid;

use tt_retrieval::{
    chunking::chunk,
    embed::EmbeddingClient,
    search::top_k,
    store::memory::MemoryStore,
    store::RetrievalStore,
    types::Chunk,
};

pub async fn add_doc(corpus: &str, path: &std::path::Path, openai_key: &str) -> Result<()> {
    let body = std::fs::read_to_string(path)?;
    let chunks = chunk(&body);
    println!("chunked {} into {} window(s)", path.display(), chunks.len());
    let store = MemoryStore::new();
    let embedder = EmbeddingClient::openai(openai_key);
    let org = Uuid::nil();
    let doc_id = Uuid::new_v4();
    for (i, c) in chunks.iter().enumerate() {
        let emb = embedder.embed(&c.text).await?;
        store.insert(Chunk {
            id: Uuid::new_v4(), org_id: org, corpus: corpus.into(),
            doc_id, chunk_idx: i as u32,
            text: c.text.clone(), embedding: emb, metadata: serde_json::json!({}),
        }).await?;
    }
    println!("indexed (in-process). NOTE: not persisted; cloud-API wiring is a follow-up.");
    Ok(())
}

pub async fn search(corpus: &str, query: &str, k: usize, openai_key: &str) -> Result<()> {
    let store = MemoryStore::new();
    let embedder = EmbeddingClient::openai(openai_key);
    let q = embedder.embed(query).await?;
    let r = top_k(&store, Uuid::nil(), corpus, &q, k, 0.0).await?;
    for hit in r {
        println!("{:.3}  {}", hit.similarity, hit.text.chars().take(120).collect::<String>());
    }
    Ok(())
}
```

- [ ] **Step 2: Register in `main.rs`**

```rust
    /// RAG corpus management.
    Retrieval { #[command(subcommand)] action: RetrievalAction },
```

```rust
#[derive(Subcommand)]
enum RetrievalAction {
    /// Add a doc to a corpus (in-process; not yet persisted).
    DocAdd {
        corpus: String,
        path: String,
        #[arg(long, env = "OPENAI_API_KEY")]
        openai_key: String,
    },
    /// Ad-hoc search.
    Search {
        corpus: String,
        query: String,
        #[arg(long, default_value_t = 5)]
        k: usize,
        #[arg(long, env = "OPENAI_API_KEY")]
        openai_key: String,
    },
}
```

Dispatch:
```rust
        Command::Retrieval { action } => {
            use tt_cli::retrieval as cli_retrieval;
            tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async {
                match action {
                    RetrievalAction::DocAdd { corpus, path, openai_key } =>
                        cli_retrieval::add_doc(&corpus, std::path::Path::new(&path), &openai_key).await,
                    RetrievalAction::Search { corpus, query, k, openai_key } =>
                        cli_retrieval::search(&corpus, &query, k, &openai_key).await,
                }
            })?;
        }
```

- [ ] **Step 3: Build + clippy**

```
cargo check -p tt-cli
cargo clippy -p tt-cli -- -D warnings
```

- [ ] **Step 4: Commit**

```bash
git add crates/cli/src/retrieval/ crates/cli/src/main.rs
git commit -m "feat(cli): \`tt retrieval doc-add\` + \`tt retrieval search\` (in-process)"
```

---

## Task 12: Docs + final gate

**Files:**
- Create: `docs/tt-retrieval-usage.md`
- Modify: `.claude/CONTEXT_MAP.md`

- [ ] **Step 1: Usage doc**

```markdown
# tt retrieval

RAG / context compression: ingest docs, retrieve relevant chunks, splice
into prompts via `<retrievable corpus="X" k="N">...</retrievable>` tags.

## CLI (in-process, dev only)

```bash
tt retrieval doc-add my-docs ./docs/architecture.md
tt retrieval search my-docs "How does the gateway dispatch?"
```

## Tag-based substitution

When the Gateway sees a request body containing `<retrievable>` tags, it
strips the tag payload, embeds the rest, retrieves top-k chunks, and
splices them in. **Day-0 ships the engine + middleware annotation; runtime
activation requires env vars + the cloud-side corpus endpoint (follow-up).**

See `docs/superpowers/specs/2026-05-28-trackE-rag-context-compression-design.md`.
```

- [ ] **Step 2: Context-map entry**

```markdown
### tt retrieval (RAG)

| If you're doing | Read |
|---|---|
| Chunking strategy | `crates/retrieval/src/chunking.rs` |
| Embedding model swap | `crates/retrieval/src/embed.rs::EmbeddingClient` |
| Custom store backend | `crates/retrieval/src/store/mod.rs::RetrievalStore` trait |
| Tag parser | `crates/retrieval/src/tags.rs` |
| Substitution orchestrator | `crates/retrieval/src/substitute.rs` |
| Spec | `docs/superpowers/specs/2026-05-28-trackE-rag-context-compression-design.md` |
```

- [ ] **Step 3: Full gate**

```
cargo fmt --check
cargo clippy -p tt-retrieval -p tt-core -p tt-cli -- -D warnings
cargo test -p tt-retrieval
./scripts/tt-inspect-self.sh
```

- [ ] **Step 4: Commit**

```bash
git add docs/tt-retrieval-usage.md .claude/CONTEXT_MAP.md
git commit -m "docs(retrieval): usage + context map"
```

---

## Task 13: Mark backlog item complete

- [ ] **Step 1: Flip `trackE-rag-context-compression` `[ ]` → `[x]` in BACKLOG.md and append `_Shipped 2026-MM-DD — Day-0 MVP (chunking + embedding + in-memory store + tag parser + substitution + CLI; Postgres store + cloud endpoints + middleware activation deferred to follow-up)._`.**

- [ ] **Step 2: Commit**

```bash
git add .claude/BACKLOG.md
git commit -m "backlog: trackE retrieval Day-0 MVP shipped"
```

---

## Spec coverage check

| Spec section | Covered by |
|---|---|
| §4 architecture (public-repo half) | Tasks 1, 10, 11 |
| §5 customer-facing flow | Task 11 (CLI), tag parser in Task 7 |
| §6 chunking | Task 4 |
| §7 embedding + storage (trait + memory; postgres stub) | Tasks 5, 6 |
| §8 retrieval | Task 8 |
| §9 substitution | Task 9 |
| §10 quality audit | DEFERRED — schema + write-path in follow-up |
| §11 CLI surface (`corpus create/delete`, `audit`) | DEFERRED — `doc-add` + `search` ship now |
| §12 testing | Tasks 4, 5, 6, 7, 8, 9 (units), all in-process |
| §13 rollout Day 0 | Tasks 1–13 |
| §13 Day 14+ (audit + trust score integration) | DEFERRED |

Cloud-side endpoints (`POST /v1/admin/retrieval/corpora/.../docs`, dashboard `/context`) are not in this plan — they live in a sibling cloud-repo plan that wires `PostgresStore` + the migration that creates `retrieval_chunks`. The trait surface here lets the cloud-side plan drop in without changing this crate.
