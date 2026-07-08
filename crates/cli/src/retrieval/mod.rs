//! `tt retrieval` — corpus + doc + ad-hoc search.
//!
//! Two store backends:
//! - **in-process** (the default when `--dsn` is unset): a `MemoryStore` whose
//!   corpus lives only in the current process and is discarded on exit. A
//!   `doc-add` therefore never survives, and a later `search` (a separate
//!   process) always starts from an empty store — these commands CANNOT see
//!   each other's data. EXPERIMENTAL, demo-only.
//! - **Postgres + pgvector** (when `--dsn` is set): a `PostgresStore` whose
//!   corpus persists (migration 0005, `crates/retrieval/src/store/postgres.rs`).
//!   A `doc-add --dsn` indexes into the shared store + a later
//!   `search --dsn` reads it — the two commands CAN see each other's data when
//!   pointed at the same DSN.
//!
//! Resolves PROJECT_REVIEW_2026-07-01 §4.6: the in-process store confusion.
//! The durable store was already implemented (`PostgresStore`); this wires it
//! to the CLI behind `--dsn`, preserving the demo path + the stderr notice.

use anyhow::{Context, Result};
use uuid::Uuid;

use tt_retrieval::{
    chunking::chunk,
    embed::EmbeddingClient,
    search::top_k,
    store::{memory::MemoryStore, postgres::PostgresStore, RetrievalStore},
    types::Chunk,
};

/// One-line stderr notice, shared by every `tt retrieval` subcommand on the
/// in-process path. The durable `--dsn` path emits a confirming notice instead.
fn experimental_notice() {
    eprintln!(
        "note: `tt retrieval` without --dsn uses the in-process store — the \
         corpus is NOT persisted; it is discarded when this command exits. Pass \
         --dsn <postgres-url> to persist to the pgvector store."
    );
}

fn durable_notice() {
    eprintln!(
        "note: `tt retrieval --dsn` uses the durable Postgres + pgvector store \
         (migration 0005). Chunks persist across invocations."
    );
}

/// Construct the store for a command. `None` (the default) → in-process
/// `MemoryStore` (EXPERIMENTAL, demo-only); `Some(dsn)` → a `PostgresStore`
/// (durable). Returns `Ok((store, durable))` so the caller can log the right
/// completion line. Errors propagate as a CLI failure (a bad DSN / unreachable
/// DB should fail loudly, not silently degrade to in-process).
async fn build_store(dsn: Option<String>) -> Result<(Box<dyn RetrievalStore>, bool)> {
    match dsn {
        None => {
            experimental_notice();
            Ok((Box::new(MemoryStore::new()), false))
        }
        Some(url) => {
            durable_notice();
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(2)
                .connect(&url)
                .await
                .with_context(|| format!("connect to retrieval Postgres {url}"))?;
            Ok((Box::new(PostgresStore::new(pool)), true))
        }
    }
}

pub async fn add_doc(
    corpus: &str,
    path: &std::path::Path,
    openai_key: &str,
    dsn: Option<String>,
) -> Result<()> {
    let body = std::fs::read_to_string(path)?;
    let chunks = chunk(&body);
    println!("chunked {} into {} window(s)", path.display(), chunks.len());
    let (store, durable) = build_store(dsn).await?;
    let embedder = EmbeddingClient::openai(openai_key);
    let org = Uuid::nil();
    let doc_id = Uuid::new_v4();
    for (i, c) in chunks.iter().enumerate() {
        let emb = embedder.embed(&c.text).await?;
        store
            .insert(Chunk {
                id: Uuid::new_v4(),
                org_id: org,
                corpus: corpus.into(),
                doc_id,
                chunk_idx: i as u32,
                text: c.text.clone(),
                embedding: emb,
                embedding_model: embedder.model.clone(),
                metadata: serde_json::json!({}),
            })
            .await?;
    }
    println!(
        "indexed. {}",
        if durable {
            "(durable pgvector; visible to a later `tt retrieval search --dsn`)."
        } else {
            "(in-process; NOT persisted — EXPERIMENTAL demo only)."
        }
    );
    Ok(())
}

pub async fn search(
    corpus: &str,
    query: &str,
    k: usize,
    openai_key: &str,
    dsn: Option<String>,
) -> Result<()> {
    let (store, _) = build_store(dsn).await?;
    let embedder = EmbeddingClient::openai(openai_key);
    let q = embedder.embed(query).await?;
    let r = top_k(&*store, Uuid::nil(), corpus, &q, k, 0.0, &embedder.model).await?;
    for hit in r {
        println!(
            "{:.3}  {}",
            hit.similarity,
            hit.text.chars().take(120).collect::<String>()
        );
    }
    Ok(())
}
