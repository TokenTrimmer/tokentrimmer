# Backlog

Single source of truth for actionable work. Entries are checkboxes; flip to `[x]` when done. Sync to GitHub Issues with `autopilot` label via `./scripts/backlog.sh sync`.

**Format**: `- [ ] [PRIORITY] [task-id] subagent: brief description (§review-ref) (est: $X.XX)`

- `PRIORITY` ∈ {P0 (blocker), P1 (next), P2 (soon), P3 (whenever)}
- `task-id` is short kebab-case, used in branch names: `autopilot/<task-id>`
- `subagent` ∈ `rust-crate-builder`, `provider-adapter-author`, `inspect-rule-author`, `astro-page-builder`, `plan-replay-validator`
- `§ref` points to the section in `PROJECT_REVIEW.md` (2026-05-30 senior review) the item came from.

> **History:** all completed pre-2026-05-30 items live in `.claude/BACKLOG_ARCHIVE.md` (frozen snapshot). Cloud-side items live in `../cloud/.claude/BACKLOG.md`.
>
> **Note on prior review:** the 2026-05-29 follow-ups shipped, but the 2026-05-30 audit found several were *incomplete*. Items below tagged "(extends …)" supersede a previously-closed item rather than duplicating it.

---

## Review follow-ups (2026-05-30) — public repo (gateway, cache, routing, plan, inspect, retrieval, auth, preview, MCP, CLI, providers)

### P0 — critical / highest-leverage

- [x] [P0] [rv-l2-cache-correctness] rust-crate-builder: Make the L2 semantic cache safe — add `AND model = $N` to the lookup (l2.rs:356-367) + in-memory scan (l2.rs:208-216); embed full context (system + history) not just the last user message (chat.rs:333-336,567-580); add an `embedding_model`/version column to cache_entries + lookup filter so embedder swaps partition cleanly. (§2.1/2.3/2.5) (est: $1.50)
- [x] [P0] [rv-cache-nondeterministic-guard] rust-crate-builder: Gate caching of non-deterministic requests (skip when temperature>0 / top_p<1 / n>1 / seed set / response has tool_calls) and wire a `tt_extras` cache-control (bypass/refresh/read-only/ttl) before the L1/L2 branches (chat.rs:295-330; key.rs; messages.rs:42-45). (§2.2/2.7) (est: $1.00)
- [x] [P0] [rv-plan-apply-writes-routes] plan-replay-validator: Extend `apply_plan` + the `PlanStore` trait to persist `proposed_routes` and emit them for the routing table; persist `proposed_routes` at plan-create time (apply.rs:162-197; types.rs:217). Pairs with cloud `rv-plan-apply-route` (dashboard+handler writeback). (§1.1/1.9) (est: $1.20)

### P1 — high

