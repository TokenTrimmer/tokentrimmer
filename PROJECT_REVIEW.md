# TokenTrimmer — Project Review & Path to Ideal State

_Reviewed 2026-05-29 · scope: both repos (`public/` OSS core + `cloud/` hosted) · method: 9-lens parallel audit (architecture, savings-proof, providers/gateway, inspect/plan, design, value-visualization, onboarding, backlog, security). Every headline claim below was independently re-verified against source (see Appendix)._

---

## 1. Verdict (TL;DR)

TokenTrimmer is **further along and better engineered than its launch surface suggests.** The Rust core is genuinely good: clean acyclic crate layering, a sound Provider abstraction, a tamper-evident audit log (BLAKE3 chain + Ed25519), strong per-tenant credential crypto (XChaCha20-Poly1305 + per-row derived keys), a statistically honest Plan replay engine with real bootstrap CIs, and a thoughtful streaming/partial-cost model. 110+ backlog items have shipped.

But there is **one systemic gap that strikes directly at the mission**, and it is the most important takeaway of this review:

> **TokenTrimmer delivers more savings than it can currently prove or show.**
> Exact-cache savings are real and measured. Provider prompt-cache savings are real and measured for OpenAI. **Model-routing savings are real on the customer's provider bill but report as ~$0** because the baseline is priced against the cheaper routed-to model. The **L2 semantic cache is fully built and tested but never wired into any production gateway** (zero savings in prod). Anthropic prompt-cache savings are **mis-measured (non-stream) and zeroed (stream).**

The second-most-important gap is **presentation**: the marketing site is a literal placeholder (`<h1>` + `<p>`), and the dashboard — while functionally rich — has **no design system** (empty token package, 16 pages of duplicated inline CSS, no brand, no dark mode). This is the largest distance between the product's premium ambition and what a prospect actually sees.

**Neither gap is hard to close.** The savings-measurement fixes are mostly small and surgical (the highest-leverage one is a few lines in `chat.rs`). The design uplift is well-scoped (tokens → shared Layout → marketing build). Closing both is the difference between "a good gateway" and "a product that visibly, provably saves money the moment you use it."

---

## Implementation status — updated 2026-05-29

Since this review was written, **27 of its items have shipped** — all inline, each test-driven, with `cargo clippy --workspace -- -D warnings` green at every step. Crucially, the savings-**measurement** gaps that were the headline concern (§2) are now closed, the L2 cache + retry/failover resilience are live, and the per-model rate catalog is externalized to versioned data.

