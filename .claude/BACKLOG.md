# Backlog

Single source of truth for actionable work. Entries are checkboxes; flip to `[x]` when done. Sync to GitHub Issues with `autopilot` label via `./scripts/backlog.sh sync`.

**Format**: `- [PRIORITY] [task-id] subagent: brief description (est: $X.XX)`

- `PRIORITY` ∈ {P0 (blocker), P1 (next), P2 (soon), P3 (whenever)}
- `task-id` is short kebab-case, used in branch names: `autopilot/<task-id>`
- `subagent` is the matching specialist: `rust-crate-builder`, `provider-adapter-author`, `inspect-rule-author`, `astro-page-builder`, `plan-replay-validator`
- `est` is a rough token-cost estimate

## Week 1 — Gateway skeleton + OpenAI adapter

- [x] [P0] [w1-axum-skeleton] rust-crate-builder: Wire Axum app in `crates/core/src/server.rs` with `/v1/chat/completions` POST, `/v1/embeddings` POST, `/v1/models` GET, `/health` GET. Pass through to provider registry. (est: $0.40)
- [x] [P0] [w1-openai-trait-impl] provider-adapter-author: Implement `Provider` trait for `tt-provider-openai` — chat_completion (non-streaming first), pricing table for GPT-5.5/5.4/o3/o4-mini/4o/4o-mini. (est: $0.80)
- [x] [P0] [w1-openai-streaming] provider-adapter-author: SSE streaming for OpenAI, with insta snapshot fixtures for typed events. (est: $0.60)
- [x] [P0] [w1-openai-tests] provider-adapter-author: httpmock tests for 200/429/500/network-reset/malformed-JSON/partial-stream. Min 20 fixtures. (est: $0.50)
- [x] [P1] [w1-redis-l1] rust-crate-builder: Wire `tt-cache` Redis impl. Key = SHA-256 of normalized request. Default TTL 24h. (est: $0.40)
- [x] [P1] [w1-request-logs-schema] rust-crate-builder: SQLx migration for `request_logs` table per spec §8.2. Include `baseline_cost_usd`, `actual_cost_usd`, `cache_layer`, `trace_id`. (est: $0.50)
- [x] [P1] [w1-otel-init] rust-crate-builder: OpenTelemetry init in `tt-telemetry`. Span the full request lifecycle. Trace ID into response header. (est: $0.30)
- [x] [P1] [w1-audit-write-path] rust-crate-builder: Audit-row write path in `tt-telemetry::audit`, hash-chained + Ed25519. Called on `provider_credential.add`, `route.create`, etc. (est: $0.70)
- [x] [P2] [w1-dockerfile] rust-crate-builder: Multi-stage Dockerfile for `tt-cli`, push to GHCR. (est: $0.20)
- [x] [P2] [w1-fly-toml] rust-crate-builder: `fly.toml` for the Gateway. Single-region `iad` initially. (est: $0.20)

## Week 0 follow-ups (not blocking, but worth doing)

- [x] [P2] [w0-cargo-resolves] verify cargo workspace resolves once Rust 1.85 is installed locally.
- [x] [P1] [w0-fp-rate-script] write `scripts/measure-fp-rate.sh` for Inspect rule FP measurement (used by inspect-rule-author). Runs each rule against its `should-detect` / `should-not-detect` fixtures in `crates/inspect-rules-tier1/tests/rules/`, computes per-rule precision / recall / FP rate, exits non-zero if any rule >5%. Rules + fixtures all shipped — script is pure bash + jq. (est: $0.30)
- [ ] [P2] [w0-corpora-seed] add `corpora/` directory with small open-source LangChain/Vercel-AI samples for Inspect FP measurement. [BLOCKED — defer to Week 14 prep]
- [x] [P3] [w0-pr-template] add `.github/pull_request_template.md` enforcing handoff format. _(done in this session)_
- [x] [P3] [w0-issue-templates] add `.github/ISSUE_TEMPLATE/` for autopilot/bug/feature. _(done in this session)_

- [x] [P1] [w1-openai-register-in-core] rust-crate-builder: Register OpenAiProvider in crates/core/src/registry.rs; update build_router caller to seed AppState with it. Update tt-core models endpoint test to assert OpenAI models appear. (est: $0.30)

## Week 2-4 — Anthropic adapter (the worked reference)

- [x] [P0] [w2-anthropic-trait] provider-adapter-author: Implement `Provider` trait for `crates/providers/anthropic/` — chat_completion, streaming, embeddings stubs, pricing table for Haiku 4.5 / Sonnet 4.6 / Opus 4.7. (est: $1.00)
- [x] [P0] [w2-anthropic-translate] provider-adapter-author: `translate.rs` — separate system messages into Anthropic `system` block array, map tool_calls → ToolUse blocks, map Tool role → user ToolResult, default max_tokens=4096. Min 20 fixtures. (est: $1.20)
- [x] [P0] [w2-anthropic-cache-inject] provider-adapter-author: Auto-inject `cache_control: ephemeral` on last system block when token count ≥ 1024 in `translate.rs`; config flag to disable. (est: $0.60)
- [x] [P0] [w2-anthropic-stream] provider-adapter-author: Typed SSE stream translation in `stream.rs` — `message_start`, `content_block_delta`, `message_delta`, tool-use JSON fragment accumulation, ping skip, mid-stream error propagation. (est: $1.00)
- [x] [P0] [w2-anthropic-error-map] provider-adapter-author: `errors.rs` — map 401→Unauthorized, 429→RateLimited with retry_after_ms, 400→InvalidRequest, 404→ModelNotFound, 5xx→ProviderUpstream. (est: $0.40)
- [x] [P1] [w2-anthropic-httpmock-tests] provider-adapter-author: httpmock integration tests for 200/429/500/network-reset/malformed-event/partial-stream; insta snapshots for translate_request and translate_response. Min 20 fixture cases. (est: $0.80)
- [x] [P1] [w3-anthropic-register] rust-crate-builder: Register AnthropicProvider in `crates/core/src/registry.rs`; update models endpoint to include Anthropic models; verify contract test passes. (est: $0.30)
- [x] [P1] [w4-load-test-gateway] rust-crate-builder: Load test script using `oha` against local Gateway: p50 miss <30ms, p50 hit <5ms on cache; verify audit rows written for `route.*` events. (est: $0.40)

## Week 5 — Gateway provider breadth (compatible providers)

- [x] [P0] [w5-gemini-adapter] provider-adapter-author: Implement `Provider` trait for `crates/providers/gemini/` — translate to `:generateContent` REST endpoint, `systemInstruction` extraction, `functionDeclarations` tool format, JSON-array `:streamGenerateContent` streaming, pricing for Flash-Lite / Flash / Pro with context-length brackets. (est: $1.50)
- [x] [P0] [w5-gemini-stream] provider-adapter-author: Gemini JSON-array stream translation in `crates/providers/gemini/src/stream.rs` — parse server-streamed JSON chunks, emit OpenAI-format ChatCompletionChunks, aggregate usage from final chunk. (est: $0.80)
- [x] [P1] [w5-mistral-adapter] provider-adapter-author: Implement `Provider` trait for `crates/providers/mistral/` using `OpenAICompatibleProvider` base; pricing table; model list; base_url override. (est: $0.40)
- [x] [P1] [w5-groq-adapter] provider-adapter-author: Implement `Provider` trait for `crates/providers/groq/` using `OpenAICompatibleProvider` base; pricing table; model list. (est: $0.40)
- [x] [P1] [w5-together-adapter] provider-adapter-author: Implement `Provider` trait for `crates/providers/together/` using `OpenAICompatibleProvider` base; pricing table; model list. (est: $0.40)
- [x] [P1] [w5-openrouter-adapter] provider-adapter-author: Implement `Provider` trait for `crates/providers/openrouter/` using `OpenAICompatibleProvider` base; 5% BYOK fee in pricing; model list via `/models` endpoint. (est: $0.50)
- [x] [P2] [w5-provider-contract-ci] rust-crate-builder: Add `provider-contract-tests.yml` GitHub Actions workflow — weekly schedule, `--ignored` flag, $5/wk cap, opens issue with `provider-broken` label on failure. (est: $0.40)

## Week 6 — Local providers + streaming hardening

