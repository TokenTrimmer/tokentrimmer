# TokenTrimmer — Buildout Plan (Solo Founder + Heavy Autonomous Loops)

## Context

TokenTrimmer is a four-pillar LLM cost-optimization platform: **Gateway** (Rust/Axum proxy in front of 10+ LLM providers, OpenAI-compatible API), **Inspect** (CLI + hosted rules engine that scans codebases for ~120 token-waste patterns), **Plan** (deterministic replay simulator that projects cost/quality/latency from proposed config changes with bootstrap CIs), and **Reporting** (Astro 5 + Solid.js dashboard, weekly digests, monthly PDFs).

The full architecture is already specified across five docs in `/Users/iansimon/Developer/TokenTrimmer/docs/`. This plan does **not** restate that work — it operationalizes it for the chosen execution mode: **solo founder driving with Claude Code, heavy autonomous build loops, two-repo structure (public OSS + private cloud), enterprise tier deferred to post-beta.** The plan also incorporates research from May 2026 that updates several spec assumptions.

User asks being satisfied:
- Repo best practices that mirror Inspect rules (dogfood — the codebase passes its own scans)
- Hooks/harnesses for autonomous building with token-cost guardrails
- Context-conscious (subagents bound parent context; hooks scope checks per-crate)
- Low-cost hosting (~$80/mo MVP, ~$300-500/mo at 100 customers)
- Variety of plans (6 tiers from OSS to Enterprise)
- Email-only support (no phone, no chat)
- Auditable results for **all** tiers (hash-chained log, trust score, reconciliation report — even Free)
- Solo dev to major enterprise (OSS, Free, Solo, Pro, Team, Scale, Enterprise)

## Critical Research Findings (May 2026) That Change the Plan

These are decisions in the spec that must shift based on current vendor state:

