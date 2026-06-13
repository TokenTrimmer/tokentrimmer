# tt retrieval

RAG / context compression: ingest docs, retrieve relevant chunks, splice
into prompts via `<retrievable corpus="X" k="N">...</retrievable>` tags.

## CLI (EXPERIMENTAL — in-process, dev only)

> **EXPERIMENTAL.** `tt retrieval doc-add` and `tt retrieval search` run
> against an **in-process store**: the corpus lives only inside the running
> process and is **discarded when the command exits**. A `doc-add` therefore
> does **not** persist, and a later `search` (a separate process) always
> starts from an empty store — so the two cannot see each other's data. These
> commands exist to prototype the chunking/embedding path locally; durable
> corpora require the Postgres-backed store + cloud endpoints (follow-up).
> Each invocation prints a one-line `note:` to stderr restating this.

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