- [x] [P0] [w6-local-crate] provider-adapter-author: Implement `crates/providers/local/` crate with `OpenAICompatibleProvider` impl, `is_local: true` flag, cost_per_million=0, higher default timeouts; covers Ollama (port 11434), vLLM (port 8000), LM Studio (port 1234). (est: $0.60)
- [x] [P0] [w6-gateway-dispatch] rust-crate-builder: Wire gateway request pipeline in tt-core: resolve provider via registry.by_model(req.model), dispatch chat_completion (non-streaming) + chat_completion_stream (SSE), populate X-TokenTrimmer-{Provider,Model-Used,Cost-Usd,Baseline-Cost-Usd,Saved-Usd} response headers, structured trace span. Skip auth/cache/routing — bare minimum dispatch. Tests: 200 success via mocked provider in registry, 404 on unknown model, streaming end-to-end. (est: $0.80)
- [x] [P0] [w6-concurrent-sse-test] rust-crate-builder: 100 concurrent SSE streams integration test in `crates/core/tests/`; gate must pass before Week 7. (est: $0.60)
- [x] [P1] [w6-dogfood-groq-routing] rust-crate-builder: Wire classification routing rule in local Gateway config to send short prompts to Groq for internal dogfooding; verify `route.*` audit rows. All deps shipped (routing engine in `crates/routing/`, audit-row wiring in `tt-telemetry`, Groq adapter in `crates/providers/groq/`, Groq key in .env.development). (est: $0.30) _Shipped 2026-05-29 — TT_DOGFOOD_GROQ_ROUTING env wires in-memory routing store with classification rule (short flagship prompts → llama-3.1-8b-instant)._

## Week 7-8 — Auth + billing + dashboard skeleton

- [x] [P0] [w7-magic-link-auth] rust-crate-builder: Auth.js magic-link via Resend in `apps/dashboard/src/lib/auth.ts`; sessions stored in Postgres, signed with rotating HMAC secret; no passwords. (est: $0.80) _Shipped in `tokentrimmer-cloud` 2026-05-27 as custom 100-line magic-link impl (no Auth.js dep)._
- [x] [P0] [w7-stripe-checkout] rust-crate-builder: Stripe Checkout integration in `crates/api/src/billing/`; create customer + subscription on first payment; store in `subscriptions` table. (est: $0.80) _Shipped 2026-05-27 — hand-rolled `StripeClient`, no async-stripe._
- [x] [P0] [w7-stripe-portal] rust-crate-builder: Stripe Customer Portal link endpoint; cancel flow downgrades subscription in `subscriptions` table via webhook. (est: $0.50) _Shipped 2026-05-27 — `POST /v1/admin/billing/portal`._
- [x] [P0] [w7-stripe-webhooks] rust-crate-builder: Stripe webhook handler for `customer.subscription.*` and `invoice.payment_*` events — verify signature, update local state, write audit row for `subscription.changed`. (est: $0.70) _Shipped 2026-05-27 — HMAC-SHA256 verify + replay guard + dispatch/apply + audit emit (`subscription.{initialized,updated,canceled}`)._
- [x] [P0] [w7-api-key-issuance] rust-crate-builder: API key issuance in `crates/auth/src/keys.rs` — `tt_live_*` prefix format, argon2 hash stored, prefix only shown in UI; `apikey.issued` audit row. (est: $0.60)
- [x] [P0] [w7-provider-cred-storage] rust-crate-builder: Provider credential storage with XChaCha20-Poly1305 encryption in `crates/api/src/credentials/`; key from Fly secrets; `provider_credential.add` audit row. (est: $0.70) _Shipped 2026-05-27 — Postgres impl with per-row HKDF derivation + AAD bound to (org_id, provider)._
- [x] [P1] [w8-dashboard-shell] astro-page-builder: Dashboard shell — login page, "create your first key" onboarding step, empty request log table at `/`; Auth.js session required on all routes. (est: $0.80) _Shipped 2026-05-27 in tokentrimmer-cloud apps/dashboard._
- [x] [P1] [w8-audit-row-gate] rust-crate-builder: Synthetic audit-row integration test — assert that `user.login`, `apikey.issued`, `subscription.changed`, `provider_credential.add` events are all written with valid hash-chain links. (est: $0.50)

## Week 11 — HARD CHECKPOINT prep

- [ ] [P0] [w11-e2e-smoke-test] rust-crate-builder: Playwright e2e test covering signup → magic-link → Stripe $1 test → issue key → curl Gateway → dashboard entry appears within 30s. (est: $0.80) [BLOCKED — needs cloud repo + GitHub remote + external accounts (Stripe/Resend/Neon/Fly)]
- [x] [P0] [w11-latency-smoke] rust-crate-builder: `oha` load smoke script against prod Fly `iad` — assert p50 miss <30ms, p50 hit <5ms; include in `make ci-local`. (est: $0.30)
- [x] [P0] [w11-webhook-replay-attack] rust-crate-builder: Stripe webhook replay-attack test — verify replayed webhook with old timestamp is rejected. (est: $0.30) _Shipped 2026-05-27 — 10 unit tests in `tokentrimmer-cloud/crates/api/src/billing.rs` cover old/future timestamps + multi-key rotation._
- [ ] [P0] [w11-cancel-downgrade] rust-crate-builder: Integration test verifying Customer Portal cancel sets subscription status to `canceled` and removes overage access in `crates/auth/src/keys.rs` quota check. (est: $0.40) [BLOCKED — needs cloud repo + GitHub remote + external accounts (Stripe/Resend/Neon/Fly)]
- [x] [P1] [w11-sentry-integration] rust-crate-builder: Wire Sentry DSN into Gateway (cli/main.rs) — `sentry::init()` with rustls transport before tracing init, guard held for process lifetime, panic handler auto-wired. Cloud crate split deferred until tokentrimmer/cloud exists. (est: $0.30)
- [x] [P1] [w11-sandbox-test-key] rust-crate-builder: `tt_test_*` sandbox key returns deterministic synthetic response without calling real providers; all response headers populated. (est: $0.40)

## Week 12-13 — Semantic cache (pgvector L2) + cost pages

- [x] [P0] [w12-pgvector-migration] rust-crate-builder: SQLx migration adding `cache_entries` table with `vector(1536)` column and HNSW index per spec §8.2; enable `pgvector` extension in Neon. (est: $0.30)
- [x] [P0] [w12-embedding-pipeline] rust-crate-builder: Embedding pipeline in `crates/cache/src/l2.rs` — embed last user message via OpenAI `text-embedding-3-small`, insert into `cache_entries`, cosine-similarity lookup with per-org threshold default 0.92. (est: $1.00)
- [x] [P0] [w12-l2-cache-hit-path] rust-crate-builder: Wire L2 semantic cache into Gateway request flow in `crates/core/src/gateway.rs`; set `X-TokenTrimmer-Cache: hit-l2` header; per-org isolation via `org_id` predicate. (est: $0.60)
- [x] [P0] [w12-reconciliation-job] rust-crate-builder: Daily reconciliation worker using Apalis on Postgres (ADR-007) — compare claimed savings against provider invoice totals; write `weekly_reconciliation` table; banner if drift >2%. (est: $1.00) _Shipped 2026-05-27 as in-process daily scheduler (Apalis deferred until rustc 1.94); backfills `plan_runs.actual_*` + `trust_score`._
- [x] [P1] [w13-dashboard-overview] astro-page-builder: Dashboard `/` overview page — current month spend, savings month-to-date, cache hit rate, top 5 cost drivers; Solid.js islands for charts via uPlot. (est: $0.80) _Shipped 2026-05-27 with uPlot CostChart island + spend anomaly banner._
- [x] [P1] [w13-dashboard-costs] astro-page-builder: Dashboard `/costs` cost-explorer page — drill down by API key, model, route, custom tag, time period; `@tanstack/solid-query` for data fetching. (est: $0.80) _Shipped 2026-05-27 — by-dimension breakdown + window selector + CostBarChart + CSV/JSONL export._

## Week 14 — Inspect CLI + 10 P0 rules