- [x] [P1] [rv-streaming-cost-accuracy] rust-crate-builder: Fix streaming telemetry — tokenize output via `tt_tokenize` instead of byte counts (sse.rs:84-98,112-116); apply the OpenRouter `fee_multiplier` on the streaming cost+baseline path (sse.rs:455-476); use `tt_tokenize` for the streaming input estimate (chat.rs:201-240). (extends `fee-multiplier-apply`, which fixed non-stream only) (§2.12/2.13/2.15) (est: $0.80)
- [x] [P1] [rv-routing-capability-floor] rust-crate-builder: Intersect a request's required capabilities (vision/tools/json/reasoning) + estimated input tokens against candidate `ModelInfo` before any route rewrite or failover; skip unqualified candidates and log `route_skipped_capability` instead of silently degrading (routing/lib.rs:135-161; chat.rs:983-987; failover.rs:107-128). (§2.11) (est: $0.80)
- [x] [P1] [rv-ssrf-url-guard] rust-crate-builder: Shared `tt_shared::url_guard` — https-only + reject RFC1918/loopback/link-local/metadata IPs; use it when building every provider client; add an `extra_headers` denylist (Authorization/Host) in the compat/openai/anthropic adapters (compat.rs:160-175; credentials.rs). Cloud `rv-ssrf-write-validate` adds write-time validation. (§5.2) (est: $0.80)
- [x] [P1] [rv-gateway-keycache-argon2] rust-crate-builder: Add the documented short-TTL verified-key cache (blake3/sha256 of token → ApiKeyContext, ~60s) in front of per-request argon2 in the auth middleware; fix the stale `lib.rs:4` claim (middleware/auth.rs:68; auth/lib.rs:4; keys.rs:273). Restores the sub-30ms target; removes a CPU-DoS lever. (§5.4) (est: $0.60)
- [x] [P1] [rv-rag-similarity-floor] rust-crate-builder: Route retrieval substitution through `top_k(min_similarity)` with a sane default floor + per-tag override; leave the payload unsubstituted when nothing clears it (substitute.rs:54-66; search.rs:9-22). (§2.4) (est: $0.40)
- [x] [P1] [rv-retrieval-orgid-failclosed] rust-crate-builder: Fail closed (skip substitution + warn header) when `ApiKeyContext` is absent instead of falling back to the shared `Uuid::nil()` retrieval namespace (middleware/retrieval.rs:208-218). (extends `retrieval-orgid-isolation`) (§5.7) (est: $0.30)
- [x] [P1] [rv-inspect-walker-hidden-dirs] inspect-rule-author: Allowlist `.cursor`/`.github`/`.claude` before the leading-dot prune so `config-agents-md-contains-secrets` can actually scan `.cursor/rules/*.md`; add a `.cursor/rules/` fixture (walk.rs:63; config_agents_md_contains_secrets.rs:34). (§4.6) (est: $0.30)
- [x] [P1] [rv-key-prefix-entropy] rust-crate-builder: Widen the key display prefix to ~12 hex chars (≥48 bits) or retry issuance on unique-violation, and surface a retryable error instead of opaque `Store(...)` (keys.rs:31,268-269,291). (§4.14) (est: $0.30)
- [x] [P1] [rv-truncated-column] rust-crate-builder: Add a `truncated BOOLEAN NOT NULL DEFAULT false` migration + INSERT bind, and exclude/down-weight truncated rows in realized-savings sums; add a bind-count test (request_logs.rs:52-53,134-167; migrations/0001). (§2.14) (est: $0.30)
- [ ] [P1] [rv-inspect-feeds-plan] rust-crate-builder: Converter from `preview::RouteSuggestion` / inspect findings → `Vec<ProposedRoute>` + a `tt inspect --suggest-plan` (and pre-filled PlanInput) so Inspect actually feeds Plan (preview/route_suggestions.rs:41-53; cli/main.rs:924-947). (§1.4) (est: $0.80)

### P2 — soon

