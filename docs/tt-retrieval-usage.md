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