- [x] [P0] [w14-inspect-cli-binary] rust-crate-builder: `tt inspect` binary in `crates/cli/`; tree-sitter for Python + TypeScript; markdown + JSON output formats; `--fail-on` flag; `--output` flag. (est: $0.80)
- [x] [P0] [w14-inspect-rule-harness] rust-crate-builder: Rule engine harness in `crates/inspect-core/`; loads YAML rule files; runs Tier 1 (AST/regex) and Tier 2 (small model) rules; FP rate measurement via `scripts/measure-fp-rate.sh`. (est: $1.00)
- [x] [P0] [w14-inspect-cache-anthropic-prompt-cache-missing] inspect-rule-author: Rule `cache-anthropic-prompt-cache-missing` — detect Anthropic calls with system ≥1024 tokens lacking `cache_control`; ≥5 positive + ≥10 negative fixtures; FP rate <5% on corpora/. (est: $0.20)
- [x] [P0] [w14-inspect-cache-openai-prompt-cache-eligible] inspect-rule-author: Rule `cache-openai-prompt-cache-eligible` — detect OpenAI calls with static prefix ≥1024 tokens that could benefit from prompt caching; fixtures + FP measurement. (est: $0.20)
- [x] [P0] [w14-inspect-lib-anthropic-sdk-no-cache-control] inspect-rule-author: Rule `lib-anthropic-sdk-no-cache-control` — detect Anthropic SDK calls with long system prompts missing `cache_control`; Python + TS tree-sitter queries; fixtures. (est: $0.20)
- [x] [P0] [w14-inspect-model-flagship-for-classification] inspect-rule-author: Rule `model-flagship-for-classification` — Tier 2 small-model classifier detects GPT-4*/Claude Sonnet/Gemini Pro calls where prompt asks for classification/label/boolean; fixtures. (est: $0.30)
- [x] [P0] [w14-inspect-model-flagship-for-extraction] inspect-rule-author: Rule `model-flagship-for-extraction` — Tier 2 small-model classifier detects flagship-model calls extracting structured data from short inputs; fixtures. (est: $0.30)
- [x] [P0] [w14-inspect-output-no-max-tokens] inspect-rule-author: Rule `output-no-max-tokens` — Tier 1 AST detect LLM calls without `max_tokens` parameter; Python + TS; ≥5 positive + ≥10 negative fixtures. (est: $0.20)
- [x] [P0] [w14-inspect-conversation-unbounded-history] inspect-rule-author: Rule `conversation-unbounded-history` — Tier 2 detect conversation handlers appending messages indefinitely without pruning; fixtures. (est: $0.30)
- [x] [P0] [w14-inspect-agent-no-termination-condition] inspect-rule-author: Rule `agent-no-termination-condition` — Tier 2 detect agent loops without max-iteration cap or explicit termination; fixtures. (est: $0.30)
- [x] [P0] [w14-inspect-config-no-agents-md] inspect-rule-author: Rule `config-no-agents-md` — Tier 1 detect repo root missing `AGENTS.md`, `CLAUDE.md`, `.cursor/rules` or equivalent; fixtures. (est: $0.20)
- [x] [P0] [w14-inspect-config-agents-md-contains-secrets] inspect-rule-author: Rule `config-agents-md-contains-secrets` — Tier 1 detect API keys, tokens, passwords in AGENTS.md using high-entropy + keyword patterns; fixtures. (est: $0.20)
- [ ] [P0] [w14-corpora-seed] rust-crate-builder: Seed `corpora/` with small open-source LangChain and Vercel-AI samples; run `scripts/measure-fp-rate.sh` on all 10 rules; gate: FP rate <5% on each. (est: $0.30) [BLOCKED — needs independently-sourced OSS samples + scripts/measure-fp-rate.sh; biased if author of rules also seeds corpora]

## Week 15 — Inspect hosted backend + GitHub Action

- [x] [P0] [w15-hosted-scan-endpoint] rust-crate-builder: `POST /v1/admin/inspect/runs` endpoint in `crates/api/`; trigger async scan job via Apalis; `GET /v1/admin/inspect/runs/:id` for status + findings; short-lived action token scoped to `/inspect/runs` only. (est: $0.80) _Shipped 2026-05-27 as synchronous spawn_blocking scan (Apalis deferred); persists inspect_runs + inspect_findings + audit emit._
- [x] [P0] [w15-inspect-findings-schema] rust-crate-builder: SQLx migrations for `inspect_runs` and `inspect_findings` tables per spec §8.2; index by `run_id`, `severity`, `rule_id`. (est: $0.30)
- [x] [P0] [w15-github-action] rust-crate-builder: `tokentrimmer/inspect-action@v1` GitHub Action wrapper — `action.yml` with `token`, `fail-on` inputs; posts check-run summary on PR; authenticate short-lived token via hosted backend. (est: $0.60)
- [x] [P1] [w15-dashboard-inspect-page] astro-page-builder: Dashboard `/inspect` page — open findings, severity distribution, projected savings if addressed; Solid.js island for findings table. (est: $0.60) _Shipped 2026-05-27 — past-runs index + per-run detail + paste-and-scan form._
- [x] [P1] [w15-inspect-self-ci] rust-crate-builder: `inspect-self.yml` GitHub Actions workflow — runs `tt inspect` on own repo as blocking CI gate; fails on new HIGH/CRITICAL findings; dogfood gate from Week 14 forward. (est: $0.30)

## Week 16-17 — Plan engine (cost projection)

- [x] [P0] [w16-plan-core-replay] plan-replay-validator: Implement replay engine core in `crates/plan-core/src/replay.rs` — fetch `request_logs` for window, apply proposed config routes, project new model + cost per request; determinism gate (same seed → bit-identical output). (est: $2.00)
- [x] [P0] [w16-plan-l1-cache-projection] plan-replay-validator: L1 cache projection in `crates/plan-core/src/cache.rs` — exact cache key SHA-256 match within proposed TTL window on in-memory request set. (est: $1.00)
- [x] [P0] [w16-plan-l2-cache-projection] plan-replay-validator: L2 semantic cache projection — pgvector HNSW in-memory index over window embeddings; cosine-similarity threshold sweep (0.85/0.90/0.92/0.95); cache-poisoning detection heuristics. (est: $2.00)
- [x] [P0] [w16-plan-bootstrap-ci] plan-replay-validator: Bootstrap CI in `crates/plan-core/src/ci.rs` — 10K iterations non-parametric resampling for cost, savings, cache hit rate, latency percentiles; wide-CI warning when relative width >30%. (est: $2.00)
- [x] [P0] [w17-plan-cli] rust-crate-builder: `tt plan` CLI subcommand — `--diff`, `--window`, `--sample`, `--quality-budget` flags; render plan summary output matching spec §14 format; `--apply` flag triggers `POST /v1/admin/plans/:id/apply`. (est: $0.80)
- [x] [P0] [w17-plan-admin-endpoint] rust-crate-builder: `POST /v1/admin/plans` + `GET /v1/admin/plans/:id` + `POST /v1/admin/plans/:id/apply` in `crates/api/src/plan/`; 202 Accepted with plan_id; async job via Apalis; apply writes atomic Postgres transaction + hot-reload event. (est: $1.00) _Shipped 2026-05-27 — synchronous `tt_plan_core::replay` + plan_runs persistence + idempotent apply + audit emit._
- [x] [P1] [w17-plan-reconciliation-schema] rust-crate-builder: `plan_runs` table migration per spec §8.2; `plan.applied` audit row; 7d and 30d reconciliation report generation comparing projected vs actual metrics. (est: $0.60)
- [x] [P1] [w17-plan-self-ci] rust-crate-builder: `plan-self.yml` GitHub Actions workflow — replays synthetic 10K-request workload with insta snapshot; fails if replay output drifts; dogfood gate from Week 17. (est: $0.40)

## Week 18-20 — Reporting (dashboard + weekly digest + trust score)

