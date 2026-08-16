# TokenTrimmer — Senior Review

> **Archived:** This 2026-05-30 review is retained only as historical evidence. It is not a current backlog, launch decision, or source of readiness claims. Current authority is the repository-root `PROJECT_REVIEW_2026-08-11.md`, source, migrations, generated contracts, and active CI/release gates.

_Date: 2026-05-30 · Reviewer: comprehensive multi-agent audit (12 area finders + adversarial verification) with maintainer spot-checks._

**Scope:** both repos — `public/` (Gateway, Inspect, Plan, CLI, MCP, retrieval, SDKs) and the sibling `cloud/` (hosted API, worker, dashboard, marketing, docs).

**Method:** 12 parallel finder agents across the 7 review areas, with adversarial verification of every high/critical bug/security/legal finding, plus direct reading of the load-bearing files. Severities below are **post-verification** — where the verification pass downgraded a finder's claim, the original is noted in parentheses. File:line citations are repo-qualified (`public/…`, `cloud/…`).

**Overall posture.** A genuinely well-engineered early product. The crypto primitives (Argon2 keys, XChaCha20-Poly1305 credential encryption, Ed25519 audit chain), the statistical core (seeded ChaCha8 replay, bootstrap CIs), the pricing-catalog/versioning design, the design *tokens*, and the *restraint* in marketing copy are all above the bar for a pre-alpha. The problems concentrate in three places: **the four products aren't wired into one loop**, **the semantic cache and streaming telemetry can silently produce wrong numbers/answers**, and **the runtime doesn't yet enforce the tiers/limits the business model sells.**

---

## 1. Product completeness & coherence

**State:** All four products exist as real, separately-tested code and are individually surprisingly complete. The seams between them are broken at *every* join — the headline "inspect → simulate → apply → report" loop does not close.

