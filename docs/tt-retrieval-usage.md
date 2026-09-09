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

For configured `/v1/chat/completions` and `/v1/messages` requests (buffered or
streaming), middleware detects `<retrievable>` tags but performs no embedding,
search, or prompt-audit I/O. It passes a trusted tenant-bound intent to chat
preparation. **Routing now selects on the original caller input, before
retrieval.** The selected route is retained even if substitution removes a
keyword or changes the token count that caused it to match.

After routing succeeds, retrieval strips tag payloads to form an embedding
query, retrieves top-k chunks, and splices them in. If no outside text remains,
the entire first tag is the fallback query. A selected `redact` policy sanitizes
the actual query in either case, for every originating message role. Retrieved
context is redacted before primary dispatch; newly substituted assistant
content is guarded too, without rewriting untouched assistant history.

A routing-store outage with no fresh snapshot returns 503 before retrieval I/O.
Unknown/incompatible forced routes return 400 before embedding. Requests that
never reach the policy-aware stage report
`X-TokenTrimmer-Retrieval-Skipped: policy-not-reached`, not an active retrieval.
A later embedding/search failure leaves the pre-substitution messages intact
for the normal privacy stage; no partial replacement is forwarded.

Token overhead estimates use the actual query sent to the embedder, including
whole-tag fallbacks. They are not invoice amounts. Encrypted prompt auditing,
when separately configured, is skipped on redact routes; other routes retain a
canonical pre-substitution body, not the byte-exact original ingress envelope.

**Runtime activation requires configured retrieval/embedding state and an
available corpus.** CLI ingestion is still ephemeral; durable corpus management
remains separately scoped.

See `docs/superpowers/specs/2026-05-28-trackE-rag-context-compression-design.md`.