- [x] [P0] [w18-dashboard-cache-page] astro-page-builder: Dashboard `/cache` page — hit rates over time, top cached patterns (no raw prompts), L1 vs L2 breakdown; uPlot time-series chart island. (est: $0.60) _Shipped 2026-05-27 — L1/L2/miss cards + stacked uPlot island + top-models table._
- [x] [P0] [w18-dashboard-routes-page] astro-page-builder: Dashboard `/routes` page — list of active routes, hit rates, savings per route; enable/disable toggle; link to Plan history for each route. (est: $0.60) _Shipped 2026-05-27 — full CRUD with create/edit modal + per-row 24h hit + cached % columns + JS infer_provider pre-submit check._
- [x] [P0] [w18-dashboard-plan-pages] astro-page-builder: Dashboard `/plan/index.astro` list + `/plan/[id].astro` detail — past simulations, applied vs unapplied status, projected vs actual comparison, per-route savings breakdown drill-down. (est: $0.80) _Shipped 2026-05-27 — list + detail with ownership-gated Apply button._
- [x] [P0] [w18-dashboard-reports-page] astro-page-builder: Dashboard `/reports` page — reconciliation report table, week-over-week comparison, trust score display, download links for CSV/JSON export. (est: $0.60) _Shipped 2026-05-27 — summary cards + inline SVG trust sparkline + projected-vs-actual delta table + CSV/JSON download._
- [x] [P0] [w19-weekly-digest-email] rust-crate-builder: Weekly digest email worker — Resend MJML template with last-week spend, savings, cache hit rate, anomalies, open Inspect findings count, unapplied Plans reminder; scheduled Monday 09:00 local time per org via Apalis. (est: $0.80) _Shipped 2026-05-27 — in-process tokio scheduler at next Monday 09:00 UTC + Resend REST (no SDK)._
- [x] [P0] [w19-trust-score] rust-crate-builder: Trust score computation in `crates/api/src/trust.rs` — 0-100 rolling variance between projected and actual savings across last N applied Plans; update on each reconciliation run; write to `plan_runs.trust_score`; surface in dashboard. (est: $0.80) _Shipped 2026-05-27 — `1 - clamp(|p-a|/max(p,a,1), 0, 1)` in reconciliation.rs; surfaced on /reports + overview card._
- [x] [P1] [w20-dashboard-settings-pages] astro-page-builder: Dashboard settings sub-pages — `/settings/api-keys.astro`, `/settings/providers.astro`, `/settings/billing.astro`, `/settings/team.astro`; Stripe Portal link; key revocation; provider credential add/delete. (est: $0.80) _Shipped 2026-05-27 — `/settings` hub linking to /keys, /credentials, /billing, /audit. Team page deferred until org-member CRUD lands._
- [ ] [P2] [w20-dashboard-perf-gate] rust-crate-builder: Playwright p75 load time measurement on dashboard pages; gate: p75 <1.5s; add to `make ci-local`. Dashboard exists at `cloud/apps/dashboard/` (60 files). Script runs against the cloud dashboard (path lives in cloud repo). (est: $0.30) [CLOUD-REPO]
- [x] [P2] [w20-inspect-badge] rust-crate-builder: README badge endpoint — `GET /v1/badges/:org_id/inspect` returns SVG with current high/critical/medium counts; serve from `crates/api/`. (est: $0.30) _Shipped 2026-05-27 — `GET /v1/admin/inspect/badge` returns shield-style SVG colored by most-severe non-zero bucket (red/orange/amber/gray/green); dashboard /inspect renders an inline preview via the proxy. Public unauthenticated README embed needs a signed-URL flow (TODO)._

## Week 21-22 — Polish (PDF, Tier 3 quality, anomaly)

- [ ] [P1] [w21-typst-pdf] rust-crate-builder: Monthly executive PDF in `cloud/crates/worker/src/pdf.rs` using `typst-pdf` crate (ADR-005). Partial — `pdf.rs` exists in cloud worker; needs finish + integration with R2 upload + Resend email. (est: $1.00) [CLOUD-REPO]
- [ ] [P1] [w21-pdf-scheduled-job] rust-crate-builder: Schedule PDF generation on the 1st of each month per org; upload to Cloudflare R2; email link via Resend; store R2 key in `reports` table. All deps wired (R2 client in `cloud/crates/api/src/r2_client.rs`, Resend in dashboard lib). (est: $0.50) [CLOUD-REPO]
- [x] [P1] [w21-plan-quality-scoring] plan-replay-validator: Tier 3 LLM-judge quality scoring in `crates/plan-core/src/quality.rs` — stratified sampling, re-run against proposed model (opts-in required), judge-prompt scoring, risk band aggregation (HIGH/MEDIUM/LOW) per spec §7.4. (est: $2.00)
- [x] [P1] [w22-anomaly-detection] rust-crate-builder: Anomaly detection worker in `crates/worker/src/anomaly.rs` — z-score on hourly spend with seasonal decomposition; trigger webhook `anomaly.detected` + dashboard notification when deviation >3σ; synthetic 5σ test must fire. (est: $0.80) _Shipped 2026-05-27 — `lib/anomaly.ts::detectSpendAnomaly` recomputes at request time; banner on `/` overview when current hour > 3σ. Seasonal decomposition + webhook deferred._
- [x] [P2] [w22-data-export] rust-crate-builder: CSV/JSON export endpoints in `crates/api/src/export/` — `GET /v1/admin/export/requests` and `GET /v1/admin/export/plans`; scoped to retention window; email download link via Resend when file ready. (est: $0.50) _Shipped 2026-05-27 — `GET /v1/admin/export/requests?format=jsonl|csv` + dashboard /costs export anchors. Plans export via /api/reports/plan-runs. Email-async delivery deferred._

## Week 23-24 — Private alpha

- [ ] [P0] [w23-free-tier-live] rust-crate-builder: Enable Free tier in production — GitHub OAuth required gate (>7d account, >0 public commits), Cloudflare Turnstile captcha, 60 req/min hard cap, 5K req/mo hard cap (no overage), L1 cache only (no L2 writes), max 2 keys + 2 provider creds per Free org. (est: $0.80) [BLOCKED — needs cloud repo + GitHub remote + external accounts (Stripe/Resend/Neon/Fly/Cloudflare/GitHub OAuth/Stripe)]
- [x] [P0] [w23-onboarding-script] astro-page-builder: Onboarding flow — guided steps for first key, first provider cred, first Gateway call; write `onboarding.step.completed` audit events at each step; visible in `/` dashboard overview. (est: $0.60) _Shipped 2026-05-27 — `lib/onboarding.ts::getOnboardingState` derives state from api_keys + provider_credentials + request_logs EXISTS queries (one round-trip). `/` overview renders a 3-step panel above the KPI cards until complete. Audit emit dropped — `apikey.issued` / `credential.added` / `request_logs` rows already record the milestones._
- [ ] [P0] [w23-alpha-reconciliation-gate] rust-crate-builder: Run daily reconciliation for 14 consecutive days; gate: drift ≤2% every day; surface in STATUS.md; must pass before beta launch. (est: $0.30) [BLOCKED — needs cloud repo + GitHub remote + external accounts (Stripe/Resend/Neon/Fly/Cloudflare/GitHub OAuth/Stripe)]
- [ ] [P0] [w24-alpha-inspect-fp-gate] rust-crate-builder: Confirm Inspect FP rate <5% on alpha user traffic; `scripts/measure-fp-rate.sh` run against alpha org repos; document results in STATUS.md. (est: $0.30) [BLOCKED — needs cloud repo + GitHub remote + external accounts (Stripe/Resend/Neon/Fly/Cloudflare/GitHub OAuth/Stripe)]
- [x] [P1] [w24-alpha-bug-triage] rust-crate-builder: Zero P0 bugs open >24h gate; create `scripts/check-p0-bugs.sh` that queries GitHub Issues for open P0 + age >24h; include in `make ci-local`. Pure shell + `gh` CLI; no cloud-repo dep. (est: $0.20)

## Week 25-26 — Bug fix + beta launch

- [ ] [P0] [w25-pro-tier-live] rust-crate-builder: Enable Pro tier ($99/mo) in Stripe; verify quota limits (500K req/mo, 90d retention, CSV/JSON export); gate: Stripe Checkout + Portal flow green in prod. (est: $0.40) [BLOCKED — needs cloud repo + GitHub remote + external accounts (Stripe/Resend/Neon/Fly)]
- [ ] [P0] [w25-game-day-runbook] rust-crate-builder: Rehearse game-day: kill Fly `iad` region, observe failover to restore; document in `runbooks/region-failover.md`; verify all PagerDuty alert runbooks exist (100% coverage). (est: $0.50) [BLOCKED — needs cloud repo + GitHub remote + external accounts (Stripe/Resend/Neon/Fly/Cloudflare/GitHub OAuth/Stripe)]
- [ ] [P0] [w25-penetration-test] rust-crate-builder: Pre-launch security checklist — auth path (magic-link replay, session fixation), key handling (timing attack on argon2 compare), audit integrity (hash chain tamper), cross-tenant isolation (org_id scoping); document findings in `SECURITY.md`. (est: $0.50) [BLOCKED — needs cloud repo + GitHub remote + external accounts (Stripe/Resend/Neon/Fly)]
- [ ] [P0] [w26-launch-hn-reddit] astro-page-builder: Pre-launch checklist — HN Show HN post drafted, Reddit r/programming post drafted, IH product page, Product Hunt scheduled; verify `tokentrimmer.com` marketing site has correct pricing table, changelog, first blog post live. (est: $0.30) [BLOCKED — needs cloud repo + GitHub remote + Resend/Stripe/Neon accounts]
- [x] [P1] [w26-status-page] rust-crate-builder: UptimeRobot status page at `status.tokentrimmer.com` monitoring `/health` on Gateway and API; RSS feed; in-app banner integration for incidents; Sentry WARN+ zero-unticketed gate. (est: $0.30) _In-app banner shipped 2026-05-27 — `lib/uptimerobot.ts` (60s in-process cache, form-urlencoded v2 API client) + incident banner on `/` listing all DOWN/seems-down monitors. External status page DNS + Sentry-WARN gate still operator config._
- [x] [P2] [w26-sdk-python-publish] rust-crate-builder: Publish `sdk-python/` as `tokentrimmer` to PyPI; thin wrapper over openai SDK with `tt_tag` convenience param and `.tt` metadata accessor on responses. (est: $0.40)
- [x] [P2] [w26-sdk-typescript-publish] rust-crate-builder: Publish `sdk-typescript/` as `@tokentrimmer/client` to npm; thin wrapper over openai SDK with `ttTag` convenience param and `.tt` metadata accessor. (est: $0.40)