| # | Finding | Sev | Effort | Evidence |
|---|---------|-----|--------|----------|
| 1.1 | **Plan "apply" never writes Gateway routing config — the core loop is a no-op.** `apply_plan` only flips `status='applied'` + writes an audit row; `proposed_routes` are discarded and never inserted into the `routes` table the Gateway reads. | **Critical** | M | `public/crates/plan-core/src/apply.rs:162-197`; `cloud/crates/api/src/plan.rs:212-253`; routes consumed at `public/crates/cli/src/main.rs:704-708` |
| 1.2 | **Dashboard "Apply plan" button POSTs to a route that doesn't exist** → guaranteed 404 on the product's headline CTA. The tt-api endpoint exists but the dashboard never proxies to it, and the in-page help text blames an already-resolved blocker. | **Critical** (finder high) | S | `cloud/apps/dashboard/src/pages/plan/[id].astro:137`; no `api/plans/` dir exists |
| 1.3 | **Paid Tier 2/3 Inspect ruleset is an empty `Vec`** — a paying hosted scan returns byte-identical findings to the free `tt inspect .`. No premium Inspect value ships today. | High | L | `cloud/crates/inspect-rules-tier23/src/lib.rs:24-26` |
| 1.4 | **Inspect never feeds Plan.** Findings carry no cost/route data; nothing converts them (or `preview`'s `RouteSuggestion`, which already computes target_model + savings) into `proposed_routes`. Users hand-author `plan-input.json`. | High | M | `public/crates/preview/src/route_suggestions.rs:41-53`; CLI reads pre-written JSON only `main.rs:924-947` |
| 1.5 | **Reporting reflects only Gateway.** Digest + PDF query only `request_logs` — no Inspect findings, no Plan trust-scores, no anomalies, despite the worker doc claiming the digest carries "open Inspect findings." | High | M | `cloud/crates/api/src/digest.rs`; `cloud/crates/worker/src/pdf.rs:121-171`; `worker/src/lib.rs:5-7` |
| 1.6 | `tt-worker` is a hollow library shell (1-line stubs, no `main.rs`); real scheduling lives in `tt-api` in-process loops. `lib.rs` misrepresents the architecture. | Medium | S | `cloud/crates/worker/src/*`; real loops `cloud/crates/api/src/main.rs:270-341` |
| 1.7 | MCP `find_route_for` returns hardcoded `claude-haiku-4-5` in every branch while its tool description claims "historical… HIGH quality confidence" data. | Medium | M | `public/crates/mcp/src/tools/find_route_for.rs:23,35-48` |
| 1.8 | `tt plan --apply` is a stub that prints a notice and silently runs projection-only. | Medium | S | `public/crates/cli/src/main.rs:67-70,934-939` |
| 1.9 | `plan_runs.config_diff` stores the `PlanResult` (output), not `proposed_routes` (input) — so even once 1.1 is built, apply has nothing to replay from. | Medium | S | `cloud/crates/api/src/plan.rs:86-87,122` |
| 1.10 | `tt_routing::RouteAction` (`fallbacks`) and `tt_plan_core::RouteAction` (`force_cache_layer`) have diverged despite both docs claiming "lockstep" — projections won't match live behavior. | Low | M | `public/crates/routing/src/lib.rs:68-81`; `public/crates/plan-core/src/types.rs:130-141` |

**Connecting insight:** 1.1/1.2/1.8/1.9/1.10 are the *same hole* viewed five ways — the apply path is unimplemented top to bottom, yet the audit log emits a `plan.applied` event asserting a change that never happened. Closing this loop (translate `proposed_routes` → org-scoped `routes` rows in the same txn as the status flip; persist input at create-time; add the dashboard route; make `--apply` real) is the single highest-leverage product change.

---

## 2. Cost-reduction effectiveness (core value prop)

**State:** Architecturally sound on the happy path (per-org-namespaced L1, tuned HNSW L2, versioned pricing catalog, circuit-breaker + bounded retry). But the **semantic cache can silently return wrong answers** and **streaming cost telemetry is systematically inaccurate**.

### Caching — quality-correctness cluster
| # | Finding | Sev | Effort | Evidence |
|---|---------|-----|--------|----------|
| 2.1 | **L2 lookup ignores the requested model.** SQL filters only org_id + expiry + cosine; a `gpt-4o` request can be served a `gpt-4o-mini` (or any near-embedding) response, attributed to the wrong model. Invisible to the caller. | **Critical** | **S** | `public/crates/cache/src/l2.rs:356-367`; `chat.rs:335-336` |
| 2.2 | **Caches & replays non-deterministic responses** (temperature>0, n>1, tool calls) for 24h with no opt-out. `n`/`seed` excluded from the key, so an `n=3` request can get a 1-choice cached body. | **Critical** | M | `public/crates/cache/src/key.rs:5-7,73-85`; `chat.rs:295-330` |
| 2.3 | **L2 semantic key embeds only the *last user message*** — ignores system prompt, history, tools. Different conversations sharing a final turn collide above threshold and return each other's answers. | **Critical** | M | `chat.rs:333-336,438-456,567-580` |
| 2.4 | **RAG substitution applies no similarity floor** in production — the `top_k(min_similarity)` helper exists but is unused, so off-topic chunks get spliced into prompts. | High | S | `public/crates/retrieval/src/substitute.rs:54-66`; unused `search.rs:9-22` |
| 2.5 | **L2 mixes embedding spaces** — no `embedding_model`/version column; the documented embedder swap would corrupt similarity. | High | M | `migrations/0002_cache_entries.up.sql:17-27`; `l2.rs:356-367` |
| 2.6 | **No cache-stampede / single-flight** — a cold/expiring popular key lets N concurrent identical requests all miss and hit the provider. | High | M | `chat.rs:303-382` |
| 2.7 | **`tt_extras` advertises per-request "cache config" but no bypass/refresh/no-cache is wired** — no escape hatch, which makes 2.2 unavoidable for mixed traffic. | High | S | `public/crates/shared/src/messages.rs:42-45` |
| 2.8 | Flat 24h TTL for L1+L2; documented per-tier (24h/7d/30d) and per-route TTLs never implemented → paid tiers lose hit-rate. | Medium | M | `state.rs:22-24`; `chat.rs:47-50,720` |
| 2.9 | L2-hit "savings" computed from a hardcoded **$1/$2-per-M placeholder**, not real model price → fabricated numbers feed dashboard/badge/PDF. | Medium | S | `chat.rs:676-683,596,905` |
| 2.10 | Streaming requests never populate the cache (read-on-stream, write-on-non-stream-only) → stream-default workloads get near-zero cache benefit. | Low | M | `chat.rs:168-294` vs `415-457` |

### Routing, fallback, telemetry, compression
| # | Finding | Sev | Effort | Evidence |
|---|---------|-----|--------|----------|
| 2.11 | **Routing rewrites & failover have no capability/context-window quality floor** — a vision/tool/long-context request can be silently downgraded to an incompatible/smaller model. `ModelInfo.capabilities`/`max_input_tokens` exist but are never consulted. | High | M | `public/crates/routing/src/lib.rs:135-161`; `chat.rs:983-987`; `failover.rs:107-128` |
| 2.12 | **Streaming output tokens fall back to raw byte count (~4× overcount)** when no usage block arrives (incl. client abort). (Finder high → verified **medium**: abort-only, bounded, self-correcting.) | Medium | S | `public/crates/core/src/routes/sse.rs:84-98,112-116,356-372` |
| 2.13 | **OpenRouter 5% BYOK fee applied only on the non-streaming path** → every OpenRouter *stream* under-reports cost ~5%. | High | S | `sse.rs:455-476` vs `chat.rs:396-401`; `openrouter/src/lib.rs:49` |
| 2.14 | `request_logs.truncated` is computed/set but **has no DB column** — silently dropped; the rows most likely to be byte-inflated can't be excluded. | Medium | S | `request_logs.rs:52-53,134-167`; `migrations/0001` |
| 2.15 | Streaming input-token estimate uses `s.len()` (bytes) not `tt_tokenize` → overcounts CJK/emoji 2-4× and skews routes. | Medium | S | `chat.rs:201-240` vs `983-984` |
| 2.16 | **No Batch API support** — the 50%-off OpenAI/Anthropic batch discount is absent; the routing `tag` predicate is the natural hook. | Medium | L | `routing/src/lib.rs:62-64` |
| 2.17 | RAG savings ignore embedding-call cost and can report a "saving" when substitution *grows* the prompt. | Medium | M | `substitute.rs:63,71-75`; `embed.rs:27` |
| 2.18 | Anthropic cache-creation tokens billed at base input rate, not the ~1.25× write premium → understates cost. | Low | M | `anthropic/src/stream.rs:551-558`; `chat.rs:748-758` |
| 2.19 | Pricing catalog is a manual 2026-05-01 snapshot; "refreshed daily" docstrings have no automation; unknown model → priced at **$0**. | Low | M | `public/crates/shared/data/pricing.toml:25-27`; `chat.rs:744-746` |

**Connecting insight:** 2.12/2.13/2.14/2.15 + 2.9 are one theme — **realized-savings numbers are unreliable for streaming, OpenRouter, and truncated traffic**, and feed the badge/digest/PDF, which creates the legal exposure in §6.

---

## 3. UX/UI & design system

**State:** A genuinely good **token** foundation (`packages/ui/src/styles.css`). But "@tokentrimmer/ui" is a *stylesheet, not a component library* — every page hand-rolls buttons/inputs/modals/badges, and ~65 hardcoded hex values break dark mode.

| # | Finding | Sev | Effort | Evidence |
|---|---------|-----|--------|----------|
| 3.1 | **`@tokentrimmer/ui` ships zero components** despite `package.json` advertising "buttons, forms, charts, table primitives." Root cause of all duplication/drift. | High | L | `cloud/packages/ui/src/` = `styles.css` + `index.ts` (fmt helpers only); `package.json:5` |
| 3.2 | **Dark mode silently breaks on 11/16 pages** via hardcoded hex — most damaging: the **API-key reveal renders white-on-white** (`#fffbe6`/`#fff`). | High | M | `keys/index.astro:162-171`; `routes/index.astro:372-375`; `audit/index.astro:293,313` |
| 3.3 | **Three divergent token systems for one brand** (dashboard `--tt-*`, marketing private `--bg/--accent`, docs hand-"mirrored"). Visible drift (Hanken Grotesk vs system font; `#8a6d00` vs `#9a7b00`). | High | M | `apps/web/src/layouts/Layout.astro:94-108`; `apps/docs/src/styles/brand.css:5` |
| 3.4 | Modals hand-rolled per page, no `role=dialog`/`aria-modal`/focus-trap/Escape; destructive actions use native `confirm()`/`alert()`. | Medium | M | `routes/index.astro:377-398,327`; `keys/index.astro` |
| 3.5 | Severity/event/status badges duplicated with conflicting hex while the token-based `.tt-chip` goes unused. | Medium | S | `inspect/index.astro:212-214`; `audit/index.astro:301-307`; `styles.css:770-792` |
| 3.6 | Spacing/radius/type scales bypassed by hand-typed `0.3rem`/`0.375rem`/`0.85em` literals. | Medium | M | `routes/index.astro:352`; `inspect/index.astro:191`; `audit/index.astro:289,297` |
| 3.7 | Minimal elevation/motion; no empty/loading/error language beyond charts; inline-SVG icons as raw strings; unverified WCAG contrast (`--tt-text-faint` ~2.5:1). | Low | M | `styles.css:104,546-551`; `DashboardLayout.astro:45-58` |

**Path to premium:** extract `tokens.css` consumed by all three apps; promote charts + build ~10 primitives (Button/Field/Select/Modal/Badge/Banner/StatCard/DataTable/Toast/EmptyState) into `packages/ui`; replace every hardcoded hex with tokens (CI stylelint); add motion tokens + press/hover states; bring brand fonts to web/docs.

---

## 4. Bugs & correctness

| # | Finding | Sev | Effort | Evidence |
|---|---------|-----|--------|----------|
| 4.1 | **All SSE parsers split only on `\n\n` and demand literal `data: `** — a CRLF/no-space compat upstream (vLLM/Ollama, reachable via customer `base_url`) buffers the whole response in memory and flushes at EOF. | Medium | M | `compat/src/stream.rs:241,269-271`; `anthropic/stream.rs:312-314,582-584`; `gemini/stream.rs:223,376-378` |
| 4.2 | **Retry nested inside failover, no jitter** — primary + 2 fallbacks all 5xx → up to **9 upstream POSTs** for one request, synchronized backoff. | Medium | M | `failover.rs:107-129`; `retry.rs:31-49` |
| 4.3 | **`is_fallback_eligible` matches `ProviderUpstream` for all statuses** — a deterministic 400/403/422 is retried against every fallback. | Medium | S | `public/crates/shared/src/error.rs:52-59`; `failover.rs:122-127` |
| 4.4 | **Schedulers (digest/pdf/overage/reconciliation) are in-process loops with no leader election** — multi-instance/deploy overlap double-sends emails. (Finder high → verified **medium**.) | Medium | M | `cloud/crates/api/src/main.rs:281-347` |
| 4.5 | **Overage idempotency key can double-bill** when Stripe succeeds but the watermark UPDATE fails and usage then grows. (Finder high → verified **medium**.) | Medium | M | `cloud/crates/api/src/overage.rs:157-181`; `stripe_client.rs:124-150` |
| 4.6 | **Inspect walker prunes all hidden dirs**, so the Critical secret-scan rule can never read `.cursor/rules/*.md`. | High | S | `public/crates/inspect-core/src/walk.rs:63`; `config_agents_md_contains_secrets.rs:34` |
| 4.7 | **`agent-no-termination-condition` whole-file substring matching** — broad `budget`/`timeout` terms suppress genuine runaway loops. (Finder high → verified **medium**.) | Medium | M | `agent_no_termination_condition.rs:60-61,91,110` |
| 4.8 | **Dashboard false "audit chain broken" alarm** on every paged view / any org >200 entries. (Finder high → verified **medium**.) | Medium | S | `cloud/apps/dashboard/src/lib/audit.ts:102-109`; `audit/index.astro:34,76-81` |
| 4.9 | Spend-anomaly detector matches local-TZ top-of-hour against UTC buckets → never fires unless process+DB both UTC. | Medium | S | `cloud/apps/dashboard/src/lib/anomaly.ts:58-67` |
| 4.10 | Anomaly leave-one-out raises the real min bucket size to 4 (doc says 3) → no anomaly until ~28 days of per-hour data. | Medium | S | `cloud/crates/worker/src/anomaly.rs:46-49,77-83` |
| 4.11 | L2 poisoning-candidate count is **summed across every threshold** → inflates the user-facing metric up to N×. | Medium | S | `public/crates/plan-core/src/l2_projection.rs:104-108` |
| 4.12 | Replay equal-priority routes resolve by input array order (no tiebreak) → identical configs yield different projected savings. | Medium | S | `public/crates/plan-core/src/replay.rs:35-36`; `routing.rs:12-16` |
| 4.13 | Plan reconciliation attributes **all** of an org's window traffic to one applied plan → trust-score becomes noise. | Medium | M | `cloud/crates/api/src/reconciliation.rs:105-134,169-173` |
| 4.14 | API-key lookup prefix = 16 bits; UNIQUE column, no retry → issuance fails (~50% at 256 keys) with an opaque error. | Medium | S | `public/crates/auth/src/keys.rs:31,291`; `migrations/0001_cloud_schema.up.sql:89` |
| 4.15 | DB-reading dashboard routes have no try/catch (Neon cold-start → unhandled 500 to polling islands, no backoff); no explicit body-size limit (axum 2MB default vs "oversized prompt" target); `/v1/embeddings` is a hard 404 stub; cosine NaN ordering can mask a valid L2 hit. | Low | S–M | `audit/since.ts:26`; `server.rs:36-73`; `embeddings.rs:11-16`; `l2.rs:208-218` |

---

## 5. Security

**State:** Strong primitives (Argon2 keys with no oracle, AEAD credentials with per-row derived keys + AAD, Ed25519 audit chain, correct constant-time Stripe HMAC verifier, parameterized SQL, **`request_logs` stores no prompt/response content**). The systemic risk is the **dashboard↔tt-api trust model** and an **SSRF primitive**.

| # | Finding | Sev | Effort | Evidence |
|---|---------|-----|--------|----------|
| 5.1 | **tt-api `/v1/admin/*` trusts caller-supplied `org_id` with no membership check** — tenant isolation rests entirely on the dashboard. Several handlers are `WHERE id = $1` only. Token leak / 2nd caller → cross-tenant read/write. | **High** | M | `cloud/crates/api/src/admin.rs:135-187,205-215,278-301`; `routes_admin.rs:183-200,242-263,280-303`; `inspect_hosted.rs:480-501` |
| 5.2 | **Customer-controlled `base_url` + `extra_headers` = authenticated SSRF + header injection.** Any `credentials.write` user can target `169.254.169.254`/internal hosts; `extra_headers` appended *after* `Authorization`. No private-IP/scheme/host validation. | **High** | M | `auth/src/credentials.rs:24-26`; `admin.rs:320-323,368-372`; `compat/src/compat.rs:160-175` |
| 5.3 | **Single shared `TT_ADMIN_TOKEN`** is the only thing between the internet and full multi-tenant admin; long-lived, in dashboard env, no rotation/scoping/rate-limit. Compounds 5.1. | Medium | M–L | `admin.rs:41-99`; `api-client.ts:5-7,29,41`; `docs/SECRETS.md:39` |
| 5.4 | **Gateway runs Argon2 verify on every request** with no cache — `lib.rs:4` claims a 60s Redis cache that doesn't exist. Blows sub-30ms target; CPU-exhaustion lever. | Medium | M | `public/crates/core/src/middleware/auth.rs:68`; `auth/src/lib.rs:4`; `keys.rs:273` |
| 5.5 | **No CSRF/Origin check** on cookie-auth mutating routes; **sessions never rotate**, fixed 30-day TTL; **no HSTS/CSP/Referrer-Policy** (magic-link token in URL → Referer/log leak). | Medium | S–M | `astro.config.mjs:15-20`; `session.ts:18,26,84-95,135-143`; `auth/callback.ts:19-28` |
| 5.6 | **Magic-link signin has no rate limit** (Turnstile optional/off by default) → mail-bomb + unbounded `verification_tokens` growth. | Medium | M | `api/auth/signin.ts:44-89`; `turnstile.ts:45-48` |
| 5.7 | Retrieval middleware buffers the whole body (1 MiB) on every chat POST when enabled + clones plaintext prompt to a detached task; **falls back to shared `Uuid::nil()` namespace** if `ApiKeyContext` absent (cross-tenant retrieval if layered ahead of auth). | Medium | S–M | `middleware/retrieval.rs:145,253-265,208-218` |
| 5.8 | Stripe webhook has **no event-id idempotency**. (Finder high → verified **low**: dispatched writes are idempotent absolute-state; overage dedups separately.) | Low | M | `cloud/crates/api/src/routes.rs:69-128,259-277` |
| 5.9 | **Dependency CVEs** (`rsa` Marvin, 4× `rustls-webpki` 0.102.8). (Finder high → verified **low**: `rsa` not compiled; `rustls-webpki` 0.102.8 sentry-only, real TLS uses patched 0.103.13; public CI runs cargo-deny.) **Real residual: `cloud/` has no CI/deny.toml — zero advisory gating.** | Low | M | `cargo-audit` both lockfiles; `public/.github/workflows/ci.yml:76-96`; cloud has none |
| 5.10 | Env-credential fallback can forward one shared key for all orgs if mis-deployed; webhook returns raw SQL error strings; L2 stores responses unencrypted; GitHub OAuth no PKCE/`__Host-`; `revoke_key`/`issue` mutation+audit not atomic. | Low–Info | S–M | `credentials.rs:205-265`; `routes.rs:110,240,311`; `0002_cache_entries.up.sql:4-6`; `auth/github.ts:15-23` |

**Connecting insight:** 5.1 + 5.2 + 5.3 compound — arbitrary `org_id` + arbitrary `base_url` behind one shared token = SSRF-as-any-tenant if the token leaks. Cheapest durable fix: (a) `WHERE id AND org_id` on every admin handler, (b) validate `base_url` (private-IP/metadata denylist) + header denylist, (c) network-isolate `/v1/admin/*` and move toward short-lived org-scoped assertions.

---

## 6. Legal & messaging safety

**State:** Unusually restrained — **no "save X%" promises, no "guaranteed savings,"** SLA language already scrubbed. Exposure concentrates in *unqualified savings dollar figures* and a few absolutes/contradictions.

| # | Finding | Sev | Effort | Evidence |
|---|---------|-----|--------|----------|
| 6.1 | **Every "Saved $X" surface (dashboard, digest, monthly PDF, public badge, SDK) shows a hard figure from a *synthetic* baseline with cache hits booked as 100% savings — no methodology disclosure.** PDF footer "figures reconciled against metered request logs" over-implies provider-invoice reconciliation. Badge is built for public embedding. (Finder high → verified **medium**: disclosure gap, additive-copy fix.) | Medium | S | `worker/src/pdf.rs:170-177`; `savings_badge.rs:168`; `api/src/digest.rs:139`; baseline `chat.rs:597,760-765` |
| 6.2 | **Free-tier quota contradicts itself: "5,000" (home + blog) vs "10,000" (pricing).** Verified truth is **10,000** (`tier.rs:67-74`). (Direction is customer-favorable → finder high → verified **medium**.) | Medium | S | `index.astro:57`; `blog/introducing-tokentrimmer.astro:74`; `pricing.astro:18`; `tier.rs:67-74` |
| 6.3 | Headline implies savings as an **outcome**: "same requests, smaller invoice," "watch the savings line move," "Stop overpaying." Implied-warranty-of-result trap. | Medium | S | `index.astro:50-51,86-89`; `blog…:75` |
| 6.4 | Absolutes: **"Cache hits never touch a provider" / "served for free"** — an L2 hit is a *similarity* match and still consumes embedding spend. | Medium | S | `index.astro:22`; `blog…:46-47`; `pricing.astro:103` |
| 6.5 | **"You're never cut off"** — unconditional commitment with no fair-use/abuse carve-out, next to "unlimited volume" on Scale. | Medium | S | `pricing.astro:95,79-80,177-178` |
| 6.6 | Scale markets **"signed SLO PDFs" / "Externally-verifiable logs"** while the FAQ disclaims any SLA and the product is "pre-alpha" — internally contradictory. | Medium | M | `pricing.astro:81-82` vs `:111`; `README.md:6` |
| 6.7 | "Cost analytics you can trust" frames an *estimate* as truth; PDF "reconciled" footer overstates verification; pre-alpha status not surfaced on paid/marketing surfaces. | Low | S | `index.astro:29-30`; `pdf.rs:176-178`; `README.md:6` |

**Recommended fix:** ship a public "How we calculate savings" page and link one consistent methodology line from every savings surface. Keep the numbers, qualify the claim; lean into the trust-score + reconciliation as the credibility story.

---

## 7. Self-serve business model

**State:** Well-designed and correctly **rejects an enterprise/contact-sales tier** — Checkout (monthly/annual, flat + metered overage), Portal (upgrade/downgrade/cancel/card), webhook-mirrored state, daily idempotent overage sweep, auto-downgrade via `effective_tier`, SCIM deferred. But two pillars aren't wired, and SAML is sold-not-built.

| # | Finding | Sev | Effort | Evidence |
|---|---------|-----|--------|----------|
| 7.1 | **Tier limits never enforced at request time.** `budget.rs` is USD-only and reads no subscription tier; only `max_keys`/`max_credentials` are checked. Free's 10K/mo hard-stop, 60 rpm, and L2 gating are decorative; a canceled/downgraded org keeps serving. | **Critical** | L | `public/crates/core/src/budget.rs:30-54`; `tier.rs:58,67-74` |
| 7.2 | **No failed-payment/dunning handling; unbounded `past_due` grace.** `invoice.payment_failed` is NoOp; `effective_tier` treats `past_due` as fully paying forever; overage sweep keeps billing it → bad debt + manual ops. | High | M | `billing.rs:355-358`; `tier.rs:84-90`; `overage.rs:123-131` |
| 7.3 | **SSO advertised on Team but unimplemented and ungated.** Auth is magic-link + GitHub OAuth only; RBAC isn't tier-gated; SAML routed to manual "email us." Your bar (SAML self-serve on top tiers) is met only in copy. | High | L | `pricing.astro:62,178`; `cloud/SECURITY.md:79` |
| 7.4 | Checkout "Initialize" **hardcodes `tier='pro'`** regardless of purchased tier → wrong limits in the gap once 7.1 is wired. | Medium | S | `routes.rs:197-207`; tier known at `admin.rs:465` |
| 7.5 | An **`'enterprise'` tier is half-wired** (`tier.rs:58`, `overage.rs:38,128`) with no Stripe price/checkout — dead code that could grant Scale-level entitlement against an unpaid base. | Medium | S | `tier.rs:58`; `overage.rs:38,128`; `main.rs:184-189` |
| 7.6 | **No self-serve budget-cap UI** despite the FAQ promising "Set a budget cap and we'll stop." `BudgetEnforcer` supports `monthly_cap_usd` but nothing writes it → bill-shock risk + a "contact us" motion. | Medium | M | `pricing.astro:95`; `budget.rs:32-33,150`; `settings/index.astro:19-36` |
| 7.7 | Overage sweep + reconciliation are 24h in-process loops (no leader election; non-transactional read→report→watermark) → multi-instance double-count race. Webhook not idempotent / no ordering guard. | Medium | M | `overage.rs:117-200`; `reconciliation.rs:200`; `routes.rs:259-277` |
| 7.8 | Provider-invoice reconciliation is operator-manual by design; annual checkout 400s opaquely if annual price envs are unset (Annual UI shown unconditionally). | Low | M | `invoice_recon.rs:1-9,23`; `main.rs:207-228,336`; `admin.rs:460-467` |

---

## Top 10 highest-leverage changes

1. **Close the Plan→Gateway apply loop** (§1.1/1.2/1.8/1.9). *This is the product.* — M
2. **Make the L2 semantic cache safe** (§2.1/2.2/2.3/2.7): model filter (one line), gate non-deterministic caching, embed full context, wire `tt_extras` bypass. — S–M
3. **Enforce tier limits at request time** (§7.1). — L
4. **Fix streaming cost telemetry** (§2.12/2.13/2.14/2.15, §2.9): tokenize not byte-count, apply OpenRouter fee on stream, persist `truncated`. — M
5. **Add routing/failover capability + context-window quality floor** (§2.11). — M
6. **Server-side org-scoping on tt-api admin endpoints** (§5.1): `WHERE id AND org_id`. — M
7. **SSRF guard on `base_url` + header denylist** (§5.2). — M
8. **Dunning + bounded `past_due` grace** (§7.2). — M
9. **Build `@tokentrimmer/ui` component layer + kill dark-mode hex** (§3.1/3.2/3.3). — L
10. **Verified-key cache in front of Argon2** (§5.4) + **savings-methodology disclosure** (§6.1). — M / S

## Quick wins (≤ ~1 day each)

- **§2.1** — `AND model = $N` on the L2 lookup. *Stops wrong-model answers.*
- **§6.2 / copy** — fix Free-tier "5,000"→"10,000" (home+blog); standardize gateway base URL (`api.tokentrimmer.com`, never `fly.dev`).
- **§6.1** — one-line savings-methodology disclaimer on PDF footer, digest footer, badge alt-text, dashboard Saved card.
- **§5.2-partial** — send the Gemini key in `x-goog-api-key` header, not the URL (`gemini/stream.rs:71`).
- **§4.3** — guard `is_fallback_eligible` on `status >= 500`.
- **§3.2 / UX** — remove API-key reveal's 6s auto-reload, add Copy button, detokenize the white-on-white panel.
- **UX** — add `$` to digest subject + a `List-Unsubscribe` header.
- **§1.2/§1.8** — relabel Apply button "Mark as applied (record only)" / hide it; make `tt plan --apply` exit non-zero until wired.
- **§4.14** — widen key display prefix to ~12 hex chars (or retry on unique-violation).
- **§7.5** — delete the dead `'enterprise'` tier branch.
- **§4.11** — report L2 poisoning per-threshold, not summed.
- **§4.12** — deterministic tiebreak on equal-priority route sorting.
- **§5.9-residual** — add `deny.toml` + cargo-deny CI to the **cloud** repo; set `advisories = "deny"`.
- **§4.15** — explicit `DefaultBodyLimit`; return 501 (not 404) from `/v1/embeddings` stub.

## What's genuinely strong

Crypto (Argon2 keys with no oracle, AEAD credentials with per-row derived keys + AAD, Ed25519 audit chain), the Stripe HMAC verifier, seeded-deterministic replay + bootstrap CIs, the versioned pricing catalog with historical replay, the design-*token* layer and token-aware charts, `request_logs` storing **no** prompt/response content, and the restrained marketing copy are all above the pre-alpha bar. The work here is mostly *connecting* and *enforcing* what already exists, not rebuilding.
