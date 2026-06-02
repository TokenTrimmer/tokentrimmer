# Track E — RAG / Context Compression (`crates/retrieval`)

**Status:** Draft 1
**Track:** E of six-track expansion
**Date:** 2026-05-28
**Depends on:** existing L2 cache pipeline (embeddings), existing Gateway middleware
**Consumed by:** Gateway (auto-context-compression middleware), Track A (MCP `retrieve_context` tool)

---

## 1. Problem

A customer's typical LLM call ships 80% repeated context (file contents, conversation history, doc snippets). They pay tokens for this every call. Existing levers:
- **L1 cache** dedupes exact-match repeat calls (response cached).
- **L2 semantic cache** dedupes near-match repeat calls (response cached).

Neither helps when the prompt itself contains stale, retrievable, or compressible context. Track E adds a third lever: **retrieve-on-demand context compression**.

The user opts in by tagging a section of their prompt as `<retrievable corpus="my-docs">…</retrievable>`. The Gateway strips that section, embeds the surrounding prompt, retrieves the top-k semantically relevant chunks from `my-docs`, and re-injects only those chunks. Token savings on long prompts: 50-90%.

## 2. Goals

1. Opt-in via explicit tagging. We do not retrieval-substitute prompts without consent — quality risk is real.
2. Multi-corpus per org. A customer can have `my-docs`, `code-snippets`, `slack-archive` corpora separately.
3. Ingestion is async. Customer uploads a doc → embedding pipeline runs → corpus updated. No live indexing in the request path.
4. Retrieval latency budget: < 50ms p50 (HNSW lookup over org+corpus partition).
5. Honest measurement: every retrieval-substituted request also stores the original full prompt (encrypted) for 30 days so the customer can audit quality offline.

## 3. Non-goals

- Auto-detecting retrievable sections of user prompts. v1 is explicit-tag only.
- Replacing the L2 semantic response cache. They live side-by-side: L2 caches *responses*, E retrieves *context*. (Yes, both can hit on the same request.)
- Custom embedding models. v1 uses OpenAI `text-embedding-3-small` (matches L2 cache, ADR-008). BYO model is v2.
- Web crawling. Customers upload files via dashboard or `tt retrieval upload`.

## 4. Architecture

```
crates/retrieval/                        [NEW crate]
└── src/
    ├── lib.rs                           [public API]
    ├── ingest.rs                        [chunk a doc into 512-token windows w/ 64-token overlap]
    ├── embed.rs                         [reuses tt-cache embedding client]
    ├── store.rs                         [Postgres + pgvector ops; per-org partition by org_id]
    ├── retrieve.rs                      [given query → top-k chunks via HNSW + cosine]
    ├── substitute.rs                    [parse <retrievable> tags, replace in prompt]
    ├── types.rs                         [Corpus, Chunk, Document, RetrievalResult]
    └── error.rs

crates/core/src/middleware/retrieval.rs  [new — wires substitute() into request pipeline]
crates/cli/src/main.rs                   [modified — `tt retrieval` subcommand]

cloud/crates/api/src/retrieval/          [new — ingestion REST endpoints]
├── ingest.rs                            [POST /v1/admin/retrieval/corpora/:name/docs]
├── corpora.rs                           [CRUD on corpora list]
└── search.rs                            [GET /v1/admin/retrieval/search?corpus=&q=]

cloud/apps/dashboard/src/pages/
└── context/
    ├── index.astro                      [list corpora, recent retrievals, savings card]
    └── [corpus].astro                   [docs in this corpus, ingestion status, search test box]
```

## 5. Customer-facing flow

### 5.1 Create a corpus (one-time)

```bash
tt retrieval corpus create my-docs --org-id auto
```

### 5.2 Ingest a doc

```bash
tt retrieval doc add my-docs ./docs/architecture.md
```

CLI uploads the file to `POST /v1/admin/retrieval/corpora/my-docs/docs`. Cloud-side: chunks, embeds, stores. Returns `{"doc_id": "...", "chunks": 24, "status": "indexed"}`.

### 5.3 Use in a request

```python
client.chat.completions.create(
    model="claude-sonnet-4-6",
    messages=[
        {"role": "user", "content": """
Summarize this architecture for a junior engineer.

<retrievable corpus="my-docs" k="5">
<!-- This section will be replaced by the top-5 semantically relevant chunks from my-docs. -->
What follows here is just context for the embed; the LLM never sees it.
This text can be anything that helps the retriever find relevant material.
</retrievable>

Focus on the gateway / cache / plan layers.
"""}
    ],
)
```

The Gateway: strips `<retrievable>` content from the prompt sent to the provider, embeds the rest of the prompt, retrieves top-5 chunks from `my-docs`, re-injects them inline where the tag was, sends the resulting prompt to the provider. Response is returned unchanged.

Response header: `X-TT-Retrieval-Tokens-Saved: 4872` shows the difference.