## Post-beta backlog (Team/Scale/Enterprise rollout)

- [ ] [P1] [post-team-rbac] rust-crate-builder: Team tier RBAC — `org_members.role` enum (owner/admin/member) enforced in API middleware; multiple API keys per role; up to 25 seats. (est: $0.80) [BLOCKED — post-beta cloud/enterprise track]
- [ ] [P1] [post-team-pr-bot] rust-crate-builder: PR bot GitHub App integration for Team tier — up to 10 repos; posts Inspect findings as check-run on every PR; `fail-on: high` configurable. (est: $0.80) [BLOCKED — post-beta cloud/enterprise track]
- [ ] [P1] [post-team-sso] rust-crate-builder: Google + GitHub OAuth SSO for Team tier — Auth.js additional providers; session merging with existing magic-link accounts. (est: $0.60) [BLOCKED — needs cloud repo + GitHub remote + external accounts (Stripe/Resend/Neon/Fly)]
- [ ] [P1] [post-scale-s3-object-lock] rust-crate-builder: AWS S3 Object Lock Compliance mode audit storage for Scale tier (ADR-009) — sync audit rows to customer-controlled S3 bucket with WORM; `tt audit verify` CLI reads from S3. (est: $1.00) [BLOCKED — needs cloud repo + GitHub remote + external accounts (Stripe/Resend/Neon/Fly/Cloudflare/GitHub OAuth/Stripe)]
- [ ] [P1] [post-scale-slo-proof] rust-crate-builder: SLO dashboard page — rolling 30d uptime, p95 latency, error rate; signed monthly summary PDF showing SLO adherence; required for Scale tier marketing. (est: $0.60) [BLOCKED — post-beta cloud/enterprise track]
- [ ] [P2] [post-enterprise-workos-saml] rust-crate-builder: WorkOS SAML/OIDC integration for Enterprise tier — replace magic-link with WorkOS managed SSO ($125/connection/mo); configure per-org; audit row for `auth.saml.login`. (est: $1.50) [BLOCKED — needs cloud repo + GitHub remote + external accounts (Stripe/Resend/Neon/Fly)]
- [ ] [P2] [post-enterprise-customer-s3-sync] rust-crate-builder: Customer S3 bucket sync for Enterprise — IAM role assumption; real-time audit row replication to customer's bucket; SIEM CEF/LEEF export format. (est: $1.00) [BLOCKED — needs cloud repo + GitHub remote + external accounts (Stripe/Resend/Neon/Fly/Cloudflare/GitHub OAuth/Stripe)]
- [ ] [P2] [post-enterprise-docker-compose] rust-crate-builder: Docker Compose self-host bundle for Enterprise — `docker-compose.yml` with Gateway + API + Worker + Postgres+pgvector + Redis + Nginx; `make setup` bootstraps keys and DB; docs at `docs.tokentrimmer.com/self-host`. (est: $0.80) [BLOCKED — needs cloud repo + GitHub remote + external accounts (Stripe/Resend/Neon/Fly/Cloudflare/GitHub OAuth/Stripe)]
- [ ] [P3] [post-watch-pr-bot-v2] rust-crate-builder: Watch product PR bot for Pro tier — cost diff comment on every PR using Plan projection against merged diff; requires Gateway telemetry baseline ≥14d. (est: $1.50) [BLOCKED — post-beta cloud/enterprise track]
- [ ] [P3] [post-clickhouse-migration] rust-crate-builder: ClickHouse migration for `request_logs` at Scale-tier write rate — `crates/telemetry/` dual-write Postgres + ClickHouse; Postgres read path fallback; `crates/plan-core/` read adapter. (est: $2.00) [BLOCKED — post-beta cloud/enterprise track]


- [x] [P2] [w6-streaming-bench] rust-crate-builder: Criterion benchmark in crates/core/benches/streaming.rs — measure per-chunk SSE overhead through gateway dispatch (excluding provider latency). Target <1ms per chunk. Use mock provider for deterministic timing. (est: $0.40)

- [x] [P2] [w7-fake-stream-cache] rust-crate-builder: When L1 cache returns a hit for a stream:true request, fake-stream the cached response in chunks via routes/sse.rs to preserve UX. Tests: cached streaming request returns SSE not JSON; X-TokenTrimmer-Cache: hit-l1 header set. _Shipped 2026-05-27 — `sse::fake_stream_from_response` emits role/content/finish chunks; integration test confirms provider not called + body contains "[DONE]"._ (est: $0.50)

- [x] [P2] [w7-partial-cost-disconnect] rust-crate-builder: Record partial token-cost in request_logs when client disconnects mid-stream. Wrap the SSE response in a guard that observes Tokio Drop and emits the audit row with accumulated usage. Tests: simulated abort via Drop yields request_logs row with truncated completion_tokens. (est: $0.60) _Shipped 2026-05-29 — SSE stream wrapped in Drop-observing guard; partial usage logged with truncated=true on client abort, truncated=false on clean completion._

- [x] [P1] [fix-config-no-agents-md] inspect-rule-author: Fix config-no-agents-md false-positive: rule fires before walker reaches AGENTS.md because AtomicBool-based stateful design is filesystem-order-dependent. Refactor to a 2-pass approach (engine-level support for finalize() OR rule pre-scans for AGENTS-like files via its own walkdir on first invocation before firing). (est: $0.40)

- [x] [P1] [w4-routing-engine-impl] rust-crate-builder: Implement tt-routing::RoutingEngine — evaluate rule list against RequestContext + ChatCompletionRequest, return matched route. Currently empty stub. Required before w6-dogfood-groq-routing. (est: $0.50)

- [x] [P2] [w17-plan-apply-audit] rust-crate-builder: Add tt_plan_core::apply_plan() library function — updates plan_runs row to status=applied + applied_at, emits plan.applied audit row via AuditWriter (analogous to tt_auth::revoke_key). (est: $0.40)

- [x] [P2] [w17-plan-reconciliation-worker] rust-crate-builder: 7d/30d reconciliation report worker — Apalis job comparing projected vs actual metrics; writes back to plan_runs.actual_* columns + computes trust_score. _Shipped 2026-05-27 — in-process daily scheduler in `tokentrimmer-cloud/crates/api/src/reconciliation.rs` + `POST /v1/admin/reconciliation/run-now` manual trigger._ (est: $0.80)

## Deploy hardening (2026-05-26 session)

- [x] [P0] [deploy-tt-gateway-impl] rust-crate-builder: Implement `tt gateway` subcommand (was a 1-line `println!` stub). Wires `AppState::with_default_providers()` + `build_router()` + `axum::serve()` with graceful shutdown on SIGTERM/SIGINT. Boots in <1s. (est: $0.40)
- [x] [P0] [deploy-tt-gateway-boot-fix] rust-crate-builder: Make DB connect non-fatal at boot. Was crashlooping when Neon scale-to-zero cold start exceeded sqlx acquire timeout; now logs error and continues since no request handler currently depends on the pool. (est: $0.15)
- [x] [P0] [deploy-tt-config-impl] rust-crate-builder: Implement `tt-config` (was `pub struct Config;` stub). `Config::from_env()` reads `PORT`/`DATABASE_URL`/`REDIS_URL`/`SENTRY_DSN`/`TT_MASTER_KEY` with port-parse error path. (est: $0.20)
- [x] [P0] [deploy-l1-redis-middleware] rust-crate-builder: Wire `tt-cache::RedisL1Cache` into the chat handler. AppState gains `with_l1()` builder; chat.rs does L1 lookup before L2, returns cached response with `X-TokenTrimmer-Cache: hit-l1` on hit, async-inserts after provider miss. Keys are per-org-namespaced (`{org_id}:{sha256}`). Tests: hit/miss + streaming-bypass in `crates/core/tests/l1_cache_hit.rs`. (est: $0.50)
- [x] [P0] [deploy-rust-1.88-bump] rust-crate-builder: Bump rust-toolchain.toml + Cargo.toml + Dockerfile from 1.86 → 1.88. `cargo-chef 0.1.77` transitively required `cargo-platform 0.3.2` which needs 1.88. (est: $0.10)
- [x] [P0] [deploy-fly-app-rename] rust-crate-builder: Rename fly.toml app from `tokentrimmer-gateway` to `tokentrimmer` to match the app the user created. (est: $0.05)
- [x] [P1] [deploy-sentry-secret-scrubbing] rust-crate-builder: Add `send_default_pii: false` + `before_send` callback to Sentry init scrubbing authorization headers, cookies, query strings, frame locals, extras, and tags whose names contain `authorization`, `api_key`, `token`, `secret`, `tt_master_key`, `tt_live_`, `bearer`, `database_url`, etc. 3 tests in `cli/src/main.rs` cover headers/cookies/locals/extras/tags. (est: $0.20)