- [ ] [P2] [rv-cache-stampede-singleflight] rust-crate-builder: Per-key single-flight (keyed async mutex / Notify map, or short Redis SETNX) so concurrent identical misses share one upstream call (chat.rs:303-382). (§2.6) (est: $0.80)
- [ ] [P2] [rv-per-tier-ttl] rust-crate-builder: Plumb the caller's tier into L1/L2 TTL selection (24h/7d/30d) + per-route TTL override (state.rs:22-24; chat.rs:47-50,720). (§2.8) (est: $0.60)
- [ ] [P2] [rv-sse-crlf-parsing] provider-adapter-author: SSE parsers accept `\r\n\r\n` event separators + optional space after `data:` across compat/anthropic/gemini `stream.rs`; add CRLF + no-space regression fixtures (stream.rs:241,269-271 et al). (§4.1) (est: $0.40)
- [ ] [P2] [rv-retry-jitter-fanout] rust-crate-builder: Add full jitter to retry backoff, bound the failover×retry fan-out (smaller per-candidate attempts when a chain exists), and feed retry-exhaustion into the circuit breaker (failover.rs:107-129; retry.rs:43-49). (§4.2) (est: $0.40)
- [ ] [P2] [rv-fallback-eligible-5xx] rust-crate-builder: Guard `is_fallback_eligible` on `status >= 500` (keep 408/429) so deterministic 4xx short-circuits failover instead of fanning out (shared/error.rs:52-59). (§4.3, quick win) (est: $0.10)
- [ ] [P2] [rv-gemini-key-header] provider-adapter-author: Send the Gemini key in the `x-goog-api-key` header instead of the URL query string (gemini/stream.rs:66,71-72). (§5.2 quick win) (est: $0.20)
- [ ] [P2] [rv-replay-priority-tiebreak] plan-replay-validator: Deterministic tiebreak (route_id/name) on the equal-priority route sort so identical configs project identically (replay.rs:35-36; routing.rs:12-16). (§4.12) (est: $0.20)
- [ ] [P2] [rv-l2-poisoning-per-threshold] plan-replay-validator: Report L2 poisoning candidates per-threshold (or dedup across passes) instead of summing across the whole sweep (l2_projection.rs:104-108). (§4.11) (est: $0.20)
- [ ] [P2] [rv-batch-api] rust-crate-builder: Batch-API dispatch path (OpenAI `/v1/batches`, Anthropic Message Batches) keyed off a route action / `tag=background`, polled async, reporting the ~50% discount as realized savings (routing/lib.rs:62-64). (§2.16) (est: $1.50)
- [ ] [P2] [rv-mcp-find-route-honest] rust-crate-builder: Back `find_route_for` with real per-task-class telemetry, or downgrade its tool description from "historical / HIGH quality confidence" to "heuristic default by keyword" (mcp/tools/find_route_for.rs:23,35-48). (§1.7) (est: $0.40)
- [ ] [P2] [rv-plan-apply-cli-honest] rust-crate-builder: Make `tt plan --apply` exit non-zero with a clear message until the apply path is wired, instead of silently running projection-only (cli/main.rs:67-70,934-939). (§1.8 quick win) (est: $0.10)
- [ ] [P2] [rv-rag-savings-embedding-cost] rust-crate-builder: Account embedding-call cost in RAG net-savings, clamp/skip negative substitution deltas, batch per-message embeddings, and use the tokenizer not byte/4 (substitute.rs:63,71-75; embed.rs:27). (§2.17) (est: $0.40)
- [ ] [P2] [rv-inspect-agent-loop-scope] inspect-rule-author: Scope `agent-no-termination-condition`'s termination search to the AST loop body, tighten the broad `budget`/`timeout` tokens, and add a multi-function (one bounded + one unbounded) fixture (agent_no_termination_condition.rs:60-61,91,110). (§4.7) (est: $0.40)

### P3 — whenever

- [ ] [P3] [rv-anthropic-cache-write-rate] rust-crate-builder: Add a `cache_write_per_million` rate to ModelPricing/pricing.toml and price `cache_creation_input_tokens` at the ~1.25× write premium (anthropic/stream.rs:551-558; chat.rs:748-758). (§2.18) (est: $0.40)
- [ ] [P3] [rv-pricing-catalog-staleness] rust-crate-builder: Pricing-catalog refresh script/job + a test that warns when newest `effective_at` ages out + a telemetry counter for requests priced at $0 from a missing catalog entry (pricing.toml; pricing.rs:2; chat.rs:744-746). (§2.19) (est: $0.40)
- [ ] [P3] [rv-l2-streaming-cache-write] rust-crate-builder: Accumulate the streamed response (SSE aggregator already tracks usage) and write it to L1/L2 on clean completion when cache-eligible, so streaming traffic builds + benefits from the cache (chat.rs:168-294; sse.rs:55-108). (§2.10) (est: $0.60)
- [ ] [P3] [rv-cache-key-canonicalization] rust-crate-builder: Model-alias canonicalization (dated ids/provider aliases → one key) + optional message whitespace normalization + short-TTL negative caching of deterministic 4xx (key.rs:46-86). (§2.20) (est: $0.60)
- [ ] [P3] [rv-l2-nan-finite-filter] rust-crate-builder: Filter non-finite similarities before `max_by` and validate embeddings are all-finite at insert (l2.rs:208-218). (§4.15) (est: $0.10)
- [ ] [P3] [rv-body-limit-embeddings-stub] rust-crate-builder: Set an explicit `DefaultBodyLimit` sized for the largest context window; return 501 (not a misleading 404) from the `/v1/embeddings` stub with a clear message (server.rs:36-73; embeddings.rs:11-16). (§4.15) (est: $0.30)
- [ ] [P3] [rv-routeaction-shared-type] rust-crate-builder: Define the route condition/action shape once in tt-shared so `tt_routing::RouteAction` and `tt_plan_core::RouteAction` round-trip losslessly (routing/lib.rs:68-81; plan-core/types.rs:130-141). (§1.10) (est: $0.50)
- [ ] [P3] [rv-revoke-key-atomic] rust-crate-builder: Wrap key mutation + audit append in one DB transaction (or emit a metric/alert on the audit-after-mutation failure path) so the chain is a complete record (auth/keys.rs:369-389). (§5.10) (est: $0.40)
- [ ] [P3] [rv-inspect-parallel-scan] rust-crate-builder: Parallelize the per-file inspect scan with rayon for large repos + store source length alongside the 64-bit AST-cache hash to remove the collision class (engine.rs:60-84; parse.rs:73-101). (§4.15) (est: $0.40)
- [ ] [P3] [rv-preview-provider-disambig] rust-crate-builder: Let preview pricing lookup honor the intended provider for cross-listed models instead of first-hit probe order (preview/pricing.rs:33-59). (§4-preview) (est: $0.20)
- [ ] [P3] [rv-env-credential-failclosed] rust-crate-builder: Don't chain the Env credential store as a multi-tenant fallback; boot-assert/metric when `EnvProviderCredentialStore` is active alongside >1 org (credentials.rs:205-265; chat.rs:822-834). (§5.10) (est: $0.30)