1. **Cloudflare R2 does NOT support Object Lock / WORM compliance.** R2 has "Bucket Locks" (a different feature, not WORM). Immutable audit storage for Scale/Enterprise must use **AWS S3 with Object Lock in Compliance mode** (or Backblaze B2 with Object Lock). Adds ~$5-20/mo per enterprise customer in COGS. R2 still fine for non-compliance assets (PDFs, exports). [R2 docs](https://developers.cloudflare.com/r2/pricing/), [R2 bucket locks](https://developers.cloudflare.com/r2/buckets/bucket-locks/)

2. **Fly.io now bills inter-region traffic at machine rates (Feb 2026).** Multi-region Gateway is more expensive than the spec assumed. Mitigation: keep `iad` as primary, add `lhr` only after $5K MRR, defer `syd` until enterprise APAC demand. [Fly pricing](https://fly.io/docs/about/pricing/)

3. **Cloudflare AI Gateway is FREE** (100K logs/mo) with caching, analytics, rate limiting, retries, fallback. Direct competitor. **Positioning shift required**: TokenTrimmer must differentiate on (a) Inspect (no competitor does code analysis), (b) Plan (no competitor does replay projection with CIs), (c) trust score + hash-chained audit log, (d) provider-native cache-control intelligence, not raw caching. [Cloudflare AI Gateway](https://developers.cloudflare.com/ai-gateway/reference/pricing/)

4. **Neon was acquired by Databricks (early 2026)**, compute costs dropped 15-25%. Read replicas exist but cost ~$162/mo per replica at 1CU avg. Multi-region story: keep primary in `us-east`, add read replica in `eu-west` only when EU customers need <100ms dashboard latency. [Neon pricing](https://neon.com/pricing)

5. **WorkOS is $125/connection/mo** (volume discount to $65 at 51-100 connections). Confirms the Enterprise floor must be ≥$24K ARR. [WorkOS pricing](https://workos.com/pricing)

6. **Typst is at 0.14, not 1.0**, but Rust-native PDF gen works today; sub-second renders; Apache 2.0. Stay with Typst over Playwright for monthly PDFs (saves ~$50/mo in render capacity vs Chromium). [Typst on crates.io](https://crates.io/crates/typst-pdf)

7. **Provider pricing baselines (May 2026)** for the pricing table that ships in `crates/providers/*/src/pricing.rs`:
   - OpenAI: GPT-5.5 $5/$30, GPT-5.4 $2.50/$15, cached input 10% of standard, Batch 50% off, Flex tier available [OpenAI pricing](https://openai.com/api/pricing/)
   - Anthropic: Haiku 4.5 $1/$5, Sonnet 4.6 $3/$15, Opus 4.7 $5/$25; prompt cache 90% off; Batch 50% off; **stacks to 95% combined savings** [Anthropic pricing](https://platform.claude.com/docs/en/about-claude/pricing)
   - Gemini: 3.1 Flash-Lite $0.25/$1.50, 3.5 Flash $1.50/$9, 3.1 Pro $2/$12 (≤200K) or $4/$18 (>200K); context cache 90% off; Batch 50% off [Gemini pricing](https://ai.google.dev/gemini-api/docs/pricing)
   - OpenRouter: pass-through, 5% BYOK fee, no markup on direct keys [OpenRouter pricing](https://openrouter.ai/pricing)

8. **Competitive landscape**:
   - **Helicone** ($79/mo Pro, 10K req free, OSS self-host) — closest competitor; lacks replay/projection [Helicone](https://www.helicone.ai/)
   - **Portkey** (usage-based, Enterprise gated) — strong governance, no code analysis [Portkey pricing](https://portkey.ai/pricing)
   - **LiteLLM** (free OSS, $250/mo Enterprise Basic) — proxy/router, no Inspect/Plan equivalent [LiteLLM enterprise](https://docs.litellm.ai/docs/enterprise)
   - **Langfuse** (free Hobby 50K/mo, $29 Core, $199 Pro, $2,499 Enterprise; MIT self-host) — observability/eval focus, no cost projection [Langfuse pricing](https://langfuse.com/)
   - **Braintrust** ($249/mo Pro) — eval-first, no proxy [Braintrust](https://www.braintrust.dev/)
   - **None of them** offer: deterministic replay with CIs, hash-chained audit log, trust score, static code analysis for LLM patterns. **That is the wedge.**

## Final 7-Tier Structure

| Tier | Price | Req/mo | Seats | Audit retention | Audit export | Key differentiator |
|---|---|---|---|---|---|---|
| **OSS** | $0 | Unlimited (self-host) | n/a | Local file (forever) | Local file | Apache 2.0, Tier 1 rules, full data sovereignty |
| **Free** | $0 | 5K | 1 | 7d in-app | None | GitHub OAuth required, L1 cache only |
| **Solo** | $19/mo | 50K | 1 | 7d | None | Hosted, L2 cache, Tier 1+2 Inspect |
| **Pro** | $99/mo | 500K | 5 | 90d | CSV/JSON on-demand | All rules, GitHub PR bot (1 repo) |
| **Team** | $399/mo | 5M | 25 | 180d | + Webhook | Custom routes, PR bot (10 repos), Google/GitHub SSO |
| **Scale** | $1,499/mo + usage | 50M | 100 | 1yr | + S3 Object Lock immutable | Priority email (12h), Microsoft SSO, signed monthly summary |
| **Enterprise** | Contract (≥$24K/yr) | Custom | Unlimited | 7yr | + Customer S3 sync, SIEM (CEF) | SAML/OIDC via WorkOS, BAA, SOC-2, Docker Compose self-host, 4h email SLA + dedicated CSM contact |

**Audit-for-all rule**: every tier (including Free) gets `X-TokenTrimmer-Trace-Id`, `X-TokenTrimmer-Cost-Usd` / `Baseline-Cost-Usd` / `Saved-Usd` headers, trust score in dashboard/CLI, and a reconciliation report (7d window for Free, longer for paid). This is the **competitive moat** vs Helicone/Portkey/LiteLLM.

**Overage**: $0.0002/req (Solo) → $0.00005/req (Scale). Free has hard cap, no overage. Annual = 10× monthly (~17% off).

**Pricing transparency**: public exact prices for OSS through Scale. Only Enterprise says "Contact us." No "starting at," no "request demo," no chat widget.

## Solo-Founder Roadmap (~26 Weeks to Beta)

The spec's 19-week plan assumed 2 engineers + 0.5 founder. Solo execution slips ~30-50%. Below is a realistic 26-week solo plan with explicit cuts vs the spec. Each week has **Build / Dogfood / Audit Gate** sections.

### Week 0 — Pre-flight (NEW)
- Create `tokentrimmer/tokentrimmer` (public, Apache 2.0) and `tokentrimmer/cloud` (private)
- Provision: Fly.io, Neon, Upstash, Cloudflare, Stripe, Resend, Sentry, GitHub org
- Write root `AGENTS.md` (target <2K tokens, hard cap 4K)
- Wire `.claude/settings.json` hooks: `pre-edit-guard.sh`, `post-edit-scoped-check.sh`, `audit-line.sh`, `cost-cap-check.sh` (see §"Dev Infra & Autonomous Harness" below)
- Define subagent fleet in `.claude/agents/`: `rust-crate-builder`, `provider-adapter-author`, `inspect-rule-author`, `astro-page-builder`, `plan-replay-validator`, `dogfood-inspect-runner`, `onboarding-context-loader`
- Throwaway "hello world" Rust binary deploys to Fly.io `iad` — pipeline proven
- **Gate**: branch protection, `CODEOWNERS`, signed commits, `cargo-deny` baseline clean

### Weeks 1-2 — Landing, waitlist, Gateway skeleton
- Marketing site (Astro on Cloudflare Pages) with Resend waitlist
- Rust workspace, `crates/shared/` types, `crates/core/` Axum skeleton, OpenTelemetry wired
- `Provider` trait defined (per `02-provider-adapter-guide.md`)
- **Dogfood**: every Claude Code session writes to `.claude/AUDIT.log`
- **Gate**: waitlist signup writes audit row (`waitlist.signup`), proving audit-from-day-1

### Weeks 3-5 — OpenAI + Anthropic adapters (the foundation)
- OpenAI adapter (canonical OpenAI format passthrough is easiest)
- Anthropic adapter (the worked example; harder due to separate system field, required max_tokens, cache_control auto-injection at ≥1024 tokens)
- L1 Redis cache, basic single-rule routing, telemetry to Postgres
- Insta snapshot tests + httpmock integration tests
- **Dogfood**: route Claude Code's own API calls through local Gateway. **Customer zero.**
- **Gate**: load test (p50 < 30ms miss, < 5ms hit on Fly `shared-cpu-2x`); audit log entries for `route.*`, `provider_credential.*`

### Weeks 6-7 — Provider breadth (compatible providers)
- Gemini (real lift: different endpoint structure, `systemInstruction`, tools format)
- Mistral, Groq, Together, OpenRouter (all OpenAI-compatible, mostly testing)
- **Dogfood**: route classifications to Groq for our own internal use; first routing-engine production exercise
- **Gate**: per-provider contract tests in scheduled weekly CI ($5/wk cap)

### Week 8 — Local providers + streaming hardening
- Ollama, vLLM, LM Studio (OpenAI-compatible, single crate)
- SSE streaming hardening (typed Anthropic events; Gemini's JSON-array stream; per-chunk < 1ms overhead)
- **Gate**: 100 concurrent SSE streams test passes

### Weeks 9-10 — Auth + billing + dashboard skeleton
- Auth.js magic-link via Resend
- Stripe Checkout, Customer Portal, webhook handlers (subscription, invoice)
- API key issuance (`tt_live_*` with argon2 hash; prefix only visible)
- Dashboard shell: login, "create your first key," empty request log table
- **Dogfood**: founder signs up via prod magic-link, pays $1 test, issues key, makes real call
- **Gate**: audit rows for `user.login`, `apikey.issued`, `subscription.changed`; Stripe webhook signature replay-attack test

### Week 11 — HARD CHECKPOINT (was Week 8 in spec; slipped by 3 weeks for solo)
Eleven acceptance items (all must be checked or scope cut):
- [ ] New user signs up via magic link on `dashboard.tokentrimmer.com`
- [ ] Pays via Stripe Checkout, lands on dashboard logged in
- [ ] Adds provider credential (encrypted with XChaCha20-Poly1305)
- [ ] Issues API key in `tt_live_*` format
- [ ] OpenAI Python SDK pointed at `https://api.tokentrimmer.com/v1` returns completion for both OpenAI and Anthropic models
- [ ] Dashboard shows that request within 30s
- [ ] Gateway p50 overhead < 30ms miss / < 5ms hit on prod
- [ ] Customer Portal cancel actually downgrades
- [ ] Every state-changing action above wrote an audit row
- [ ] Sentry has captured an intentional test error from each service
- [ ] All CI green; zero clippy warnings; zero cargo-audit highs

**If any unchecked**: cut in this order: monthly PDF → anomaly detection → Plan quality scoring → Inspect Tier 2 → Inspect rules count from 15 → 10.

### Weeks 12-13 — Semantic cache (pgvector L2) + cost pages
- pgvector HNSW index, embedding pipeline via `text-embedding-3-small` (cheap default)
- Dashboard `/` overview + `/costs` cost-explorer pages
- **Dogfood**: L2 on for our traffic; hit rate climbs across days
- **Gate**: weekly reconciliation job runs — savings claimed must match actual provider invoices within 2%. **This gate blocks the customer-facing "saved $X" headline.**

### Week 14 — Inspect CLI + 10 P0 rules (reduced from 15 for solo)
- `tt inspect` Rust binary, tree-sitter for Python + TS
- 10 highest-confidence rules: `cache-anthropic-prompt-cache-missing`, `cache-openai-prompt-cache-eligible`, `lib-anthropic-sdk-no-cache-control`, `model-flagship-for-classification`, `model-flagship-for-extraction`, `output-no-max-tokens`, `conversation-unbounded-history`, `agent-no-termination-condition`, `config-no-agents-md`, `config-agents-md-contains-secrets`
- **Dogfood**: `tt inspect` runs against our own repos as CI gate (initially advisory)
- **Gate**: each rule has ≥5 positive + ≥10 negative fixtures; FP rate <5% on `corpora/` open-source samples

### Week 15 — Inspect hosted backend + GitHub Action
- Hosted scan endpoint, GitHub Action wrapper
- Dashboard `/inspect` page
- **Dogfood**: GitHub Action becomes blocking on our own PRs (`fail-on: high`)
- **Gate**: short-lived Action token can ONLY call `/inspect/runs`, never billing/keys

### Weeks 16-17 — Plan engine (cost projection only, defer quality scoring)
- Replay engine using stored telemetry (`request_logs` schema per `03-plan-replay-design.md`)
- Bootstrap CIs (10K iterations) on cost, savings, cache hit rate
- CLI `tt plan`; admin endpoint `POST /v1/admin/plans`
- **Defer**: Tier 3 LLM-judge quality scoring → Week 21 if time, else post-beta
- **Dogfood**: run `tt plan` with "swap Sonnet for Haiku on classification" diff against our own traffic
- **Gate**: replay determinism (same inputs → bit-identical output); reconciliation of one applied Plan within 15% on cost

### Weeks 18-20 — Reporting (dashboard + weekly digest + trust score)
- All dashboard pages (`/cache`, `/routes`, `/plan/[id]`, `/reports`, settings)
- Weekly digest email (Resend MJML template)
- **Trust score** (the key user-visible audit primitive — rolling variance between projected and actual savings, 0-100)
- **Defer**: monthly PDF (Typst) → Week 22, anomaly detection → Week 23
- **Dogfood**: founder receives weekly digest; must lead to ≥1 action
- **Gate**: dashboard p75 load <1.5s; reconciliation report shows projection vs actual on Plans applied in Week 16-17

### Weeks 21-22 — Polish + monthly PDF + Tier 3 quality (if time)
- Typst monthly executive PDF (Apache 2.0, Rust-native, sub-second render)
- Plan Tier 3 quality scoring if Week 16-17 deferred items still feasible
- Anomaly detection (z-score on hourly spend)
- **Gate**: PDF <500KB, accessible (PDF/UA-1); anomaly synthetic-5σ test fires

### Weeks 23-24 — Private alpha (5-10 hand-picked from waitlist)
- Free tier goes live for alpha only
- Onboarding script with `onboarding.step.completed` audit events
- **Gate**: 14 consecutive days reconciliation ≤2%; Inspect FP rate <5% confirmed on alpha users; zero P0 bugs >24h open

### Weeks 25-26 — Bug fix + beta launch
- Pro tier goes live ($99/mo)
- HN, Reddit, IH, Product Hunt
- **Pre-launch gate**: rehearsed game-day (kill Fly region, observe failover); penetration test (auth, key handling, audit integrity, cross-tenant isolation); runbook coverage 100% of PagerDuty alerts

### Post-beta tier rollout
- **+30d**: Team tier ($399/mo) — needs RBAC + multiple keys per role (small additional scope)
- **+60d**: Scale tier ($1,499/mo) — needs SLO proof + S3 Object Lock for immutable audit
- **+90d**: Start enterprise design-partner conversations if any waitlist contacts ask
- **+6mo**: Enterprise tier GA (WorkOS SAML, SOC-2 Type I via Vanta/Drata started, BAA available, Docker Compose self-host bundle)

## Dev Infra & Autonomous Harness (Heavy Loops)

**The thesis**: every line of code written before the harness exists costs more and is harder to audit. Build the harness first, then build the product.

### `.claude/settings.json` (Week 0)
Hooks designed to convert expensive whole-workspace operations into cheap scoped operations and fail fast:

- **SessionStart**: inject minimal context (1500 token target) — branch, HEAD, `git status --short`, `STATE.md` carryover. **Do not** load tree.
- **UserPromptSubmit**: `inject-agents-md-once.sh` — injects root `AGENTS.md` only on first turn (sentinel `/tmp/tt-session-$ID-loaded`), not every turn
- **PreToolUse(Write|Edit)** — `pre-edit-guard.sh`:
  - Blocks edits >800 lines on `.rs` files
  - Blocks `AGENTS.md` edits pushing past 4000 tokens (our own `config-agents-md-too-long`)
  - Blocks secret patterns (Anthropic `sk-ant-`, OpenAI `sk-`, Stripe `sk_live_`, generic 40+ char high-entropy near "key"/"secret")
  - Blocks Anthropic `messages.create` with long literal `system=` and no `cache_control` (`lib-anthropic-sdk-no-cache-control` enforced at edit time)
- **PostToolUse(Write|Edit)** — `post-edit-scoped-check.sh`: resolves crate from `Cargo.toml` ancestry, runs ONLY `cargo check -p <crate>` + `cargo clippy -p <crate> -- -D warnings`. For test edits: `cargo test -p <crate> --test <module>`. For `.astro`/`.tsx`: `pnpm --filter <pkg> typecheck`. Compiler output returned via `additionalContext` if fails. **This single hook is the highest-ROI token saver.**
- **Stop** — `audit-line.sh` appends one line to `.claude/AUDIT.log` (session, branch, files, added/removed, model, cost, trace_id); `cost-cap-check.sh` reads `.claude/cost-ledger.jsonl`, drops `.claude/PAUSED` if daily or session cap exceeded

**Permissions**: allow only scoped cargo commands (`cargo check -p *`, `cargo test -p *`, `cargo clippy -p *`), deny `cargo test --workspace`, `cargo build --release`, `rm -rf`, `git push`, `curl`.

### Subagent Fleet (`.claude/agents/`)
Each agent's system prompt <1500 tokens, scoped to one crate/rule/page, returns mandatory 5-line summary, declared max tool calls, inherits hook suite (no bypass).

Definitions to ship:
- **`rust-crate-builder`** — scoped to one crate; runs `cargo test -p <crate>` before returning
- **`provider-adapter-author`** — one provider; reads `02-provider-adapter-guide.md` once; produces Provider trait impl + httpmock tests + insta snapshots; min 20 fixture cases
- **`inspect-rule-author`** — one rule; min 5 positive + 10 negative fixtures; refuses return if FP rate >5%
- **`astro-page-builder`** — one page; must reuse `packages/ui` components first; runs typecheck + build
- **`plan-replay-validator`** — synthetic telemetry with known savings; verifies CI coverage rate
- **`dogfood-inspect-runner`** — runs `tt inspect .` on this repo, surfaces only NEW findings vs `main`
- **`onboarding-context-loader`** — given one-line task, returns 500-token brief of relevant files; parent uses this before dispatching workers (the "Read for me so I don't pollute my context" pattern)

### Ralph-Loop-Style Autonomous Build (`scripts/ralph-iteration.sh`)
Run by `loop` skill or `CronCreate`. State in `.claude/STATE.md` (current task, last iteration, weekly budget used, next-permitted time). Queue in GitHub Issues with `autopilot` label.

Iteration body:
1. Check `.claude/PAUSED` — exit if present
2. Check weekly budget cap — pause if exceeded
3. `gh issue list --label autopilot --state open --limit 1`
4. `git switch -c autopilot/issue-<n>`
5. Dispatch ONE specialized subagent matched to issue label
6. **Mandatory gates before commit**:
   - `cargo test -p <changed-crate>` green
   - `./scripts/tt-inspect-self.sh` no NEW high/critical findings
   - Iteration cost <$1.00 (from session ledger)
7. Open PR with audit log appended
8. Update `STATE.md`, exit. **Never auto-merge.** Human review mandatory.

**Safety properties**: one issue per iteration; one test crate per iteration; self-Inspect gate; two-layer cost fuse ($1 iteration + $25 weekly); PRs only; cron decides cadence (not the model).

### CI/CD (`.github/workflows/`)
Required for merge to `main`:
- `fmt-and-clippy` (60s)
- `rust-test-changed` (5min, dep-graph aware, skips unchanged crates)
- `rust-test-workspace` (12min, parallelized)
- `frontend-test` (5min)
- `inspect-self` (90s) — **the dogfood gate**, fails on new HIGH/CRITICAL findings
- `plan-self` (3min) — replays checked-in synthetic 10K-request workload, insta snapshot
- `secret-scan` (gitleaks + our own rule)
- `license-check` (cargo-deny: Apache/MIT/BSD/MPL only in OSS repo)
- `bindings-drift-check` (regenerate TS bindings, fail if differs from committed)

Advisory (don't block):
- `token-budget-telemetry` — reads `.claude/AUDIT.log` from branch, posts PR comment with $ spent
- `cargo-audit`

Separate scheduled:
- `provider-contract-tests.yml` (weekly, $5/wk cap, real providers, auto-opens issue with `provider-broken` label on failure)

### Repo Structure (Two-Repo, Confirmed)

```
github.com/tokentrimmer/
├── tokentrimmer/                 PUBLIC (Apache 2.0)
│   ├── Cargo.toml                workspace root
│   ├── crates/
│   │   ├── shared/               types, errors, Provider trait, RequestContext
│   │   ├── core/                 Axum app, routing, middleware
│   │   ├── cache/                L1 trait + Redis impl
│   │   ├── routing/              rule engine
│   │   ├── auth/                 API key validation
│   │   ├── telemetry/            OTel, audit hook
│   │   ├── config/               yaml/toml/env layering
│   │   ├── providers/{openai,anthropic,gemini,mistral,groq,together,openrouter,local}/
│   │   ├── inspect-core/         rule engine, tree-sitter harness
│   │   ├── inspect-rules-tier1/  the P0 rules
│   │   ├── plan-core/            replay engine, bootstrap
│   │   ├── cli/                  `tt` binary
│   │   └── ts-types/             schemars + ts-rs → bindings
│   ├── sdk-python/
│   ├── sdk-typescript/
│   ├── examples/
│   ├── AGENTS.md, CLAUDE.md (symlink), .claude/, .github/workflows/
│
└── tokentrimmer-cloud/           PRIVATE
    ├── pnpm-workspace.yaml
    ├── apps/{web,dashboard,docs}/
    ├── packages/{ui,api-client,tt-types}/
    ├── crates/{api,worker,inspect-rules-tier23}/
    ├── infra/
    └── AGENTS.md, .claude/
```

**Cross-language type sharing**: Rust structs are the source of truth. `schemars` derives JSON Schema; `ts-rs` emits TS bindings published as `@tokentrimmer/types`. `utoipa` emits OpenAPI; `openapi-typescript` generates `packages/api-client`. **Bindings drift check** is a required CI step.

## Dogfooding Strategy

The repo is the demo and the QA harness simultaneously.

| Phase | When | What |
|---|---|---|
| 0 | Week 1 | `.claude/AUDIT.log` starts. Every AI session logged. |
| 1 | Week 3 | Gateway in staging in front of Claude Code itself. `X-TokenTrimmer-Trace-Id` logged into AUDIT entries. Closes loop AI-session ↔ Gateway. |
| 2 | Week 6 | Gateway in front of GHA jobs that use LLMs. PR `token-budget-telemetry` reads from Gateway telemetry. |
| 3 | Week 14 | Inspect-self required gate in CI. Repo engineered to PASS its own rules. |
| 4 | Week 16 | Plan-self runs on routing-engine PRs. Catches regressions in cost projection. |
| 5 | Week 20 | README badge: "Inspect: 0 high · 0 critical · 7 medium". Weekly blog with cost reports from our own dashboard. |

**Auditable AI assistance** (four layers): local `.claude/AUDIT.log`; per-session `.claude/sessions/<id>/manifest.json` uploaded to R2 once dogfooding live; commit trailers (`AI-Session:`, `AI-Cost-USD:`, `AI-Trace-Id:`) validated by GHA; dashboard view at `/internal/sessions` (Week 20).

## Audit Guarantees (Auditable for All Tiers)

Five primitives, every tier:
1. **Per-request trace** — `X-TokenTrimmer-Trace-Id` header
2. **Cost transparency** — `X-TokenTrimmer-Cost-Usd` + `Baseline-Cost-Usd` + `Saved-Usd` headers
3. **Reconciliation report** — projected vs actual provider invoice, side-by-side
4. **Trust score** — 0-100, rolling variance between projected and actual; >90 = match, <70 = investigate
5. **Hash-chained audit log** — every entry: `(prev_hash, ts, actor, event, payload_hash, signature)`. Ed25519 per-org signing key. `tt audit verify` CLI walks the chain.

Events always logged: `org.*`, `member.*`, `api_key.*`, `provider_credential.*`, `route.*`, `plan.*`, `subscription.change`, `data.export.*`, `data.purge`, `auth.login{,_failed}`, `auth.magic_link.sent`.

**Storage tier escalation**:
- Free/Solo/Pro/Team: Postgres rows + hash chain
- **Scale**: + AWS S3 Object Lock in Compliance mode (NOT R2 — research finding; ~$10-20/mo COGS uplift)
- **Enterprise**: + customer's own S3 bucket sync (IAM role assumption), + SIEM-format export (CEF/LEEF)

OSS users get local `./tt-audit.log` with same hash-chain format; customer holds their own Ed25519 key.

## Email-Only Support Infrastructure

**Inbound addresses** (DNS-routed groups):
`support@`, `security@`, `contact@` (Enterprise), `billing@`, `abuse@`, `dpo@`, `legal@`, `press@`, `careers@`.

**Ticketing**: Gmail + canned responses through Week 19; Plain ($35/mo, plain.com) when volume hits 20 tickets/wk or first Team customer onboards.

**Resend templates (18 at launch)**: magic-link, welcome+first-API-call, weekly digest, monthly PDF, anomaly alert, billing receipt, payment failed (day 0/3/7), quota warning (80%), quota exceeded, key created/rotated/revoked, member invited/joined/removed, subscription change, data export ready, data purge scheduled/executed, support reply (via Plain), incident notification, trust score change.

**SLA tiers**:
- OSS/Free: community (Discord, GitHub Issues, best-effort)
- Solo/Pro: 48h business days
- Team: 24h
- Scale: 12h
- Enterprise: 4h for p1, 24h for p2, dedicated CSM email + Slack-Connect (still async)

**Channels**: Discord community, GitHub Issues per OSS repo, Starlight docs `docs.tokentrimmer.com` with `/troubleshooting` (30+ articles before launch), in-app help widget (compose-email form, NOT chat), UptimeRobot status page with RSS + in-app banner. **No phone, no live chat, ever.** The one exception: Enterprise p1 incident bridge call, max 4h response, written into contract.

**Free tier abuse mitigation**: GitHub OAuth required (>7d old account, >0 public commits/stars), Cloudflare Turnstile captcha, 60 req/min hard cap, 5K req/mo hard cap (no overage), no semantic cache writes (L1 only — blocks "free embedding generator" abuse), max 2 API keys + 2 provider creds per Free org, IP velocity check on signup.

## Risk Register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Scope creep delays launch | High | High | Week 11 hard checkpoint; documented cut order (PDF → anomaly → quality scoring → Inspect Tier 2 → fewer rules) |
| Inspect false positives erode trust | Medium | High | Start with 10 (not 15) rules; per-rule FP tracking from Week 14; severity downgrade pipeline |
| Provider API churn | Continuous | Medium | Weekly contract tests from Week 7; adapter version pinning; per-provider feature-flag rollback |
| **Cloudflare AI Gateway free tier undercuts us** | High | Medium-High | Differentiate on Inspect + Plan + trust score + hash-chained audit (no Cloudflare equivalent); position as "Helicone is observability, we're cost engineering with proof" |
| **R2 Object Lock gap blocks Scale/Enterprise audit promise** | Confirmed | Medium | Add AWS S3 for compliance customers; ~$10-20/mo per Scale/Enterprise; keep R2 for non-compliance assets |
| **Fly inter-region billing increase (Feb 2026)** | Confirmed | Medium | Single-region (`iad`) until $5K MRR; add `lhr` only when EU customers >20% of base |
| Token cost overrun in autonomous loops | Medium | Medium | $1/iteration + $25/week caps in `cost-cap-check.sh`; PAUSED sentinel; weekly cost-by-branch report |
| AI-generated code quality regression | Medium | High | Full CI gate on every autonomous PR (clippy, inspect-self, snapshots, integration, load smoke); human review required for auth/cache/billing/audit-log paths; signed commits enforced |
| Semantic cache embedding model price hike | Low-Medium | Medium | Design pipeline to allow swap to self-hosted BGE-small via Candle (spec §20.2 open Q); budget alert if embedding cost >$0.00015/cached request |
| Reconciliation drift dashboard vs invoices | Medium | High | Daily reconciliation job from Week 12; >2% delta shows "savings under recalculation" banner instead of wrong number; >5% requires postmortem |
| Solo founder burnout / single point of failure | High (solo) | Catastrophic | Quarterly 2-week capacity audit; hire #2 at $10K MRR not before; OSS contributors can pick up Tier 1 work; bus factor mitigation via thorough AGENTS.md per crate |

## Universal Verification Gate (`make ci-local`)

Every weekly close, run all of:
- [ ] `cargo fmt --check && cargo clippy --workspace -- -D warnings && cargo test --workspace`
- [ ] `cargo audit && cargo deny check`
- [ ] `pnpm typecheck && pnpm test && pnpm build` (every Astro app)
- [ ] `tt inspect tokentrimmer/ cloud/` — zero new high-severity
- [ ] Playwright e2e: signup → key → Gateway call → dashboard entry — green
- [ ] Latency smoke: p50 miss <30ms, p50 hit <5ms on fixed Fly size
- [ ] Reconciliation drift this week ≤2%
- [ ] Synthetic audit-row test: every state-changing endpoint produced a row
- [ ] Weekly LLM cost (autonomous loops) under cap
- [ ] Zero new Sentry WARN+ without ticket
- [ ] All checkpoint items for the current week checked

**If any red, week does not close — carries into next week's slack.**

## Critical Files To Create First

These are the load-bearing files; build them in Week 0 before any product code:

1. `/Users/iansimon/Developer/TokenTrimmer/.claude/settings.json` — hooks, permissions, security perimeter
2. `/Users/iansimon/Developer/TokenTrimmer/.claude/hooks/post-edit-scoped-check.sh` — single highest-ROI token saver
3. `/Users/iansimon/Developer/TokenTrimmer/.claude/hooks/pre-edit-guard.sh` — enforces secrets, size, cache-control at edit time
4. `/Users/iansimon/Developer/TokenTrimmer/AGENTS.md` — must stay <4K tokens, passes own rules
5. `/Users/iansimon/Developer/TokenTrimmer/Cargo.toml` — workspace root with all crates listed
6. `/Users/iansimon/Developer/TokenTrimmer/.github/workflows/ci.yml` — fmt, clippy, scoped tests, secret scan
7. `/Users/iansimon/Developer/TokenTrimmer/.github/workflows/inspect-self.yml` — the dogfood gate (Week 14 required)
8. `/Users/iansimon/Developer/TokenTrimmer/scripts/ralph-iteration.sh` — autonomous loop with test/inspect/cost gates
9. `/Users/iansimon/Developer/TokenTrimmer/.claude/agents/rust-crate-builder.md` — first subagent definition
10. `/Users/iansimon/Developer/TokenTrimmer/flake.nix` — locked toolchain (Rust 1.83, Node 20, pnpm 9, tree-sitter parsers)
11. `/Users/iansimon/Developer/TokenTrimmer/docker-compose.dev.yml` — local Postgres+pgvector, Redis, MinIO, mailpit, Ollama
12. `/Users/iansimon/Developer/TokenTrimmer/CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`

## Verification Plan

To verify the buildout end-to-end (post-Week 11 hard checkpoint):

```bash
# 1. Repo + harness health
cd tokentrimmer && nix develop --command bash -c "cargo test --workspace && cargo clippy --workspace -- -D warnings"
./scripts/tt-inspect-self.sh   # zero new high/critical

# 2. Gateway latency on Fly prod
oha -n 1000 -c 10 https://api.tokentrimmer.com/v1/chat/completions  # p50 miss <30ms, hit <5ms

# 3. End-to-end customer flow
# Browser: visit dashboard.tokentrimmer.com → magic link → Stripe $1 test → issue key → curl with key
curl https://api.tokentrimmer.com/v1/chat/completions \
  -H "Authorization: Bearer tt_live_..." \
  -d '{"model":"claude-3-5-sonnet","messages":[{"role":"user","content":"hi"}]}'
# Verify: response includes X-TokenTrimmer-Trace-Id, Cost-Usd, Saved-Usd headers
# Dashboard: request appears within 30s

# 4. Audit log integrity
tt audit verify --org $ORG_ID   # chain valid, signatures verify

# 5. Reconciliation
psql $DATABASE_URL -c "SELECT abs(reconciled_savings - claimed_savings) / claimed_savings AS drift FROM weekly_reconciliation ORDER BY week DESC LIMIT 4;"
# All <2%

# 6. Dogfood inspect
tt inspect tokentrimmer/ cloud/ --fail-on=high   # zero findings

# 7. Plan replay determinism
tt plan --config-diff cuts/test-diff.yaml --window 30d --seed 42 > out1.json
tt plan --config-diff cuts/test-diff.yaml --window 30d --seed 42 > out2.json
diff out1.json out2.json   # bit-identical
```

## Open Decisions Remaining

These need answers before week 10 (auth + billing implementation) but can be deferred from this plan:

1. **Embedding model**: OpenAI `text-embedding-3-small` (recommended, cheap, simple) vs self-hosted BGE-small via Candle (zero per-call but ops burden). Pricing pressure: ~$0.02/1M tokens for OpenAI as of May 2026. Stay with OpenAI for v1, design swap path.

2. **Worker queue**: Postgres-backed (Apalis crate) vs Redis-backed. Recommend Apalis on Postgres — one less moving part, matches spec's "all coordination through database" principle.

3. **Auth library**: Auth.js (Node) running as Astro Pages Functions vs BoxyHQ saml-jackson sidecar for v1.1 enterprise. Recommend Auth.js for magic-link v1, BoxyHQ for v1.1 SAML — keeps WorkOS cost ($125/conn/mo) deferred until enterprise volume justifies it.

4. **Ticketing platform**: Gmail+canned through Week 19, Plain ($35/mo) when volume justifies. No decision required until Week 18.

5. **Region rollout**: `iad` only at launch; `lhr` when EU customers >20% of base or first European Pro/Team customer; `syd` only on enterprise APAC demand.

6. **Enterprise floor price**: $24K/yr ARR ($2K/mo) as gap from Scale ($1,499). Could raise to $30K-50K if Scale gets priced higher.