## Newly identified — to schedule

- [x] [P1] [trackB-preview-header-integration] rust-crate-builder: Wire `tt proxy` to call `POST /v1/preview` (fire-and-forget, with timeout) before forwarding each request, and inject `X-TT-Preview-Cost-Usd` + `X-TT-Suggested-Route` headers on the upstream response. Deferred in the trackB plan; trackC now ships so this can land. Module to extend: `crates/cli/src/proxy/routes/`. (est: $0.50)
- [x] [P1] [trackA-simulate-plan-tool] rust-crate-builder: Add `simulate_plan` MCP tool to `crates/mcp/src/tools/` — HTTP POST to `/v1/admin/plans` (existing in cloud) — and `mcp://tokentrimmer/plan/history?last=N` resource. Day-7 deferral from the trackA plan. (est: $0.80)
- [x] [P2] [trackA-sse-transport] rust-crate-builder: Add SSE transport to `crates/mcp/src/transport/sse.rs` for Cursor/Zed compatibility. `tt mcp --transport sse --sse-port 31416`. Day-14 deferral from trackA plan. (est: $1.00) _Shipped 2026-05-29 — bidirectional SSE transport (GET /sse + POST /messages?sessionId=…) wired alongside stdio; tt mcp --transport sse --sse-port 31416._
- [ ] [P2] [trackE-quality-audit-log] rust-crate-builder: Encrypted-prompt audit log for retrieval substitutions per Track E spec §10. Adds `retrieval_audit_log` table (schema + cloud migration in cloud-repo), public-side write path in retrieval middleware encrypts the original prompt with `TT_MASTER_KEY` before calling substitute. 30d retention. Enables customer-facing offline quality audit. (est: $1.50) [CLOUD-REPO partial — schema lives in cloud]
- [x] [P1] [trackE-runtime-activation] rust-crate-builder: tt-retrieval ships the engine + a Day-0 middleware annotation header (`x-tt-retrieval-enabled: v1-deferred-runtime`). Wire actual runtime activation in `crates/core/src/middleware/retrieval.rs`: read `TT_RETRIEVAL_STORE` + `TT_OPENAI_EMBED_KEY` at boot, instantiate `MemoryStore` or `PostgresStore` accordingly, parse the request body, call `substitute_in_messages` before forwarding to the provider, set `x-tt-retrieval-tokens-saved` response header. (est: $1.20) _Shipped 2026-05-28 — middleware now activates when TT_RETRIEVAL_STORE=memory + TT_OPENAI_EMBED_KEY set._
- [ ] [P2] [trackE-postgres-store] rust-crate-builder: Wire `tt_retrieval::store::postgres::PostgresStore` — currently a `todo!()` stub behind the `postgres` feature. Requires SQLx schema for `retrieval_chunks` table. [BLOCKED — needs cloud-repo migration to ship the `retrieval_chunks` table first] (est: $0.60)
- [x] [P2] [fix-gemini-pricing-names-route-suggestions] rust-crate-builder: `crates/preview/src/route_suggestions.rs` references Gemini models `gemini-2-5-flash-lite` / `gemini-2-5-flash` that don't exist in `crates/providers/gemini/src/pricing.rs` (which has `gemini-3.1-flash-lite`, `gemini-3.5-flash`, `gemini-3.1-pro`). Result: Gemini suggestions silently skip; tt-preview only ever suggests Anthropic models. Either rename candidates to match pricing, or extend pricing table. Discovered 2026-05-28 during trackC autopilot iteration. (est: $0.20)
- [x] [P2] [fix-uninlined-format-args-providers] rust-crate-builder: Pre-existing `uninlined_format_args` clippy warnings in `crates/providers/*` block `cargo clippy --workspace -- -D warnings`. Fix the format strings (e.g. `format!("x={}", x)` → `format!("x={x}")`) across all provider crates. Discovered 2026-05-28 — out of scope for trackC but blocks workspace-wide gate. (est: $0.30)
- [x] [P1] [fix-inspect-self-delta] rust-crate-builder: `scripts/tt-inspect-self.sh` currently fails on absolute count of HIGH/CRITICAL findings, not delta-vs-main. Rewrite to (1) run `tt inspect . --output /tmp/curr.json`, (2) checkout main into a worktree and run inspect there for /tmp/base.json, (3) diff and fail only on findings present in curr but not base. Discovered 2026-05-28 during trackD autopilot iteration — 10 pre-existing HIGH findings in sdk-python/sdk-typescript cause every gate to fail. (est: $0.40)
- [x] [P1] [fix-sdk-output-max-tokens] inspect-rule-author: Fix the 10 HIGH `output-no-max-tokens` findings in our own SDKs (sdk-python/tokentrimmer/{__init__,client}.py and sdk-typescript/src/index.ts). Add `max_tokens` defaults appropriate to the thin-wrapper use case (probably 4096 for chat, smaller for embedding). We dogfood our own product; these are real cost regressions. (est: $0.30)
- [x] [P2] [fix-inspect-self-medium] inspect-rule-author: Also fix the 1 MEDIUM `model-flagship-for-extraction` finding in `sdk-typescript/src/index.ts:20`. (est: $0.10)