---

## Carried over — still open from prior backlog (full notes in `.claude/BACKLOG_ARCHIVE.md`)

- [ ] [P0] [w11-e2e-smoke-test] rust-crate-builder: signup→magic-link→Stripe $1→issue key→curl Gateway→dashboard within 30s. 🟡 authored + CI-wired. [BLOCKED — CI/staging only, not runnable in-sandbox] (est: $0.80)
- [ ] [P2] [w20-dashboard-perf-gate] rust-crate-builder: Playwright p75<1.5s on dashboard pages. 🟡 authored + CI-wired, CI/staging step. (est: $0.30)
- [ ] [P0] [w23-free-tier-live] rust-crate-builder: Enable Free tier in prod. [BLOCKED — external accounts] Remaining code bit (per-org tier→BudgetLimits) is now tracked as cloud `rv-tier-limits-enforcement`. (est: $0.80)
- [ ] [P0] [w23-alpha-reconciliation-gate] rust-crate-builder: 14 consecutive days of drift ≤2%. [BLOCKED — needs live alpha traffic] (est: $0.30)
- [ ] [P0] [w24-alpha-inspect-fp-gate] rust-crate-builder: Inspect FP <5% on real alpha org repos. [BLOCKED — needs alpha traffic] (est: $0.30)
- [ ] [P1] [post-team-pr-bot] rust-crate-builder: PR-bot GitHub App (free to register), Inspect findings as check-runs, ≤10 repos. Post-beta. (est: $0.80)
- [ ] [P1] [post-team-sso] rust-crate-builder: Google OAuth SSO (free; GitHub half shipped). Post-beta. See review §7.3 for the SAML/self-serve angle. (est: $0.60)
- [ ] [P1] [post-scale-s3-object-lock] rust-crate-builder: WORM audit storage via Cloudflare R2 Bucket Locks (B2 only if certified COMPLIANCE-mode demanded). Post-beta. (est: $1.00)
- [ ] [P1] [post-scale-slo-proof] rust-crate-builder: SLO dashboard + signed monthly PDF. [BLOCKED — post-beta] (est: $0.60)
- [ ] [P2] [post-enterprise-workos-saml] rust-crate-builder: Defer the build; surface "SSO/SAML on request" and integrate WorkOS ($0 dev; $125/connection when a paying enterprise asks). See review §7.3. (est: $1.50)
- [ ] [P2] [post-enterprise-customer-s3-sync] rust-crate-builder: Customer S3-compatible bucket sync + SIEM CEF/LEEF export (any S3 endpoint; not AWS-specific). Post-beta. (est: $1.00)
- [ ] [P3] [post-watch-pr-bot-v2] rust-crate-builder: Cost-diff PR comments (same free GitHub App; needs ≥14d telemetry baseline). Post-beta. (est: $1.50)
- [ ] [P3] [post-clickhouse-migration] rust-crate-builder: ClickHouse dual-write for request_logs at Scale rate. [BLOCKED — post-beta] (est: $2.00)
- [ ] [P1] [env-secret-split-rotate] ops: Rotate live keys read this session (TT_MASTER_KEY re-encrypt, TT_ADMIN_TOKEN, FLY_DEPLOY_KEY, Stripe) + split dev/prod secret sets. [BLOCKED — human: key rotation] Re-encryption primitive + runbook already shipped. (est: —)