## 6. Chunking

- Fixed window: 512 tokens (tiktoken `cl100k_base`).
- Overlap: 64 tokens (smooths topic boundaries).
- Markdown-aware: if a chunk would split a code block or heading, the boundary is shifted up to ±128 tokens to preserve the structure.

## 7. Embedding + storage

- Model: OpenAI `text-embedding-3-small` (1536-dim, matches L2 cache pgvector schema).
- Storage: `cache_entries` table extended OR new `retrieval_chunks` table (decision: new table — different access pattern + per-org partition lets us avoid touching cache schema).
  ```sql
  CREATE TABLE retrieval_chunks (
    id BIGSERIAL PRIMARY KEY,
    org_id UUID NOT NULL,
    corpus TEXT NOT NULL,
    doc_id UUID NOT NULL,
    chunk_idx INT NOT NULL,
    chunk_text TEXT NOT NULL,
    embedding vector(1536) NOT NULL,
    metadata JSONB,
    created_at TIMESTAMPTZ DEFAULT now()
  );
  CREATE INDEX retrieval_chunks_hnsw ON retrieval_chunks USING hnsw (embedding vector_cosine_ops);
  CREATE INDEX retrieval_chunks_org_corpus ON retrieval_chunks (org_id, corpus);
  ```
- Per-org partition via `org_id` predicate on every query.

## 8. Retrieval

Given a query text:
1. Embed the query.
2. `SELECT chunk_text, 1 - (embedding <=> $1) AS sim FROM retrieval_chunks WHERE org_id = $org AND corpus = $corpus ORDER BY embedding <=> $1 LIMIT k`
3. Optionally filter by `sim >= threshold` (default 0.5).
4. Return chunks ordered by similarity desc.

## 9. Substitution

Parse the user's last message for `<retrievable corpus="X" k="N">…</retrievable>` tags. For each tag:
1. Embed the rest of the message (everything outside the tag).
2. Retrieve top-N from corpus X.
3. Replace the tag's full payload with the retrieved chunks (joined by `\n\n---\n\n`).
4. Send modified message to the provider.
5. Log token delta to `request_logs.retrieval_tokens_saved` column (migration adds the column).

## 10. Quality audit (the honest part)

For every retrieval-substituted request:
- Store the original full prompt (encrypted with `TT_MASTER_KEY`) in `retrieval_audit_log` table, retention 30 days.
- Dashboard `/context` page lets customer re-run the original prompt on demand (opt-in, costs them tokens) to compare against the retrieved-substituted answer.
- Trust score (existing) extends to "retrieval substitution" — if customers consistently downgrade the substituted answer, that corpus' quality score drops.

## 11. CLI surface

```
tt retrieval [SUBCOMMAND]

  corpus list
  corpus create <NAME> [--description <D>]
  corpus delete <NAME>

  doc list <CORPUS>
  doc add <CORPUS> <PATH> [--metadata <JSON>]
  doc remove <CORPUS> <DOC_ID>

  search <CORPUS> <QUERY> [-k <N>]    # ad-hoc search test
  audit <REQUEST_ID>                  # show original vs substituted prompt
```

## 12. Testing

| Layer | Tests |
|---|---|
| Unit (ingest) | Fixture markdown doc → expected chunk count + chunk text. |
| Unit (embed) | Mock OpenAI embeddings → vector inserted. |
| Unit (retrieve) | Given fixture chunks + known cosine values → expected ordered top-k. |
| Unit (substitute) | Parse `<retrievable>` correctly; handle nested + malformed gracefully. |
| Integration | End-to-end: ingest fixture corpus, send request with `<retrievable>` tag, assert provider received substituted prompt with retrieved chunks. |
| Quality audit | Original prompt stored + recoverable; trust score updates on customer downgrade. |
| Per-org isolation | Org A cannot retrieve from Org B's corpora. Assert by querying with wrong org_id. |

## 13. Rollout

1. Day 0: ship corpus CRUD + ingest + retrieve + substitute. Single dashboard page.
2. Day 14: add quality audit + trust-score integration.
3. Day 30: add MCP tool `retrieve_context` (Track A) for assistants to query directly.
4. Day 60: BYO embedding model (BGE via Candle).

## 14. Open questions

- Should retrieval substitution happen pre-cache or post-cache? Decision needed: probably pre-cache so cached responses are keyed off the substituted prompt (more cache hits).
- Should we expose corpus-mix retrieval (top-k from multiple corpora)? Defer.
- Encrypted retrieval audit log retention — 30d default, configurable per org? Defer.

## 15. References

- ADR-008: OpenAI embeddings for L2 cache
- Existing pgvector schema: `cloud/crates/api/migrations/0XXX_cache_entries.sql`
- Existing L2 cache: `cloud/crates/api/src/cache/l2.rs`
- Inspect rule `conversation-unbounded-history` (related token-waste check)