**✅ Shipped this session**
- _Savings correctness:_ `fix-routing-baseline-savings` (routing `saved_usd` now correct), `fix-anthropic-cache-usage-mapping` + `fix-anthropic-stream-cached-tokens` + `anthropic-total-tokens-fix`, `fee-multiplier-apply` (OpenRouter 5% BYOK), `plan-cache-savings-wire`, `plan-latency-projection`
- _Gateway:_ `registry-model-passthrough` (unlisted models dispatch, no 404), `streaming-client-timeout` (read-idle), `openai-reasoning-stream-unblock`, `retry-fallback-layer` (bounded retry/backoff on transient 429/5xx), `wire-l2-cache-production` (L2 semantic cache live behind `TT_L2_SEMANTIC_CACHE=1`), `provider-failover` (per-provider circuit breaker + ordered fallback chain on a route's `fallbacks`; non-stream dispatch fails over on 5xx/timeout)
- _Moment-of-use:_ `fix-proxy-savings-banner`, `stream-cost-headers` (terminal `tokentrimmer.usage` event), `live-cli-savings-ticker`
- _Headline feature:_ **`budget-caps-quota`** — per-org monthly spend cap + per-minute rate, enforced in auth middleware → 429 + `X-TT-Budget-Remaining-Usd`
- _Inspect / quality:_ `inspect-5-missing-rules` (15/15 P0 rules now ship), `preview-pricing-all-providers`, `retrieval-orgid-isolation`
- _Docs / hygiene:_ `getting-started-guide` (`GETTING_STARTED.md`), `docs-readme-quickstart-fix`, `kdf-doc-align`, `workspace-lints-align`, `perms-least-privilege-cleanup`, `.gitignore` hardening
- _Architecture:_ `pricing-externalize` — per-model rates moved to a versioned, embedded `data/pricing.toml` (real `effective_at` + price history; live + historical-replay lookups); all 7 paid providers delegate to the shared catalog
- _CI / shift-left:_ `cost-diff-ci-lint` — `tt inspect --cost-diff [--base <ref>] [--fail-on-cost-increase]` prices LLM model ids added/removed in a `git diff`, reusing the pricing catalog (no cloud); markdown/JSON for a check-run
- _Layering:_ `compat-crate-split` — extracted `tt-provider-compat` (OpenAI-wire machinery); the 4 BYOK adapters no longer depend on the full OpenAI crate; config-aware registration (`TT_PROVIDERS` allowlist) + sorted `/v1/models`
- _Accuracy:_ `token-estimator-shared` — new `tt-tokenize` crate (cached tiktoken + heuristic); preview and the live routing estimate share it, so route thresholds match what `/v1/preview` reports
- _Ops:_ `cloud-repo-remote` — cloud monorepo baselined + pushed to private `TokenTrimmer/cloud`

**◻ Remaining — public, doable:** `hnsw-org-recall`, `inspect-new-rules`, `inspect-ast-migration`, `inspect-corpora-seed`

**◻ Remaining — cloud repo (design + reporting):** `design-system-foundation`, `marketing-site-build`, `brand-kit`, `dark-mode`, `chart-theming`, `app-shell-nav`, `docs-site-theme`, `savings-badge`, `alert-dispatcher-slack`, `finops-export`, `forgone-savings-view`, `plan-reconciliation-trustscore`, `cloud-backlog-sync`

**◻ Remaining — human-gated:** `env-secret-split-rotate` (rotate the live keys read this session; prod secrets → `fly secrets`)

The §6 / §7 checklists below are annotated `✅` where shipped.

---

## 2. Does it actually save tokens? — The savings reality

> **Update (2026-05-29):** the measurement gaps below are now largely **fixed** — routing `saved_usd` is priced against the original model, Anthropic prompt-cache usage is mapped correctly (non-stream + stream), and streaming responses emit a terminal `tokentrimmer.usage` event. So the **provable** savings now approach the **delivered** savings. **L2 semantic cache is now wired** (behind `TT_L2_SEMANTIC_CACHE=1`); its hit-savings still use a synthetic baseline pending a `cache_entries.baseline_cost_usd` column (cloud migration). The original analysis below is preserved for context.

**This is the question that matters most, so it goes first.** Here is the honest mechanism-by-mechanism scorecard, traced through real code:

| Mechanism | Saves real $? | Measured & shown? | Status |
|---|:---:|:---:|---|
| **L1 exact cache** (Redis) | ✅ yes | ✅ yes | **Real & honest.** Hit → cost $0, `saved_usd` = real baseline carried in the L1 envelope. (`chat.rs:436-460`, `l1_entry.rs`, tests pass.) |
| **Provider prompt-cache (OpenAI)** | ✅ yes | ✅ yes | **Real & honest.** Cached input billed at the discounted rate, baseline at full rate. (`chat.rs:640-667`.) Savings are produced by the provider; TT measures them. |
| **Model routing** (downgrade to cheaper model) | ✅ **yes — on the bill** | ❌ **no — reports ~$0** | **The core bug.** Routing rewrites `req.model` then prices *both* cost and baseline against the cheap routed-to model → `saved_usd ≈ 0`. Customer saves money; TT can't see it. (`chat.rs:143,314-316`.) |
| **L2 semantic cache** (pgvector) | ⚠️ would, but | ❌ **$0 in prod** | **Built, tested, never wired.** `with_l2` is called nowhere outside the builder definition + doc examples. Dead at runtime in both the CLI gateway and cloud API. (`state.rs:111` only.) |
| **Anthropic prompt-cache injection** | ✅ yes | ❌ **mis-measured** | Injection is correct, but usage-mapping mismatch under-reports savings (non-stream) and **streaming hardcodes `cached_tokens: 0`** → every streamed Claude call shows zero cache benefit. (`translate.rs:489-498`, `stream.rs:549`.) |
| **RAG context compression** | ⚠️ maybe | ❌ heuristic-only | `tokens_saved` is `char_delta/4`, can go **negative**, never folded into cost, `org_id` hardcoded to `nil`. Not production-isolated. (`substitute.rs:63-74`, `retrieval.rs:160`.) |
| **Inspect static fixes** | ✅ (advisory) | ✅ (qualitative) | **Honest.** Flags waste pre-runtime with dollar-quantified hints; doesn't claim measured savings. Correct positioning. |
| **Plan projected savings** (offline) | ✅ correct math | ⚠️ corrupted input | The one place routing savings are computed correctly — but it reads `request_logs.baseline_cost_usd`, which the gateway writes wrong for routed traffic. Fix the gateway and Plan becomes trustworthy. |

### 2.1 A realistic savings model

Grounded in the **actual pricing tables in the code** (not marketing). Representative customer: a SaaS feature on OpenAI `gpt-4o`, ~10M input + 2M output tokens/day.

- **Baseline:** 10M × $2.50/M + 2M × $10/M = **$45/day ($1,350/mo).**
- **Layer 1 — L1 exact cache** _(real + measured)_: ~20% exact repeats → **−$9/day** → $36.
- **Layer 2 — provider prompt cache** _(real + measured, OpenAI)_: ~40% of input cached at 50% off → **−$4/day** → $32.
- **Layer 3 — model routing** _(real on the bill, **not measured today**)_: route ~30% of low-stakes calls `gpt-4o → gpt-4o-mini` (≈94% cheaper on that slice) → **−$12.69/day** → ~$20.

| Band | Total reduction | What it represents |
|---|:---:|---|
| **Conservative** | **~12%** | Low cache-hit, low repeat rate. Measured today. |
| **Expected** | **~28%** | The provable-today number (L1 + prompt cache). **What TT can prove right now.** |
| **Optimistic** | **~55%** | Includes routing — **delivered to the customer's bill today, but invisible** in TT's headers/dashboard until the routing-baseline bug is fixed. |

> **The single most valuable fix in this entire review** is `fix-routing-baseline-savings` (a few lines in `chat.rs`): capture the original requested model before routing rewrites it, and price `baseline_cost_usd` against *that*. This moves the provable number from ~28% toward ~55% — making the headline claim true *and demonstrable*. It also repairs Plan's input and every downstream dashboard/digest figure.

### 2.2 The honest one-liner for marketing

Until the fixes land, the defensible public claim is: **"15–30% measured savings from caching alone, typically 40–55% with routing"** — and you should be able to show the measured portion live. After the fixes, the full number becomes provable per-request.

---

## 3. Scorecard by dimension

| Dimension | Grade | One-line |
|---|:---:|---|
| Architecture & scalability | **A−** | Clean layering; debts are pricing-as-code, registry choke point, unwired retry/fallback. |
| Savings correctness | **C+** | Caching honest; routing/L2/Anthropic-cache measurement broken — fixable. |
| Providers & gateway | **B** | All 8 adapters real; registry 404s unlisted models; a few cost-accounting bugs. |
| Inspect | **B−** | 10/15 rules ship, all regex (AST harness unused), FP gate on self-authored fixtures. |
| Plan | **B+** | Excellent engine; cache-savings + latency projection + reconciliation are gaps. |
| Value visualization | **C+** | Dashboard strong; **moment-of-use** (streaming headers, proxy banner) broken. |
| Design & brand | **D** | Functional but no design system; marketing site is a placeholder. |
| Onboarding & docs | **C+** | CLI self-documents; no unified guide; README quickstart is inaccurate. |
| Security & crypto | **A−** | Strong core; ops hygiene (prod secrets on disk) + harness least-privilege to fix. |
| Backlog hygiene | **B** | Public near-drained & honest; cloud backlog stale; one cheap unblock gates ~10 P0s. |

---

## 4. The critical path — what to work on next (ranked)

If you do nothing else, do these, in this order. The first cluster makes the product **provably** save money; the second makes it **look** like the premium product it is; the third unblocks the launch chain.

> **Status (2026-05-29):** clusters 1, 2 and 4 below are **done** — the savings-measurement fixes, moment-of-use surfacing, and the cloud-repo unblock all shipped (see the Implementation-status section above). The big remaining piece is cluster 3 (**design/marketing — cloud repo**), plus `wire-l2-cache-production` and `env-secret-split-rotate`.

1. **Prove the savings you already deliver** (small, surgical, highest ROI):
   - `fix-routing-baseline-savings` — price baseline against the *original* model. **(P0, S)**
   - `fix-anthropic-stream-cached-tokens` — stop zeroing cache reads on streamed Claude calls. **(P0, S)**
   - `fix-anthropic-cache-usage-mapping` — correct the OpenAI-vs-Anthropic usage convention. **(P1, S)**
   - `wire-l2-cache-production` — actually attach L2 in the gateway (behind a flag) or stop listing it as live. **(P0, M)**
2. **Show the savings at the moment of use:**
   - `fix-proxy-savings-banner` — the Ctrl-C banner always prints `$0.0000`; wire the values already in scope. **(P1, S)**
   - `stream-cost-headers` — emit cost/saved on streaming responses (terminal SSE event). **(P1, M)**
3. **Look premium:**
   - `design-system-foundation` — tokens + shared Layout (kills 16× duplicated CSS). **(P0, L)**
   - `marketing-site-build` — replace the placeholder front door. **(P0, L)**
4. **Unblock the launch chain (near-free):**
   - `cloud-repo-remote` — create the private GitHub repo + push; unblocks ~10 P0 launch gates in minutes. **(P0, S)**
   - `cloud-backlog-sync` — ~9 "open" cloud items are already shipped; flip them. **(P1, S)**
5. **Correct the on-ramp:**
   - `getting-started-guide` — **done in this session** (`GETTING_STARTED.md`); link from README. **(P1, done)**
   - `docs-readme-quickstart-fix` — wrong Docker image + nonexistent YAML config + 3 conflicting Rust versions. **(P1, S)**
6. **Tighten the harness** (your explicit request): least-privilege permission cleanup — **applied in this session** (see §9). **(P1, done)**

Everything else is in the new-development backlog (§7).

---

## 5. Detailed findings by area

Severity: **P0** blocks the core promise/launch · **P1** important soon · **P2** worthwhile · **P3** nice-to-have. `→` = recommendation.

### 5.1 Savings correctness & measurement
- **[P0] Routing savings unmeasurable** — `chat.rs:314-316` prices baseline against the routed-to (cheap) model; `_requested_model` is reserved-but-unused (`chat.rs:643,662`). `saved_usd ≈ 0` for every routed request, and the wrong baseline is persisted to `request_logs`, corrupting Plan. → Capture original model pre-routing; price baseline against it; assert `saved_usd > 0` in `route_rewrite.rs`.
- **[P0] L2 semantic cache never wired** — `with_l2` only appears at `state.rs:111` (definition) + doc examples. → Wire in CLI gateway behind `TT_L2_SEMANTIC_CACHE=1` (needs an embedder + real per-row baseline column), or stop advertising it as live.
- **[P0] Anthropic streaming zeroes cache** — `stream.rs:549 cached_tokens: 0`; `handle_message_start` discards `cache_read_input_tokens`. → Thread cache fields through `StreamState`; add a streaming snapshot test.
- **[P1] Anthropic non-stream cache mis-mapped** — `translate.rs:489-498` sets `prompt_tokens = input_tokens` (excludes cache reads) while `compute_cost` assumes the OpenAI subset convention → under-reported savings + understated cost. → Set `prompt_tokens = input_tokens + cache_read_input_tokens`.
- **[P2] Streaming responses carry no cost/saved headers** — `sse.rs:364-373` (see §5.5).
- **[P2] RAG tokens-saved is an unvalidated char-heuristic, can go negative, not tenant-isolated** — `substitute.rs:63-74`, `retrieval.rs:160`. → Clamp ≥0, real tokenizer, wire `org_id`.
- **[P3] Cost-preview knows pricing for only 3 of 8 providers** — `preview/src/pricing.rs:18-41` errors on Groq/Mistral/Together/OpenRouter/Local — including the dogfood routing target. → Probe all provider pricing tables.

### 5.2 Architecture & scalability
- **Strengths:** acyclic layering (`shared` = pure contracts); two-level error model; DB-optional by construction; solid CI (clippy `-D warnings`, deny, audit, gitleaks, weekly provider-contract matrix); reproducible distroless build.
- **[P1] Retry/fallback advertised but never wired** — `ProviderError::is_retriable/is_fallback_eligible` have zero callers; `chat.rs:309` is a single attempt. A transient 429/5xx fails the user. → Add a retry/backoff+fallback policy layer (the error type already encodes the decision).
- **[P1] `fee_multiplier` is a tested no-op** — set to 1.05 for OpenRouter, consumed nowhere in core → OpenRouter cost understated 5%. → Add `fee_multiplier()` to the Provider trait (default 1.0) and apply in `compute_cost`, or remove the field.
- **[P1] Pricing/model catalogs hardcoded in Rust with `effective_at: Utc::now()`** — won't scale to 50+ providers; breaks historical replay; price change = recompile+redeploy. → Externalize to versioned data + a refresh path; make `effective_at` the real date.
- **[P2] Accurate tiktoken estimator bypassed on hot paths** — routing decisions + stream cost use `len()/4` while `tt-preview` ships a real estimator. → Extract a shared `tt-tokenize` crate.
- **[P2] Registry is a compile-time choke point** — every new provider edits core; compat crates pull the *full* OpenAI adapter. → Split a `tt-provider-compat` base crate; make registration config-aware; sort `/v1/models`.
- **[P3] Public/cloud pin different axum/tokio/hyper majors** — latent hazard if a shared type touches axum. → Keep axum/hyper out of shared crates; add `[workspace.lints]`; align pins.

### 5.3 Providers & gateway
- **Strengths:** all 8 adapters are real; robust SSE parsers (fragmentation, `[DONE]`, ping-skip, mid-stream error); specific per-provider error mapping; `SecretString` redaction.
- **[P0] Registry 404s any unlisted model** — `by_model` is a static HashMap; a valid newer model (future GPT/Claude/Gemini, o3-mini, OpenRouter/Together passthrough, local models) 404s even though upstream would serve it. → Fall back to `infer_provider(model) → by_id` and pass through; keep the static table for pricing only.
- **[P1] Fixed 120s client timeout caps streaming mid-stream** — `client.rs:13-27`; `RequestContext.deadline` is documented-but-unused. → Separate streaming client config; honor `deadline` or drop it.
- **[P2] Anthropic `total_tokens` undercounts when caching active** — `translate.rs:489-498`. → `input + cache_read + cache_creation`.
- **[P3] OpenAI adapter blocks streaming for o3/o4-mini on a stale assumption** — `stream.rs:75-80`. → Remove the guard.
- **[P3] Anthropic ToolResult flattened to a string** — drops structured/multimodal tool results (`translate.rs:273-285`).

### 5.4 Inspect & Plan
- **Plan strengths:** real determinism (ChaCha8-seeded, byte-identical snapshot), honest bootstrap CIs (Monte-Carlo coverage test asserts 93–97%), conservative cost invariant, gated Tier-3 LLM-judge, audit-coupled apply.
- **[P1] Plan savings excludes ALL cache benefit** — cache hits compute a *rate* but never zero request cost (`replay.rs:189-224`), despite the design doc's `if hit: cost = 0`. → Thread the hit set into the cost vector.
- **[P1] Plan latency is a pure echo of historical latency** — a Sonnet→Haiku swap reports the *old* latency (`replay.rs:191`). → Build per-(provider,model) latency distributions or mark "not projected."
- **[P1] Reconciliation + user-facing trust score are design-only** — zero source hits for `reconcil/trust_score/calibrat`. This is the design's entire credibility loop. → Build the minimal reconcile job (compare projected vs actual post-apply).
- **[P1] Inspect ships 10 of 15 documented P0 rules** — missing `model-deprecated`, `prompt-bloated-system`, `config-agents-md-too-long`, etc. → Implement the 5 (all Tier-1 feasible) or reconcile the catalog.
- **[P1] FP gate runs on self-authored fixtures; OSS corpus BLOCKED** — stop citing "0% FP" until an independent corpus exists.
- **[P2] Every rule is regex; the tree-sitter AST harness is built but unused** — brittle FNs + silent FP-suppression. → Migrate the structural rules to AST queries.
- **[P2] New high-ROI rule ideas** — `cache-anthropic-tools-not-cached`, `output-n-greater-than-one`, `model-reasoning-effort-default-high`, `prompt-dynamic-prefix-breaks-cache`.

### 5.5 Value visualization ("showcase usefulness while it's being useful")
- **Strengths:** realized-savings math is honest end-to-end; both SDKs expose `.tt.saved_usd`; dashboard reconciles projected-vs-actual with a trust-score sparkline; weekly digest leads with "$X saved this week."
- **[P1] Streaming = zero moment-of-use savings visibility** — `sse.rs:364-373` emits only trace-id + provider; Claude Code/Cursor/chat UIs stream, so the most common path shows nothing and `.tt.savedUsd` is always null. → Emit a terminal `event: tokentrimmer.usage` before `[DONE]`.
- **[P1] `tt proxy` Ctrl-C banner always shows `$0.0000`** — handlers hardcode `suggested_savings_usd: None` (`anthropic.rs:63`, `openai.rs:58`) and never read `x-tokentrimmer-saved-usd`; `tui.rs` fakes "cache savings." → Wire the values already in scope (~15 lines).
- **[P2] No live CLI savings ticker** — savings only at shutdown. → A rewriting stderr line: `tt · 142 req · $1.83 saved · 38% cached`.
- **[P2] No shareable "saved $X" badge** — the badge plumbing exists but renders only Inspect counts. → Clone the HMAC signed-URL badge, swap the data source. High virality, near-zero infra.
- **[P3] Forgone-savings ("you left $X on the table") never aggregated** — preview suggestions are computed then dropped.

### 5.6 Design & brand (premium / luxury / modern bar)
- **Strengths:** sound IA (Overview/Costs/Cache/Plan/Reports/Routes/Inspect/Audit/Settings); thoughtful empty/onboarding states; chart loading/error states; consistent numeric formatting; Astro+Solid+uPlot is the right premium-capable stack.
- **[P0] Marketing site is a bare placeholder** — `apps/web/src/pages/index.astro` is `<h1>` + `<p>`, no CSS, no head, no nav/pricing/proof. The product's front door has no design. → Build a real site (hero + live savings proof, See/Plan/Optimize, dashboard preview, drop-in snippet, pricing, footer).
- **[P0] No design system** — `packages/ui/src/styles.css` is one comment, `index.ts` is `export {}`, 16 pages duplicate inline CSS, `#06c` hardcoded 24×, no shared Layout. → Author real tokens (color/type/space/radius/shadow/z) + a shared `<Layout>`; migrate all pages.
- **[P1] No brand identity** — no logo, favicon, or custom type; `system-ui` everywhere. → A wordmark+mark, favicon set, a typeface pairing (grotesk/geometric + mono for numerics), self-hosted woff2.
- **[P1] No dark mode** — table stakes for a developer/cost tool; zero `prefers-color-scheme`/`data-theme`. → Light+dark token sets + a persisted toggle.
- **[P2] Charts generic & off-brand** — monochrome, `$0.0010` raw axis labels, plain loading text. → Theme uPlot from tokens; smart currency formatting; skeletons; a shared Chart wrapper.
- **[P2] Nav/chrome reads as a prototype** — plain blue text links, no active state, no app shell. → Branded sidebar/topbar with icons, org/user menu, focus rings; styled auth screens.
- **[P3] Docs site is unstyled Starlight default** — theme it once the brand kit exists.

### 5.7 Onboarding & docs
- **Strengths:** the `tt` CLI is fully self-documenting; both SDKs are real with working READMEs; the one-line `base_url` swap genuinely works; `.env.example` is clean; `make dev` is real.
- **[P1] No single getting-started guide** — scattered across README + AGENTS.md + four ~700-byte stubs. → **Done: `GETTING_STARTED.md` drafted this session** (every command/path verified against source). Link from README.
- **[P1] README self-host quickstart is wrong** — references `ghcr.io/tokentrimmer/gateway` (the Dockerfile builds `tt-cli`) and a `/etc/tokentrimmer.yaml` mount (config is env-only, no YAML loader). → Fix to real image + env config.
- **[P2] Rust version stated 3 ways** — README 1.85, CONTRIBUTING 1.83+, toolchain/Docker 1.88. → Make it 1.88 everywhere.
- **[P2] `examples/` is empty** but README documents it. → Populate or remove the claim; clarify how to obtain `tt`.
- **[P3] 8080 (self-host gateway) vs 31415 (tt proxy) undocumented as distinct** — the new guide separates them.

### 5.8 Security & least-privilege
- **Strengths (genuinely strong):** XChaCha20-Poly1305 provider creds with per-(org,provider) derived keys + AAD row-swap defense; argon2 keys with NotFound-collapse (no existence leak); BLAKE3+Ed25519 audit chain with SERIALIZABLE/FOR-UPDATE append; `SecretString` redaction; Sentry `before_send` scrubber; **no secrets in git history**; cargo-deny/audit enforced in CI.
- **[P1] `.env.development` and `.env.production` are byte-identical and hold LIVE prod secrets** — real OpenAI key, Stripe, 64-char `TT_MASTER_KEY`/`TT_ADMIN_TOKEN`/audit key, 691-char Fly deploy token. Correctly gitignored & never committed, but production secrets sitting on the workstation under a "development" name. → Split dev/prod secret sets; keep prod only in `fly secrets`; **rotate** the high-value keys (note: `TT_MASTER_KEY` rotation requires re-encrypting `provider_credentials`).
- **[P1] Retrieval middleware hardcodes `org_id = Uuid::nil()`** — collapses all tenants on that path (`retrieval.rs:159-163`). The real `org_id` is already available via `ApiKeyContext`. → Thread it through; add a cross-org isolation test.
- **[P1] Harness allowlist has accreted broad, work-destroying entries** — `git checkout *`, `git restore *`, `awk *`, `cargo clippy *`, plus ~15 single-use prose entries. → **Cleaned up this session** (§9).
- **[P2] Harness allows a GPG-signing bypass** — `git -c commit.gpgsign=false commit` contradicts SECURITY.md's "all commits signed." → **Removed this session** (§9).
- **[P3] KDF documented as "HKDF" but is a SHA-256 one-shot** — sound construction, but align the docs (or switch to real HKDF) for audit accuracy.
- **[P3] `tt_test_*` sandbox keys bypass all auth** — limited blast radius (free synthetic responses). → Document as unauthenticated; consider rate-limiting.

### 5.9 Backlog triage & unblocks
- **[P0] Cloud repo has no git remote** — `git -C .../cloud remote -v` is empty. Every `[BLOCKED — needs cloud repo + GitHub remote]` P0 (≈10 launch gates) is gated on this one minutes-long action. → Create the private repo + push.
- **[P1] Cloud backlog is stale** — ~9 "open" items already shipped on disk (reconciliation, hosted inspect, /inspect, /reports, /settings, anomaly, export, trust-score, stripe-webhooks). → Flip to `[x]` so the real launch gates stand out.
- **Genuinely actionable now in public:** `inspect-corpora-seed`, `trackE-quality-audit-log` (public write path), `trackE-postgres-store` (after one cloud migration).
- **Missed opportunities (net-new):** spend budget caps / quota enforcement (the gateway has *none* today), Slack/webhook cost-alert bot, provider failover, cost-diff CI linting, savings badge, FinOps export — see §7.

---

## 6. Actionable remediation checklist (fix existing)

> Fixes to things that already exist — correctness, measurement, docs, security. Roughly ordered by leverage.

**Savings correctness (make the mission provable)**
- [x] `fix-routing-baseline-savings` **[P0/S]** ✅ — baseline priced against the original requested model (non-stream + streaming); asserts `saved_usd>0` on downgrade.
- [x] `fix-anthropic-stream-cached-tokens` **[P0/S]** ✅ — `cache_read/creation` threaded through `StreamState`; streamed Claude calls report real `cached_tokens`.
- [x] `wire-l2-cache-production` **[P0/M]** ✅ — L2 wired into gateway boot behind `TT_L2_SEMANTIC_CACHE=1` (PostgresL2Cache + OpenAI embedder) (`29a5b0a`). Honest per-row baseline still needs a `cache_entries.baseline_cost_usd` column (cloud).
- [x] `fix-anthropic-cache-usage-mapping` **[P1/S]** ✅ — `prompt_tokens = input + cache_read (+ creation)`; cost/savings now correct (`669506c`, `5a53298`).
- [x] `fee-multiplier-apply` **[P1/S]** ✅ — `Provider::fee_multiplier()` applied to cost+baseline; OpenRouter 5% BYOK (`cb0909b`).
- [x] `anthropic-total-tokens-fix` **[P2/S]** ✅ — cache-creation folded into `prompt_tokens`/`total_tokens` (`5a53298`).
- [x] `preview-pricing-all-providers` **[P3/S]** ✅ — `lookup()` probes all compat providers (`7d6e226`).

**Moment-of-use visibility**
- [x] `fix-proxy-savings-banner` **[P1/S]** ✅ — banner shows real realized savings (was `$0.0000`).
- [x] `stream-cost-headers` **[P1/M]** ✅ — terminal `tokentrimmer.usage` SSE event before `[DONE]` (`deec5bf`).

**Gateway correctness / resilience**
- [x] `registry-model-passthrough` **[P0/M]** ✅ — `registry.resolve()` falls back to `infer_provider → by_id` (`9317b76`).
- [x] `retry-fallback-layer` **[P1/M]** ✅ — bounded retry/backoff on transient errors (honors `retry_after_ms`), wired into non-stream + initial-stream dispatch (`691a055`). Alternate-provider fallback chain shipped in `provider-failover` (below).
- [x] `streaming-client-timeout` **[P1/S]** ✅ — read-idle timeout so long streams aren't cut at 120s (`05e048e`).
- [x] `openai-reasoning-stream-unblock` **[P3/S]** ✅ — o3/o4-mini stream; `Streaming` capability added (`a60f1ff`).

**Plan correctness**
- [x] `plan-cache-savings-wire` **[P1/M]** ✅ — projected cache hits zero per-request cost (`fbeafd0`).
- [x] `plan-latency-projection` **[P1/M]** ✅ — latency projected from the target model's window history (`7c3c2ec`).
- [ ] `plan-reconciliation-trustscore` **[P1/L]** — projected-vs-actual reconcile loop feeding the trust score. **(cloud-side — deferred.)**

**Security & hygiene**
- [ ] `env-secret-split-rotate` **[P1/M]** — **needs you:** split dev/prod secret sets; prod only in `fly secrets`; rotate `TT_MASTER_KEY`/`TT_ADMIN_TOKEN`/Fly/Stripe (read into this env).
- [x] `retrieval-orgid-isolation` **[P1/S]** ✅ — real `org_id` from `ApiKeyContext`; cross-org isolation test (`8e43137`).
- [x] `kdf-doc-align` **[P3/S]** ✅ — corrected to "SHA-256 KDF" (`38a7faf`).

**Docs & onboarding**
- [x] `getting-started-guide` **[P1]** ✅ — `GETTING_STARTED.md`.
- [x] `docs-readme-quickstart-fix` **[P1/S]** ✅ — real image + env config, Rust 1.88, `examples/` fixed (`58a0d01`).
- [x] `link-getting-started-from-readme` **[P1/XS]** ✅ — prominent link added in `58a0d01`.

**Backlog hygiene / unblocks**
- [x] `cloud-repo-remote` **[P0/S]** ✅ — cloud baseline committed + pushed to private `TokenTrimmer/cloud`; unblocks the launch gates.
- [ ] `cloud-backlog-sync` **[P1/S]** — flip ~9 already-shipped cloud items to `[x]`. **(cloud repo.)**
- [x] `perms-least-privilege-cleanup` **[P1]** ✅ — `settings.local.json` tightened (§9).

---

## 7. New-development backlog (net-new)

> Net-new features and larger initiatives, in `BACKLOG.md` format so they can be synced (`./scripts/backlog.sh sync`). These are **additions**, not fixes (those are §6).

### Design & brand (premium uplift) — _cloud repo_
- [ ] [P0] [design-system-foundation] astro-page-builder: Real token layer in `packages/ui/src/styles.css` (color/type-scale/8px-space/radius/shadow/z) + shared `<Layout>`/`<DashboardShell>` owning `<head>`, fonts, favicon, nav; migrate all 16 dashboard pages off inline CSS. (est: ~$2.50)
- [ ] [P0] [marketing-site-build] astro-page-builder: Build `apps/web` — hero + live savings proof, See/Plan/Optimize features, dashboard preview, drop-in snippet, pricing, social proof, footer; consume the new tokens. (est: ~$2.50)
- [ ] [P1] [brand-kit] astro-page-builder: Wordmark + mark (SVG), favicon/apple-touch set, typeface pairing (UI grotesk + numeric mono), self-hosted woff2; wire via Layout. (est: ~$1.00)
- [ ] [P1] [dark-mode] astro-page-builder: Light+dark token sets on `[data-theme]` + persisted toggle; theme uPlot from tokens. (est: ~$0.80)
- [ ] [P2] [chart-theming] astro-page-builder: Brand-themed uPlot (area fill, semantic green savings), compact currency axis, styled tooltips, skeleton loaders, shared Chart wrapper. (est: ~$0.80)
- [ ] [P2] [app-shell-nav] astro-page-builder: Branded sidebar/topbar with icons + active state, org/user menu, focus rings; styled signin/verify. (est: ~$0.80)
- [ ] [P3] [docs-site-theme] astro-page-builder: Theme Starlight to brand + land docs content sync. (est: ~$0.50)

### Cost-layer capabilities (the mission) — _public + cloud_
- [x] [P1] [budget-caps-quota] ✅ **shipped `afc7f68`** — `BudgetEnforcer` trait + `InMemoryBudgetEnforcer` in tt-core; per-org monthly cap + per-minute rate; auth middleware → 429 + `X-TT-Budget-Remaining-Usd` + `Retry-After`; record in chat handler + SSE guard. Postgres-backed limits remain a cloud follow-up.
- [x] [P2] [provider-failover] ✅ **shipped** — ordered fallback chain + per-provider circuit breaker. `RouteAction.fallbacks: Vec<String>` (serde-default, cloud-populated) drives `dispatch_with_failover` in `tt-core::failover`: tries `[primary, …fallbacks]`, skips providers whose `CircuitBreaker` is open (5 consecutive failures → 30s cooldown), fails over on fallback-eligible errors (5xx/timeout/model-not-found), short-circuits on non-eligible (bad request). Non-stream dispatch only; the serving provider is rebound so cost/headers/telemetry attribute correctly. 5 unit + 2 integration tests. Turns "cost layer" into "cost + reliability layer."
- [ ] [P2] [alert-dispatcher-slack] rust-crate-builder: Outbound webhook + Slack sink firing on budget thresholds (50/80/100%), `anomaly.detected`, reconciliation drift >2%. Reuses existing signals. (est: ~$0.80)
- [x] [P2] [cost-diff-ci-lint] ✅ **shipped** — `tt inspect --cost-diff [--base <ref>] [--fail-on-cost-increase]`: a pure `tt_cli::cost_diff::analyze` over `git diff <base> -- <path>` extracts `model`-keyed string assignments on added/removed lines, prices each via `tt_preview::pricing` (shared catalog, no cloud), and reports per-model rate deltas + a net projected per-call change under a standard 1K-in/500-out profile. Markdown (check-run summary) or JSON; optional non-zero exit on a projected increase. 7 unit tests + e2e verified. A sticky surface no competitor occupies.
- [ ] [P3] [finops-export] rust-crate-builder: FOCUS-aligned export format on the existing export endpoint for Cloudability/Vantage ingestion. (est: ~$0.50)

### Value visualization
- [ ] [P2] [savings-badge] rust-crate-builder: `GET /v1/badges/savings?org_id&expires&sig` SVG via the existing HMAC signed-URL plumbing; "$X saved this month" README/Slack badge + copy snippet on /reports. (est: ~$0.40)
- [x] [P2] [live-cli-savings-ticker] ✅ **shipped `7bcaea2`** — rewriting stderr status line in `tt proxy`: `tt · N req · $X saved · Y% cached`.
- [ ] [P3] [forgone-savings-view] rust-crate-builder: Aggregate preview `suggested_savings_usd` into a "potential additional savings" card (after the banner fix). (est: ~$0.60)

### Inspect & Plan depth — _public_
- [x] [P1] [inspect-5-missing-rules] ✅ **shipped `e168859`** — all 5 (`model-deprecated`, `prompt-bloated-system`, `prompt-verbose-few-shot`, `prompt-no-output-constraint`, `config-agents-md-too-long`) + fixtures; 15/15 P0 rules now ship.
- [ ] [P1] [inspect-corpora-seed] rust-crate-builder: Vendor 5–10 pinned permissive OSS LLM samples into `corpora/` (independent of rule author), run `measure-fp-rate.sh`, record per-rule precision/recall. Unblocks the w24 FP gate. (est: ~$0.50)
- [ ] [P2] [inspect-ast-migration] inspect-rule-author: Migrate the structural rules (cache_control/max_tokens/model-arg/loop-termination) from regex to the existing tree-sitter harness + a rule-level AST cache. (est: ~$1.20)
- [ ] [P2] [inspect-new-rules] inspect-rule-author: `cache-anthropic-tools-not-cached`, `output-n-greater-than-one`, `model-reasoning-effort-default-high`, `prompt-dynamic-prefix-breaks-cache`. (est: ~$0.80)

### Architecture scalability — _public_
- [x] [P1] [pricing-externalize] ✅ **shipped** — model *rates* moved out of Rust source into a versioned, embedded data file (`crates/shared/data/pricing.toml`, `include_str!`-loaded once into `tt_shared::pricing::PricingCatalog` via `OnceLock`). A rate refresh is now a data edit, not a release. Real `effective_at` per row + per-model price *history*: `catalog().latest(provider, model)` for live pricing, `catalog().at(provider, model, ts)` for historical replay. All 7 paid providers delegate to it (openai/anthropic/gemini via `pricing_for`; groq/mistral/openrouter/together via `pricing_table` → `latest_for_provider`); local stays zero. 32 models; each provider's existing `pricing_values_match_spec`/`pricing_table_correct_rates` test verifies the TOML transcription is exact. _Scope note:_ model **descriptors** (capabilities/limits) stay typed in Rust; `tt-plan-core` keeps its own `ModelPricing` (separate replay catalog) — externalizing that + a live refresh/Postgres path are follow-ups.
- [x] [P2] [token-estimator-shared] ✅ **shipped** — new `tt-tokenize` crate holds one estimator (cached tiktoken `cl100k` for openai/anthropic, `chars/4` heuristic otherwise, with a `Confidence`). `/v1/preview` (`tt-preview`) and the live routing estimate (`tt-core` `apply_routing`) both delegate to it, so a route's token-threshold decision matches what preview reports instead of using a separate `len/4` heuristic. `tiktoken-rs` moved into `[workspace.dependencies]`. tt-routing stays tokenization-free by design (receives the estimate). 5 tokenize unit tests; preview + routing suites green.
- [x] [P2] [compat-crate-split] ✅ **shipped** — new `tt-provider-compat` crate holds the OpenAI-wire machinery (`client`/`compat`/`errors`/`stream`/`translate` + `OpenAICompatibleProvider`/`CompatConfig`/`ClientConfig`); `tt-provider-openai` now depends on it (native = compat + OpenAI pricing) and re-exports the types for back-compat; Groq/Mistral/Together/OpenRouter depend only on `tt-provider-compat`, not the full OpenAI adapter. Registration is config-aware via `ProvidersConfig` + `register_providers` (honors a `TT_PROVIDERS` allowlist; unset = all-on). `/v1/models` is sorted by (provider, id) for a stable catalog. Tests + snapshots moved with the code; all provider suites green.
- [ ] [P2] [hnsw-org-recall] rust-crate-builder: Fix L2 recall under multi-tenant load (tune `ef_search` for org-filtered query or partition `cache_entries` by org); add a recall regression test. (est: ~$0.80)
- [x] [P3] [workspace-lints-align] ✅ **shipped `6d99465`** — `[workspace.lints]` (forbid unsafe, deny `uninlined_format_args`) propagated to all 23 crates, mirroring cloud. (axum/hyper-out-of-shared remains a convention — not cleanly a cargo-deny ban.)

---

## 8. Getting-started guide

**Created this session:** [`GETTING_STARTED.md`](GETTING_STARTED.md) — a complete, copy-paste-ready guide covering what TokenTrimmer is, the four products, prerequisites per path, the 5-minute hosted quickstart, the self-host (Docker, env-only) path, and a verified example for **every** surface (`gateway`, `inspect`, `plan`, `init`, `mcp`, `proxy`, `retrieval`, both SDKs, `audit verify`). Every command, path, port, env var, and header was checked against source before drafting. It also supersedes the inaccurate README quickstart — fold those corrections back into README (`docs-readme-quickstart-fix`).

---

## 9. Least-privilege permissions (your request)

Applied this session to `.claude/settings.local.json`:
- **Removed** the broad, work-destroying allows: `git checkout *`, `git restore *`, `awk *`, `cargo clippy *`, and the GPG-signing-bypass commit allow (which contradicted SECURITY.md's "all commits signed").
- **Removed** ~15 single-use, prose-laden one-off entries that can never match again (specific `session-end.sh`/`context-for.sh` strings, etc.).
- **Kept** a minimal, narrowly-scoped working set (scoped `tt`/script invocations, read-only status checks).

**Principle going forward** (also saved to memory): scope every allowlist entry as narrowly as the task permits (verb + subcommand, never bare `Bash(tool *)`); prefer the project-tracked `settings.json` for durable narrow allows; approve broad/destructive verbs per-invocation rather than persisting them; keep mutating-git verbs (`checkout`/`restore`/`reset`/`clean`) deny-by-default. The committed `settings.json` deny-list (push, `rm -rf`, curl/wget, publish, sudo) remains authoritative.

---

## 10. The ideal state (north star)

A prospect lands on a **premium, branded marketing site** with a live savings proof point. They change one line (`base_url`) and immediately see, **in the moment** — in their terminal, in the proxy banner, on the streamed response — a running, *accurate* "$X saved" that includes routing. They run `tt inspect` in CI and get dollar-quantified waste findings with an AST-precise FP rate validated on real OSS code. They run `tt plan` and trust the projection because a **reconciliation loop** has scored its past projections (the trust score is real, not a stub). The dashboard, in a cohesive design system with dark mode, shows realized savings, forgone savings, a shareable badge, and budget caps that actually enforce. Every claim the product makes about saving money is **measured, provable, and visible while it's being useful** — which is exactly the gap this review maps the path to closing.

---

## Appendix — verification log

Independently re-verified against source before publishing (not taken on the reviewers' word):
- Routing baseline: `chat.rs:143` (`apply_routing(&mut req)`), `:314` (`pricing(&response.model)`), `:316` (`compute_cost(..., &req.model)`), `:643/:662` (`_requested_model` reserved-unused). **Confirmed.**
- L2 wiring: `with_l2`/`PostgresL2Cache::new`/`InMemoryL2Cache::new` → only `state.rs:111` + `l2.rs` doc examples across both repos. **Confirmed dead in prod.**
- Anthropic stream: `stream.rs:549 cached_tokens: 0`, `:550 cache_creation_input_tokens: None`. **Confirmed.**
- Proxy banner: `routes/anthropic.rs:63` + `routes/openai.rs:58` `suggested_savings_usd: None`; `session.rs:65` sums it; `tui.rs:13` reads it. **Confirmed always $0.**
- `fee_multiplier`: zero consumers in `crates/core` or `crates/cli`; defined only in shared/providers/tests. **Confirmed no-op.**
- Marketing placeholder: `cloud/apps/web/src/pages/index.astro` = `<h1>` + `<p>`, comment admits "placeholder to keep the build green." **Confirmed.**
- Cloud git remote: `git -C cloud remote -v` empty. **Confirmed.**