- [x] [P0] [w7-auth-credentials-middleware] rust-crate-builder: Axum middleware in `crates/core/src/middleware/auth.rs` that resolves `Authorization: Bearer tt_live_*` → org_id + provider credentials → populated `RequestContext`. v1 ships with two pluggable stores behind a trait: an `InMemoryProviderCredentialStore` and an `EnvProviderCredentialStore` (uses `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, etc. from process env so dogfooding works without cloud-repo UI). Postgres-backed swap is `w7-auth-credentials-postgres` (BLOCKED — needs cloud repo). (est: $0.60)
- [x] [P0] [w7-auth-credentials-postgres] rust-crate-builder: Replace stub credential lookup with a Postgres-backed `provider_credentials` table; rows are encrypted with `TT_MASTER_KEY` (XChaCha20-Poly1305). _Shipped 2026-05-27 — `ChainedProviderCredentialStore(Postgres, EnvProviderCredentialStore)` wired in gateway CLI; dashboard /credentials page populates rows._ (est: $0.40)
- [x] [P1] [l1-baseline-cost-schema] rust-crate-builder: L1 cache schema improvement — store original baseline_cost_usd alongside the response bytes so hit responses can report accurate savings instead of the conservative synthetic baseline from cached `Usage` fields. (est: $0.30)
- [x] [P2] [deploy-min-machines-policy] rust-crate-builder: Decide whether `min_machines_running = 1` stays (always-on, ~$1.50/mo idle) or drops to 0 (scale-to-zero, ~30s cold start on first request). Document the tradeoff in ADR. (est: $0.10)

## Six-track expansion (2026-05-28 brainstorm)

Six tracks scoped to evolve TokenTrimmer from "runtime API gateway" → "dev-tool layer + self-driving business". Each track gets its own spec → plan cycle before execution. Items marked `[NEEDS-SPEC]` are placeholders only — do NOT pick via autopilot until a spec lives in `docs/superpowers/specs/` and a plan in `.claude/plans/`.

- [ ] [P1] [trackF-self-driving-backlog] rust-crate-builder: Self-driving autopilot backlog generator. Daily GHA runs `tt backlog generate` → Sentry + tt-api signals + inspect-self + gh-triage + Tier-3 LLM. Three-label scheme: `autopilot` / `autopilot-triaged` / `autopilot-proposed`. Spec: `docs/superpowers/specs/2026-05-28-self-driving-backlog-design.md`. Plan: `docs/superpowers/plans/2026-05-28-self-driving-backlog-generator.md`. (est: ~$3.50) [DEFERRED 2026-05-28 — user manually feeds backlog; no separate API-billed automation. Spec + plan remain on disk for future revival.]
- [x] [P2] [trackA-mcp-server] rust-crate-builder: MCP server exposing TokenTrimmer intelligence (cost preview, route suggestions, semantic cache lookup, inspect findings, plan projections) as tools/resources/prompts to Claude Code, Cursor, and any MCP client. New crate `crates/mcp/`. Spec: `docs/superpowers/specs/2026-05-28-trackA-mcp-server-design.md`. Plan: `docs/superpowers/plans/2026-05-28-trackA-mcp-server.md`. (est: ~$3.50) _Shipped 2026-05-28 — Day-0 MVP (stdio transport, 4 tools, 2 resources)._
- [x] [P2] [trackB-claude-code-codex-proxy] rust-crate-builder: Claude Code / Codex proxy mode. `tt proxy` runs a local OpenAI-/Anthropic-compatible endpoint that forwards to the hosted Gateway, injects cost-preview headers, and writes session-level cost rollups. Spec: `docs/superpowers/specs/2026-05-28-trackB-claude-code-codex-proxy-design.md`. Plan: `docs/superpowers/plans/2026-05-28-trackB-claude-code-codex-proxy.md`. (est: ~$2.50) _Shipped 2026-05-28 — Day-0 MVP (Anthropic + OpenAI native + gateway/bypass/hybrid modes; SSE preserved; session log + Ctrl-C banner)._
- [x] [P1] [trackC-cost-preview-api] rust-crate-builder: `POST /v1/preview` endpoint — accepts a chat-completion request, returns projected cost (current model), cheapest-equivalent (Plan engine quality projection), savings if cached, and suggested route. Reusable surface for MCP (A), proxy (B), CLI (D). Spec: `docs/superpowers/specs/2026-05-28-trackC-cost-preview-api-design.md`. Plan: `docs/superpowers/plans/2026-05-28-trackC-cost-preview-api.md`. (est: ~$2.00) _Shipped 2026-05-28 — Day-0 MVP._
- [x] [P1] [trackD-tt-init-installer] rust-crate-builder: `tt init` subcommand drops `AGENTS.md`, hooks (pre-edit-guard, cost-cap-check, audit-line), `.claude/`, an Inspect baseline run, and cost-cap config into any user repo. Templates in `crates/cli/templates/init/`. Bundles existing assets as a product. Spec: `docs/superpowers/specs/2026-05-28-trackD-tt-init-installer-design.md`. Plan: `docs/superpowers/plans/2026-05-28-trackD-tt-init-installer.md`. (est: ~$2.50) _Shipped 2026-05-28 — Day-0 MVP._
- [x] [P3] [trackE-rag-context-compression] rust-crate-builder: RAG / context-compression pillar. New crate `crates/retrieval/` — corpus ingestion via OpenAI embeddings, HNSW lookup over user codebase + docs, prompt-prefix swap that replaces verbose context with retrieved snippets. Dashboard `/context` page. Distinct from L2 semantic cache (which caches responses; this retrieves context). Spec: `docs/superpowers/specs/2026-05-28-trackE-rag-context-compression-design.md`. Plan: `docs/superpowers/plans/2026-05-28-trackE-rag-context-compression.md`. (est: ~$4.00) _Shipped 2026-05-28 — Day-0 MVP (chunking + embedding + in-memory store + tag parser + substitution + CLI; Postgres store + cloud endpoints + middleware activation deferred to follow-up)._

## Project review follow-ups (2026-05-29)

Items surfaced by the 9-lens project review (`PROJECT_REVIEW.md`). The three top savings-measurement fixes (routing-baseline, anthropic-stream-cached-tokens, proxy-savings-banner) already shipped in `9e6188c`. Public items below are unblocked and autopilot-pickable; design/cloud items are tagged `[CLOUD-REPO]` so the public loop skips them.

### Savings correctness + gateway (public, actionable)

- [x] [P0] [registry-model-passthrough] rust-crate-builder: When `registry.by_model` misses, fall back to `tt_shared::providers::infer_provider(model) -> by_id` so valid unlisted models (newer GPT/Claude/Gemini, o3-mini, OpenRouter/Together/local passthrough) dispatch instead of 404ing. Keep the static table for pricing only. Tests: unknown-but-inferrable model dispatches; truly-unknown still 404s. (est: $0.60)
- [x] [P0] [wire-l2-cache-production] rust-crate-builder: Wire `with_l2` into the `tt gateway` CLI boot behind `TT_L2_SEMANTIC_CACHE=1` (construct embedder + `PostgresL2Cache` from the db pool); add a real per-row `baseline_cost_usd` so L2-hit savings are honest rather than the $1/$2 synthetic. If embedder/db absent, log and skip. Tests: L2 attached when flag+deps present; honest baseline on hit. (est: $1.00)
- [x] [P1] [fix-anthropic-cache-usage-mapping] provider-adapter-author: Normalize Anthropic usage to the OpenAI subset convention so `compute_cost` is correct: set `prompt_tokens = input_tokens + cache_read_input_tokens` (total input incl. cache reads), keep `cached_tokens = cache_read_input_tokens`. Apply to both `translate_usage` and the streaming terminal usage. Add a unit test with cache-read usage asserting cost == provider-billed and baseline == full-rate-over-all-input. (est: $0.40)
- [x] [P1] [fee-multiplier-apply] rust-crate-builder: Apply `CompatConfig::fee_multiplier` (OpenRouter 5% BYOK) to cost/baseline in `compute_cost` — add `fn fee_multiplier(&self)->f64` (default 1.0) to the Provider trait and multiply, or remove the dead field. Test: OpenRouter cost reflects 1.05x. (est: $0.40)
- [x] [P1] [retry-fallback-layer] rust-crate-builder: Wrap provider dispatch in a retry/backoff+fallback policy that consumes `ProviderError::is_retriable` (honor `RateLimited.retry_after_ms`) and `is_fallback_eligible`; streaming retry only before first chunk. Tests: 429 then 200 succeeds; non-retriable surfaces immediately. (est: $0.80)
- [x] [P1] [streaming-client-timeout] rust-crate-builder: Stop the fixed 120s reqwest total timeout from cutting long streams — use a separate streaming client (connect + read-idle timeout) or honor `RequestContext.deadline`; drop the deadline field if unused. Test: long stream past 120s isn't truncated by client timeout. (est: $0.50)
- [x] [P1] [retrieval-orgid-isolation] rust-crate-builder: Replace the hardcoded `org_id = Uuid::nil()` in `crates/core/src/middleware/retrieval.rs` with the real org from the `ApiKeyContext` extension; reject/empty when absent. Add a cross-org isolation test for the retrieval path. (est: $0.40)
- [x] [P2] [anthropic-total-tokens-fix] provider-adapter-author: Count `total_tokens = input + cache_read + cache_creation + output` in Anthropic non-stream + stream usage so totals reflect the full prompt when caching is active. (est: $0.30)
- [x] [P2] [preview-pricing-all-providers] rust-crate-builder: Extend `crates/preview/src/pricing.rs::lookup` to probe all 8 provider pricing tables (Groq/Mistral/Together/OpenRouter/Local), so cost-preview works for routed-to models like llama-3.1-8b-instant. (est: $0.30)
- [x] [P3] [openai-reasoning-stream-unblock] provider-adapter-author: Remove the stale `is_reasoning_model` streaming short-circuit in `crates/providers/openai/src/stream.rs:75-80` (or gate behind a verified per-model flag). (est: $0.20)
- [x] [P3] [kdf-doc-align] rust-crate-builder: Align the credential KDF wording — call it a SHA-256 KDF everywhere (credentials.rs/postgres.rs/SECURITY.md), or switch to real HKDF-SHA256. (est: $0.20)

### Plan engine (public, actionable)

- [x] [P1] [plan-cache-savings-wire] plan-replay-validator: Thread projected L1/L2 cache hits into the cost loop in `crates/plan-core/src/replay.rs` — set a hit request's projected cost to 0 (or the provider cached-prefix discount) before aggregation/bootstrap. Add a test asserting a cache-only diff produces savings > 0. (est: $1.00)
- [x] [P1] [plan-latency-projection] plan-replay-validator: Stop echoing historical latency for rerouted requests — build a per-(provider,model) latency distribution from the window and resample for the proposed model, or mark latency "not projected (insufficient model history)". (est: $1.00)

### Docs (public, actionable)

- [x] [P1] [docs-readme-quickstart-fix] rust-crate-builder: Fix `README.md` quickstart — real Docker image (`ghcr.io/tokentrimmer/tt-cli`, env-only config, no YAML mount), single Rust version (1.88) across README/CONTRIBUTING, populate-or-remove the `examples/` claim, and link `GETTING_STARTED.md`. (est: $0.30)

### Inspect depth (public, actionable)

- [x] [P1] [inspect-5-missing-rules] inspect-rule-author: Implement the 5 documented-but-missing P0 rules (`model-deprecated`, `prompt-bloated-system`, `prompt-verbose-few-shot`, `prompt-no-output-constraint`, `config-agents-md-too-long`) with fixtures; reconcile the catalog's "15" claim. (est: $1.00)
- [ ] [P1] [inspect-corpora-seed] rust-crate-builder: Vendor 5-10 pinned, permissively-licensed OSS LLM samples (LangChain/Vercel-AI/openai-cookbook) into `corpora/`, run `scripts/measure-fp-rate.sh`, record per-rule precision/recall. Unblocks the w24 FP gate. (est: $0.50)
- [ ] [P2] [inspect-ast-migration] inspect-rule-author: Migrate the structural rules (cache_control/max_tokens/model-arg/loop-termination) from regex to the existing tree-sitter harness in `crates/inspect-core/src/parse.rs`; add a rule-level AST cache. (est: $1.20)
- [ ] [P2] [inspect-new-rules] inspect-rule-author: Add `cache-anthropic-tools-not-cached`, `output-n-greater-than-one`, `model-reasoning-effort-default-high`, `prompt-dynamic-prefix-breaks-cache` with fixtures. (est: $0.80)

### Architecture scalability (public, actionable)

- [x] [P1] [pricing-externalize] rust-crate-builder: Move pricing/model catalogs out of Rust source into versioned data (`include_dir` + refresh path, or a Postgres pricing table) with real `effective_at`; decouples the 50-provider catalog from releases and fixes historical replay. (est: $1.50)
- [x] [P2] [token-estimator-shared] rust-crate-builder: Extract a `tt-tokenize` crate (tiktoken) shared by tt-preview, tt-core dispatch, and tt-routing so routing rewrites use the same accurate estimate `/v1/preview` reports; move tiktoken-rs into `[workspace.dependencies]`. (est: $0.60)
- [x] [P2] [compat-crate-split] rust-crate-builder: Split `OpenAICompatibleProvider` into a `tt-provider-compat` crate so Mistral/Groq/Together/OpenRouter stop depending on the full OpenAI adapter; make registry registration config-aware; sort `/v1/models` output. (est: $0.80)
- [x] [P2] [hnsw-org-recall] rust-crate-builder: Fix L2 recall under multi-tenant load — tune `hnsw.ef_search` for the org-filtered query or partition `cache_entries` by org_id; add a recall regression test loading N orgs. (est: $0.80)
- [x] [P3] [workspace-lints-align] rust-crate-builder: Add `[workspace.lints]` to the public workspace mirroring cloud (forbid unsafe, deny the shared clippy set); add a cargo-deny ban keeping axum/hyper out of shared crates; align tokio/uuid pins. (est: $0.30)

### Value visualization (public, actionable)

- [x] [P1] [stream-cost-headers] rust-crate-builder: Emit a terminal `event: tokentrimmer.usage` SSE event carrying cost/baseline/saved before `[DONE]` so streaming clients/SDKs see per-request savings (the dominant client mode currently shows none). (est: $0.60)
- [x] [P2] [live-cli-savings-ticker] rust-crate-builder: Add a rewriting stderr status line to `tt proxy` (gated by `--no-tui`) updating on each request: `tt · N req · $X saved · Y% cached`. (est: $0.50)

### Cost-layer capabilities (public, actionable)

- [x] [P1] [budget-caps-quota] rust-crate-builder: Add a spend/quota enforcement primitive (org-agnostic trait + in-memory impl in public): per-org monthly spend cap + per-minute window, enforced in auth middleware returning 429 + `X-TT-Budget-Remaining`. Foundational for tier caps and a headline "hard spend cap" feature. (est: $1.50)
- [x] [P2] [provider-failover] rust-crate-builder: Extend the route target schema with an ordered fallback chain + per-provider circuit breaker in `crates/routing` (primary -> fallback on 429/5xx). Turns "cost layer" into "cost + reliability layer". (est: $1.20)
- [x] [P2] [cost-diff-ci-lint] rust-crate-builder: Add `tt inspect --cost-diff` (or extend the GitHub Action) estimating the projected per-call cost change of a PR's added/modified LLM calls, posted as a check-run; reuses `crates/preview`, no cloud dependency. (est: $1.00)

### Design + brand uplift (cloud repo — skipped by public autopilot)

- [ ] [P0] [design-system-foundation] astro-page-builder: Real token layer in `cloud/packages/ui/src/styles.css` (color/type-scale/8px-space/radius/shadow/z) + shared `<Layout>`/`<DashboardShell>` owning head/fonts/favicon/nav; migrate all 16 dashboard pages off duplicated inline CSS. [CLOUD-REPO] (est: $2.50)
- [ ] [P0] [marketing-site-build] astro-page-builder: Build `cloud/apps/web` — hero + live savings proof, See/Plan/Optimize features, dashboard preview, drop-in snippet, pricing, social proof, footer; consume the new tokens. Replaces the current placeholder front door. [CLOUD-REPO] (est: $2.50)
- [ ] [P1] [brand-kit] astro-page-builder: Wordmark + mark (SVG), favicon/apple-touch set, typeface pairing (UI grotesk + numeric mono, self-hosted woff2); wire via the shared Layout. [CLOUD-REPO] (est: $1.00)
- [ ] [P1] [dark-mode] astro-page-builder: Light+dark token sets on `[data-theme]` + persisted toggle; theme uPlot charts from tokens. [CLOUD-REPO] (est: $0.80)
- [ ] [P2] [chart-theming] astro-page-builder: Brand-themed uPlot (area fill, semantic-green savings), compact currency axis, styled tooltips, skeleton loaders, shared Chart wrapper component. [CLOUD-REPO] (est: $0.80)
- [ ] [P2] [app-shell-nav] astro-page-builder: Branded sidebar/topbar nav with icons + active-route highlighting, org/user menu, focus rings; styled signin/verify auth screens. [CLOUD-REPO] (est: $0.80)
- [ ] [P3] [docs-site-theme] astro-page-builder: Theme the Starlight docs site to the brand and land the build-time docs content sync. [CLOUD-REPO] (est: $0.50)

### Cloud-side follow-ups (skipped by public autopilot)

- [ ] [P1] [plan-reconciliation-trustscore] rust-crate-builder: Build the projected-vs-actual reconciliation loop feeding the user-facing trust score (calibration math in plan-core; post-window data fetch in cloud). [CLOUD-REPO] (est: $1.50)
- [ ] [P2] [alert-dispatcher-slack] rust-crate-builder: Outbound webhook + Slack sink firing on budget thresholds (50/80/100%), `anomaly.detected` >3σ, reconciliation drift >2%; reuses existing cloud signals. [CLOUD-REPO] (est: $0.80)
- [ ] [P2] [savings-badge] rust-crate-builder: `GET /v1/badges/savings?org_id&expires&sig` SVG via the existing HMAC signed-URL plumbing; "$X saved this month" README/Slack badge + copy snippet on /reports. [CLOUD-REPO] (est: $0.40)
- [ ] [P3] [finops-export] rust-crate-builder: Add a FOCUS-aligned export format to the existing export endpoint for Cloudability/Vantage ingestion. [CLOUD-REPO] (est: $0.50)
- [ ] [P3] [forgone-savings-view] rust-crate-builder: Aggregate preview `suggested_savings_usd` into a "potential additional savings" dashboard card (after the proxy banner fix). [CLOUD-REPO] (est: $0.60)

### Ops / human-gated (not autopilot-pickable)

- [ ] [P0] [cloud-repo-remote] Create the private `TokenTrimmer/cloud` GitHub repo + push the existing cloud checkout; unblocks ~10 P0 launch gates. [BLOCKED — needs human: repo create + push]
- [ ] [P1] [cloud-backlog-sync] Flip the ~9 already-shipped cloud BACKLOG items to `[x]` (reconciliation, hosted-inspect, /inspect, /reports, /settings, anomaly, export, trust-score, stripe-webhooks). [CLOUD-REPO]
- [ ] [P1] [env-secret-split-rotate] Split dev/prod secret sets (prod only in `fly secrets`); rotate the live keys read this session (TT_MASTER_KEY → requires re-encrypting provider_credentials, TT_ADMIN_TOKEN, FLY_DEPLOY_KEY, Stripe). [BLOCKED — needs human: key rotation]

## Completed

- [x] [w0-pre-flight] Harness scaffolding (hooks, agents, Cargo workspace, CI, scripts, governance docs). 2026-05-25.
- [x] [deploy-session] Gateway deployable to Fly.io with Sentry + L1 Redis wired. 2026-05-26.
