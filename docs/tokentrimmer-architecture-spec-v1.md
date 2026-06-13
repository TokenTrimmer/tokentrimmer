# TokenTrimmer — v1 Architecture Specification

**Status:** Draft 1
**Audience:** Solo founder (you), future contributors, future hires
**Scope:** Everything required to build, ship, and operate v1

---

## Table of Contents

1. Product overview
2. Architectural principles
3. System architecture
4. Gateway architecture
5. Inspect architecture
6. Plan architecture
7. Reporting architecture
8. Data architecture
9. Frontend architecture
10. Auth, billing & metering
11. Infrastructure & deployment
12. Open-core boundary
13. Repository structure
14. Build & deploy pipeline
15. Testing strategy
16. Security & privacy
17. Observability
18. Internal cost discipline
19. v1 roadmap & milestones
20. Risks & open questions
21. Appendices

---

## 1. Product overview

TokenTrimmer is the cost layer for LLM applications. It helps teams *see*, *plan*, and *optimize* every LLM token — before the call, during the call, and after the call. It is delivered as an open-source core (Apache 2.0) plus a hosted SaaS with fixed, public pricing.

**Four product pillars in v1:**

| Pillar | Surface | What it does |
|---|---|---|
| **Gateway** | OpenAI-compatible HTTP proxy | Runtime traffic: caching, routing, compression, attribution |
| **Inspect** | CLI + hosted CI integration | Static analysis of code repos for LLM cost waste |
| **Plan** | CLI + dashboard UI | Replay traffic against proposed configs, project savings before applying |
| **Reporting** | Dashboard + scheduled emails | Live metrics, monthly PDF executive summary, weekly digests, cost attribution |

All four ship at launch. Reporting and Plan are cross-cutting features that read from Gateway telemetry and Inspect findings.

**Out of scope for v1 (in roadmap):**
- Watch product (PR bot, IDE integration, real-time injection)
- Specialized fine-tuned models for in-house analysis
- Public dashboards
- Custom report builder
- White-label reports
- SOC-2 certification (path defined but not pursued in v1)

---

## 2. Architectural principles

These decisions are locked. Any change to these affects every subsequent section.

1. **Open core, Apache 2.0.** The Gateway core proxy is OSS. Differentiating intelligence (semantic cache, optimizer, dashboard, Plan engine, Reporting) is hosted. License the OSS components under Apache 2.0 for patent protection.
2. **OpenAI-compatible API is the contract.** All providers — Anthropic, Gemini, Mistral, Groq, Together, OpenRouter, Ollama, vLLM, LM Studio — are exposed behind one OpenAI-compatible request/response surface. Customers integrate by changing `base_url` only.
3. **Self-serve everything.** Fixed pricing tiers. Stripe Checkout signup. Cancel button in dashboard. Zero negotiation, zero demos, zero sales calls.
4. **Async-only support.** Email, Discord, GitHub Issues. No live chat. No phone.
5. **Privacy-preserving by default.** Request logs are metadata-only by default — no prompt or response bodies (only token counts, model, timing, route). Two paid features are the exceptions, and both are off unless enabled: the L2 semantic cache **stores full response bodies** (in Postgres, scoped by `org_id`) so it can replay them on a hit, and any feature that generates embeddings — L2 cache lookups and retrieval — **sends the prompt text to OpenAI** (`text-embedding-3-small`) to produce the embedding vector. A customer can additionally opt into **encrypted request/response body capture** for `/logs` replay (per-org, off by default): bodies are encrypted at rest, redacted before storage, size-capped, retention-bounded, and flagged on responses with `X-TokenTrimmer-Captured` (see §16.1).
6. **Latency budget.** Target sub-30ms gateway overhead (p50) on cache misses. Sub-5ms (p50) on cache hits. Multi-region deployment is non-negotiable.
7. **Cost discipline on our side.** TokenTrimmer's own LLM and infra costs must be predictable and bounded. We dogfood our own product.
8. **Honest measurement.** No inflated savings claims. Confidence intervals everywhere. The "savings" number on the dashboard must reconcile to provider invoices.

---

## 3. System architecture

```
                          ┌─────────────────────────┐
                          │   Customer's codebase   │
                          └────────────┬────────────┘
                                       │
                  ┌────────────────────┼────────────────────┐
                  │ (runtime)          │ (static)           │ (preview)
                  ▼                    ▼                    ▼
        ┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐
        │     Gateway      │  │     Inspect      │  │       Plan       │
        │  (Rust / Axum)   │  │ (Rust CLI + svc) │  │  (Rust service)  │
        └────────┬─────────┘  └────────┬─────────┘  └────────┬─────────┘
                 │                     │                     │
                 │   telemetry         │   findings          │   simulations
                 │                     │                     │
                 ▼                     ▼                     ▼
        ┌──────────────────────────────────────────────────────────┐
        │  Data layer: Postgres + pgvector · Redis · (CH later)    │
        └────────────────────────────┬─────────────────────────────┘
                                     │
                                     ▼
                          ┌──────────────────────┐
                          │     Reporting        │
                          │  (Astro + Solid)     │
                          │  + scheduled jobs    │
                          │  + PDF/email render  │
                          └──────────────────────┘
```

**Component responsibilities:**

- **Gateway:** Sits in the request path. Owns latency budget. Writes per-request telemetry to data layer. Reads routing config and cached responses from data layer.
- **Inspect:** Pulls customer's repo (or runs locally as CLI), runs detection rules, writes findings to data layer. Can call frontier LLMs for deeper analysis (Tier 3 rules).
- **Plan:** Reads historical telemetry from data layer, replays against a proposed config, writes simulation results. Surfaces results in CLI and dashboard.
- **Reporting:** Renders dashboards from data layer. Runs scheduled jobs for digest emails and monthly PDFs. Owns the Astro frontend.

The data layer is the integration point. Components do not call each other directly; they communicate through the database.

---

## 4. Gateway architecture

The Gateway is the only component in the synchronous request path. It must be fast, reliable, and small.

### 4.1 Module layout (Rust workspace)

```
gateway/
├── crates/
│   ├── core/                  # request handling, routing, middleware
│   ├── providers/             # adapter per provider
│   │   ├── openai/
│   │   ├── anthropic/
│   │   ├── gemini/
│   │   ├── mistral/
│   │   ├── groq/
│   │   ├── together/
│   │   ├── openrouter/
│   │   └── local/             # ollama, vllm, lm-studio
│   ├── cache/                 # exact + semantic cache
│   ├── routing/               # rule engine + classifier hook
│   ├── auth/                  # API key validation
│   ├── telemetry/             # OTel, metrics, request logs
│   ├── config/                # YAML/TOML config parsing
│   └── shared/                # types, errors, utilities
├── bin/
│   └── tokentrimmer/          # the binary
└── Cargo.toml
```

### 4.2 The `Provider` trait

The contract every provider adapter implements. Designed so that adding a new provider is a self-contained crate.

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    /// Provider identifier ("openai", "anthropic", etc.)
    fn id(&self) -> &str;

    /// Translate an OpenAI-format chat completion request into a
    /// provider-native HTTP request and execute it.
    async fn chat_completion(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<ChatCompletionResponse, ProviderError>;

    /// Streaming variant. Returns an SSE stream of OpenAI-format chunks.
    async fn chat_completion_stream(
        &self,
        req: ChatCompletionRequest,
        ctx: &RequestContext,
    ) -> Result<BoxStream<'static, Result<ChatCompletionChunk, ProviderError>>, ProviderError>;

    /// Embeddings (uniform OpenAI format).
    async fn embeddings(
        &self,
        req: EmbeddingsRequest,
        ctx: &RequestContext,
    ) -> Result<EmbeddingsResponse, ProviderError>;

    /// Model pricing — for cost calculation. Cached, refreshed daily.
    fn pricing(&self, model: &str) -> Option<ModelPricing>;
}
```

`RequestContext` carries the authenticated customer, the originating API key, trace IDs, and routing decisions made upstream.

### 4.3 Request flow

```
1. HTTP request arrives at edge (Fly.io anycast)
2. Auth middleware:
     - Extract X-TokenTrimmer-Key header
     - Validate against Postgres (cached in Redis for 60s)
     - Reject if invalid or quota exceeded
3. Parse OpenAI-format request body
4. Routing engine:
     - Evaluate rules in priority order
     - Select target provider + model
     - Apply transformations (compression, system prompt injection)
5. Cache lookup:
     - L1: Redis exact-match (SHA-256 of normalized request)
     - L2: pgvector semantic match (embed prompt, cosine similarity > threshold)
     - If hit: return cached response, record hit
6. Provider call:
     - Translate to provider-native format
     - Forward request (with timeout from config)
     - Handle errors (retry, fallback per config)
7. Response processing:
     - Translate back to OpenAI format
     - Compute cost from token counts × pricing table
     - Write to cache (if eligible)
     - Emit telemetry event
8. Return to client
```

For streaming, steps 6–8 happen concurrently — the response streams through with telemetry emitted on stream close.

### 4.4 Caching architecture

**L1 — Exact match (Redis / Upstash):**
- Key: SHA-256 of `(model, messages, temperature, top_p, max_tokens, tools, response_format)`
- Value: serialized response + token counts
- TTL: configurable per route, default 1 hour
- Eviction: LRU within Upstash limits

**L2 — Semantic match (pgvector):**
- Embed a canonicalized representation of the conversation: the system prompt plus the ordered user/assistant messages (tool messages omitted) — embedding only the last user message caused different conversations sharing a final turn to collide
- Use `text-embedding-3-small` (cheap, fast, 1536 dims)
- Insert into `cache_entries` with HNSW index
- Lookup: cosine similarity, threshold per route (default 0.92)
- Storage: response JSON, model used, token counts, hit count, expires_at
- Per-org isolation: every cache entry has `org_id`; queries scope to org
- Privacy: cache stores the **full response JSON** and the prompt embedding; it does not persist the original prompt text, but generating the embedding sends that prompt text — the system prompt plus the conversation messages (canonicalized) — to OpenAI for `text-embedding-3-small`

**What is never cached:**
- Requests with `stream: true` and `cache: false` header
- Requests above a token threshold (configurable, default 100k)
- Requests with tools/functions (unless explicitly enabled — too much risk of stale tool definitions)
- Requests within a user-defined "no cache" route

### 4.5 Routing engine

Rule-based, evaluated in priority order. Rules are stored per-org in Postgres and hot-reloaded on change.

**Rule shape (YAML/JSON):**

```yaml
routes:
  - name: "cheap-for-short"
    priority: 100
    when:
      messages.total_tokens: { lt: 200 }
      model: { in: ["gpt-4o", "claude-3-5-sonnet"] }
    then:
      target_model: "claude-3-5-haiku"
      cache:
        enabled: true
        ttl: 86400
        semantic_threshold: 0.92

  - name: "local-for-classification"
    priority: 200
    when:
      messages.last.content: { matches: "(classify|categorize|extract)" }
      messages.total_tokens: { lt: 500 }
    then:
      target_model: "ollama/llama3:8b"
      fallback: "gpt-4o-mini"

  - name: "default"
    priority: 0
    when: { always: true }
    then:
      target_model: "${request.model}"   # pass through
```

Conditions support: token counts, regex on content, model name, header presence, time-of-day, budget remaining, custom tags.

**Classifier hook (v1.1+):** routes can delegate decisions to a small classifier model. The classifier is a Tier 2 specialized model (e.g., a fine-tuned Llama 3.1 8B) called via the same Provider trait. Cached aggressively.

### 4.6 Streaming (SSE)

LLM responses are mostly streamed. Streaming must pass through with minimal buffering.

- Gateway accepts `Accept: text/event-stream` and forwards as SSE
- Provider streams are converted chunk-by-chunk to OpenAI format
- Per-chunk overhead must be negligible (target: under 1ms per chunk)
- Token counting happens incrementally; cost is finalized on stream close
- If stream is cut (client disconnect), partial cost is still recorded
- Cached responses can be "fake-streamed" back to clients that requested streaming — split cached response into chunks with small delays

### 4.7 Provider adapters — v1 launch list

| Provider | Status | Native or compatible |
|---|---|---|
| OpenAI | Native | — |
| Anthropic | Native (custom adapter) | — |
| Google Gemini | Native (custom adapter) | — |
| Mistral | Compatible (minor tweaks) | OpenAI-compatible |
| Groq | Compatible | OpenAI-compatible |
| Together AI | Compatible | OpenAI-compatible |
| OpenRouter | Compatible | OpenAI-compatible |
| Ollama | Compatible | OpenAI-compatible |
| vLLM | Compatible | OpenAI-compatible |
| LM Studio | Compatible | OpenAI-compatible |

**Heavy lifting is Anthropic and Gemini adapters.** Both have non-OpenAI message formats and streaming protocols. Both need careful handling of:
- System prompts (Anthropic separates them; Gemini uses `systemInstruction`)
- Tool/function calling format differences
- Vision/multimodal input differences
- Streaming chunk format
- Token counting (different tokenizers)

Provider pricing is loaded from a `pricing.toml` file in the binary, updateable via config refresh from a public URL.

### 4.8 Rate limiting & quotas

Two layers:
- **Per-API-key:** RPM and TPM limits, configurable per key
- **Per-organization:** monthly request quota tied to subscription tier

Implemented in Redis using token bucket algorithm. 429 responses include `Retry-After` header and current quota state in body.

### 4.9 Error handling & fallback

Each route can declare a fallback chain. On provider error (5xx, timeout), Gateway tries each fallback in order before returning an error to the client.

Errors are classified:
- **Retriable:** 429, 5xx, timeout, connection reset → exponential backoff with jitter
- **Non-retriable:** 400, 401, 403 from upstream → return to client immediately
- **Internal:** Gateway bugs → 500 with trace ID, logged loudly

All errors emit telemetry events for the dashboard.

### 4.10 Configuration

Config is layered, highest priority wins:
1. Default config baked into binary
2. `tokentrimmer.yaml` (or `.toml`) in working directory
3. Environment variables (`TT_*`)
4. Per-org config fetched from API (hosted mode only)
5. Per-request headers (`X-TokenTrimmer-Cache: false`, etc.)

OSS mode uses layers 1–3. Hosted mode adds layer 4.

---

## 5. Inspect architecture

Inspect is the static analyzer. It scans code repositories for LLM cost waste and produces actionable findings.

### 5.1 Two deployment modes

- **CLI (OSS):** `tt inspect /path/to/repo` runs locally, outputs a markdown report. No data leaves the user's machine unless they opt in.
- **Hosted (paid):** runs in CI via GitHub Action, stores findings in the dashboard, integrates with Plan to project savings, integrates with Gateway telemetry to verify which findings are validated by real traffic.

### 5.2 Rule engine — three tiers

Detection rules are tagged with a tier indicating how they execute:

| Tier | Mechanism | Cost per scan | Speed | Examples |
|---|---|---|---|---|
| 1 | Deterministic (AST + regex) | $0 | < 1s | "uses gpt-4 for sub-200-token outputs", "no `cache_control` on >1024-token Anthropic system prompts", "no AGENTS.md present" |
| 2 | Small specialized model | < $0.01 | seconds | "prompt is verbose", "two prompts are semantically redundant", "AGENTS.md section is malformed" |
| 3 | Frontier model (Claude/GPT-4) | $0.05–$0.50 | tens of seconds | "this agent architecture should be refactored to planner/executor", "this codebase pattern would benefit from MCP server X" |

CI mode runs Tier 1 + Tier 2 on every PR. Tier 3 runs on scheduled "deep scans" (weekly) or on user demand.

### 5.3 Rule shape

```yaml
- id: "anthropic-missing-prompt-cache"
  tier: 1
  category: "caching"
  severity: "high"
  languages: ["python", "typescript"]
  description: "Long Anthropic system prompts without cache_control"
  detect:
    type: "ast"
    pattern: |
      anthropic.messages.create(
        system=$SYSTEM_PROMPT,
        ...
      ) where token_count($SYSTEM_PROMPT) > 1024
        and not has_cache_control($SYSTEM_PROMPT)
  recommend:
    summary: "Enable Anthropic prompt caching on this system prompt"
    expected_savings: "up to 90% on input tokens for cached portions"
    fix_template: |
      system=[
        {
          "type": "text",
          "text": $SYSTEM_PROMPT,
          "cache_control": {"type": "ephemeral"}
        }
      ]
    docs_url: "https://tokentrimmer.com/docs/rules/anthropic-missing-prompt-cache"
```

Rules live in a separate repository, `tokentrimmer/rules`, versioned independently from the binary. Rules can be loaded from disk, from the rules repo, or from a customer's private rule repo.

### 5.4 Language support — v1

- **Python** (primary — most LLM code lives here)
- **TypeScript / JavaScript** (secondary — Vercel AI SDK, LangChain.js, custom)

Parsing via tree-sitter. Rules can mix tree-sitter queries (precise) with regex (loose) as needed.

Other languages (Go, Rust, Ruby, Java) deferred to v2 based on demand.

### 5.5 Output formats

**Markdown report** (CLI default):
```markdown
# TokenTrimmer Inspect Report

Scanned: /path/to/repo at <commit_sha>
Findings: 14 (3 high, 8 medium, 3 low)
Estimated annual savings if all addressed: $4,872

## High severity

### anthropic-missing-prompt-cache — src/chat.py:23
Long Anthropic system prompt without cache_control.
Expected savings: up to 90% on input tokens for cached portions.
Suggested fix:
  <diff snippet>
Docs: https://tokentrimmer.com/docs/rules/anthropic-missing-prompt-cache

[... more findings ...]
```

**JSON output** (for CI consumption):
```json
{
  "scan_id": "uuid",
  "repo": "...",
  "commit": "...",
  "findings": [
    {
      "rule_id": "anthropic-missing-prompt-cache",
      "severity": "high",
      "file": "src/chat.py",
      "line": 23,
      "estimated_annual_savings_usd": 1240,
      "fix_diff": "...",
      "confidence": 0.95
    }
  ],
  "summary": {
    "total_findings": 14,
    "estimated_annual_savings_usd": 4872
  }
}
```

**GitHub Action output:** posts a check-run summary on the PR with collapsed-by-default detail.

### 5.6 GitHub Action integration

Published as `tokentrimmer/inspect-action@v1`. Customer adds it to `.github/workflows/`:

```yaml
- name: TokenTrimmer Inspect
  uses: tokentrimmer/inspect-action@v1
  with:
    token: ${{ secrets.TOKENTRIMMER_TOKEN }}
    fail-on: high   # high | medium | low | never
```

Authenticated against the hosted Inspect service for Tier 2/3 rules. Tier 1 rules run locally even without authentication.

### 5.7 v1 launch rules

Initial 15 P0 rules cover the highest-impact, highest-confidence detections. Full catalog is maintained in `tokentrimmer/rules` and tracked in a separate document (`inspect-rule-catalog.md`). Categories represented at launch:

- Model selection (3 rules)
- Anthropic prompt caching (1 rule, very high impact)
- Prompt bloat (2 rules)
- AGENTS.md / CLAUDE.md (2 rules)
- Caching opportunities (2 rules)
- Output handling (2 rules)
- LLM-doing-classical-work (2 rules)
- MCP candidates (1 rule, basic)

---

## 6. Plan architecture

Plan is the simulator. It answers "what would have happened if I had used this config instead?"

### 6.1 Mental model

Plan is **Terraform plan for LLM configs.** A user proposes a change (new route, different cache settings, model swap). Plan replays the last N days of traffic against the proposed change and reports the projected impact with confidence intervals.

### 6.2 Inputs

- Current organization config (active routes, cache settings)
- Proposed config diff (added/changed/removed routes)
- Time window (default: last 30 days)
- Sample mode (default: 100% of traffic; can downsample for speed)

### 6.3 Replay engine

For each historical request in the window:
1. Reconstruct the request (from telemetry — only metadata, not raw bodies unless customer has opted into full logging)
2. Apply proposed config to determine new route + target model
3. Determine the projected outcome:
   - **Cache hit projection:** would the new cache settings have served this from cache? Use embeddings stored in the historical request log.
   - **Cost projection:** apply new model pricing × historical token counts (adjusted for cache hits)
   - **Latency projection:** look up provider latency distributions from historical data
4. Sum across the window
5. Compute confidence intervals via bootstrap sampling

### 6.4 Quality risk scoring

A cheaper model only saves money if it produces acceptable output. Plan must estimate quality risk.

**For requests where we have historical output from the flagship model:**
- Sample a percentage of those requests
- Re-run them through the proposed cheaper model
- Use an LLM-as-judge (Tier 3) to score the divergence
- Aggregate into a risk band (LOW / MEDIUM / HIGH)

**For requests where we have no historical baseline:**
- Mark as "unverified" in the report
- Suggest running an A/B test before applying

Customer can configure judge model (or supply their own custom judge) and sampling rate.

### 6.5 Output

CLI (`tt plan`):
```
Proposed changes:
  ~ route: messages.total_tokens < 200 → claude-3-haiku (was: claude-3-5-sonnet)
  + cache: semantic, ttl=86400, threshold=0.92

Projected impact (last 30 days, 1,234,567 requests):
  Cost:           $4,247.13 → $1,683.42  (-60.4%, save $2,563.71/mo)
                  95% CI: [$2,401.18, $2,726.24]
  Latency p50:    412ms → 287ms          (-30.3%)
  Cache hit rate: 0% → 41.2% (projected)
  Quality risk:   LOW
    Sampled 1,000 requests for quality comparison
    96.8% acceptable, 3.2% flagged (sample diffs available)

Apply this plan? [y/N]
```

Dashboard UI shows the same content with interactive drill-down: which specific routes drove the savings, which requests were flagged for quality, ability to export the flagged set as a regression test suite.

### 6.6 Apply

If approved, the diff is applied to the org's active config. Gateway hot-reloads within 30 seconds. The plan record is preserved for later comparison against actual results (which Reporting surfaces as "projected vs actual").

### 6.7 What makes Plan defensible

- **Honest confidence intervals.** Plan never reports a point estimate without bounds.
- **Quality scoring built in.** Cost savings alone are misleading without quality risk.
- **Closed loop with Reporting.** Projected vs actual comparison builds trust over time.
- **Replay uses real traffic.** Not synthetic benchmarks, not vendor-supplied numbers.

---

## 7. Reporting architecture

Reporting is the user-facing layer that turns raw telemetry into something a customer can show their boss.

### 7.1 Live dashboard

Pages, in order of importance:

1. **Overview** — current month spend, savings month-to-date, cache hit rate, top 5 cost drivers
2. **Cost explorer** — drill down by API key, model, route, custom tag, time period
3. **Routes** — list of active routes, hit rates, savings per route
4. **Cache** — hit rates over time, top cached patterns (no raw prompts shown)
5. **Inspect findings** — open findings, severity distribution, projected savings if addressed
6. **Plan history** — past simulations, applied vs unapplied, projected vs actual

Built with Astro + Solid islands. Server-rendered shell, interactive islands for charts and filters. Charts via `chart.js` or `uPlot` (uPlot is faster for time-series, recommended).

### 7.2 Monthly executive PDF

Auto-generated on the 1st of each month, emailed to all team members.

**One page, front and back.**

Front:
- Headline: "You saved $X this month with TokenTrimmer"
- ROI line: "$X saved vs $Y subscription = Zx return"
- Spend trend chart (last 6 months)
- Top 3 cost drivers with sparklines

Back:
- Cache performance summary
- Top 3 Inspect findings (with projected savings if addressed)
- Optimizations applied during the month (Plan history)
- Suggested next action

Generated by rendering a dedicated `/internal/reports/exec-monthly` route in the Astro app to PDF via `@playwright/browser` in a scheduled worker.

### 7.3 Weekly digest email

Plain text (with optional HTML), sent every Monday morning local time per org.

Contents:
- Last week's spend vs previous week
- Savings, cache hit rate
- Any anomalies detected (spend spikes, error rate jumps, cache rate drops)
- Open Inspect findings count, severity
- Reminder of any unapplied Plans

Sent via Resend.

### 7.4 Cost attribution

Every request carries up to three labels:
- API key (always)
- `X-TokenTrimmer-Tag` header (free-form, e.g., "feature=chat-support", "user=internal-123")
- Route name (assigned by routing engine)

Dashboard surfaces aggregations by all three. Customers can define custom tag taxonomies in dashboard settings.

### 7.5 Saved $$ tracking

The headline metric. Computed as:

```
savings = sum over requests:
    (baseline_cost - actual_cost)
where baseline_cost = cost of the request if routed to the customer's
                     declared "baseline model" with no cache
```

Customer declares baseline model per org (default: GPT-4o or Claude Sonnet, whichever was their first-used flagship). This is honest, defensible, and reconciles to provider invoices.

### 7.6 Data export

- CSV export of request logs (within retention window)
- JSON export of Plan records
- Webhooks for cost events (budget threshold, anomaly, etc.)
- (v2) API for BI integration

### 7.7 Anomaly detection

Simple seasonality-aware z-score on hourly spend. Triggers webhook + dashboard notification when spend deviates > 3σ from forecast.

Implemented as a scheduled job in the worker service. No ML needed in v1 — a moving average with seasonal decomposition is sufficient.

---

## 8. Data architecture

### 8.1 Storage choices

| Store | Use | v1 / later |
|---|---|---|
| **Postgres (Neon)** | Users, orgs, API keys, billing, routes, budgets, Inspect findings, Plan records, semantic cache (pgvector) | v1 |
| **Redis (Upstash)** | L1 exact-match cache, rate limit counters, session tokens | v1 |
| **Cloudflare R2** | PDF reports, large export files | v1 |
| **ClickHouse (self-hosted on Hetzner)** | High-volume request logs | v2 (when Postgres can't handle the write rate) |

### 8.2 Postgres schema — core tables

(Abridged. Indexes and constraints elided for brevity.)

```sql
-- Identity
CREATE TABLE users (
  id          UUID PRIMARY KEY,
  email       TEXT UNIQUE NOT NULL,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE orgs (
  id          UUID PRIMARY KEY,
  name        TEXT NOT NULL,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  baseline_model TEXT NOT NULL DEFAULT 'gpt-4o'
);

CREATE TABLE org_members (
  org_id      UUID REFERENCES orgs(id),
  user_id     UUID REFERENCES users(id),
  role        TEXT NOT NULL,    -- owner | admin | member
  PRIMARY KEY (org_id, user_id)
);

-- API keys (TokenTrimmer's own)
CREATE TABLE api_keys (
  id          UUID PRIMARY KEY,
  org_id      UUID REFERENCES orgs(id),
  label       TEXT NOT NULL,
  prefix      TEXT NOT NULL,            -- shown in UI, e.g. "tt_live_abc"
  secret_hash TEXT NOT NULL,            -- argon2 hash
  created_at  TIMESTAMPTZ NOT NULL,
  last_used_at TIMESTAMPTZ,
  revoked_at  TIMESTAMPTZ
);

-- Provider credentials (customer's keys to OpenAI, Anthropic, etc.)
CREATE TABLE provider_credentials (
  id          UUID PRIMARY KEY,
  org_id      UUID REFERENCES orgs(id),
  provider    TEXT NOT NULL,            -- openai | anthropic | gemini | ...
  label       TEXT NOT NULL,
  secret_enc  BYTEA NOT NULL,           -- encrypted via libsodium
  created_at  TIMESTAMPTZ NOT NULL
);

-- Routing
CREATE TABLE routes (
  id          UUID PRIMARY KEY,
  org_id      UUID REFERENCES orgs(id),
  name        TEXT NOT NULL,
  priority    INT NOT NULL,
  config      JSONB NOT NULL,           -- the rule body
  enabled     BOOLEAN NOT NULL DEFAULT true,
  created_at  TIMESTAMPTZ NOT NULL,
  updated_at  TIMESTAMPTZ NOT NULL
);

-- Request telemetry (summary; full logs go to ClickHouse later)
CREATE TABLE request_logs (
  id               UUID PRIMARY KEY,
  org_id           UUID REFERENCES orgs(id),
  api_key_id       UUID REFERENCES api_keys(id),
  ts               TIMESTAMPTZ NOT NULL,
  provider         TEXT NOT NULL,
  model            TEXT NOT NULL,
  input_tokens     INT NOT NULL,
  output_tokens    INT NOT NULL,
  cost_usd         NUMERIC(12,6) NOT NULL,
  baseline_cost_usd NUMERIC(12,6) NOT NULL,
  cached           BOOLEAN NOT NULL,
  cache_layer      TEXT,                 -- l1 | l2 | null
  route_id         UUID REFERENCES routes(id),
  latency_ms       INT NOT NULL,
  status           INT NOT NULL,
  tag              TEXT,
  error_class      TEXT
);

CREATE INDEX request_logs_org_ts ON request_logs (org_id, ts DESC);

-- Semantic cache (pgvector)
CREATE EXTENSION IF NOT EXISTS vector;
CREATE TABLE cache_entries (
  id          UUID PRIMARY KEY,
  org_id      UUID REFERENCES orgs(id),
  embedding   vector(1536) NOT NULL,
  response    JSONB NOT NULL,
  model       TEXT NOT NULL,
  input_tokens INT NOT NULL,
  output_tokens INT NOT NULL,
  hit_count   INT NOT NULL DEFAULT 0,
  expires_at  TIMESTAMPTZ NOT NULL,
  created_at  TIMESTAMPTZ NOT NULL
);
CREATE INDEX ON cache_entries USING hnsw (embedding vector_cosine_ops);

-- Inspect
CREATE TABLE inspect_runs (
  id          UUID PRIMARY KEY,
  org_id      UUID REFERENCES orgs(id),
  repo_url    TEXT,
  commit_sha  TEXT,
  started_at  TIMESTAMPTZ NOT NULL,
  finished_at TIMESTAMPTZ,
  status      TEXT NOT NULL
);

CREATE TABLE inspect_findings (
  id          UUID PRIMARY KEY,
  run_id      UUID REFERENCES inspect_runs(id),
  rule_id     TEXT NOT NULL,
  severity    TEXT NOT NULL,
  file_path   TEXT NOT NULL,
  line        INT,
  message     TEXT NOT NULL,
  fix_diff    TEXT,
  estimated_annual_savings_usd NUMERIC(12,2)
);

-- Plan
CREATE TABLE plan_runs (
  id          UUID PRIMARY KEY,
  org_id      UUID REFERENCES orgs(id),
  name        TEXT NOT NULL,
  config_diff JSONB NOT NULL,
  window_start TIMESTAMPTZ NOT NULL,
  window_end  TIMESTAMPTZ NOT NULL,
  projected_savings_usd NUMERIC(12,2),
  projected_savings_ci_low NUMERIC(12,2),
  projected_savings_ci_high NUMERIC(12,2),
  quality_risk TEXT,                    -- low | medium | high
  applied_at  TIMESTAMPTZ,
  status      TEXT NOT NULL,
  created_at  TIMESTAMPTZ NOT NULL
);

-- Subscriptions (mirrored from Stripe)
CREATE TABLE subscriptions (
  org_id              UUID PRIMARY KEY REFERENCES orgs(id),
  stripe_subscription_id TEXT NOT NULL,
  stripe_customer_id  TEXT NOT NULL,
  tier                TEXT NOT NULL,    -- hobby | pro | scale
  status              TEXT NOT NULL,
  current_period_end  TIMESTAMPTZ NOT NULL
);
```

### 8.3 Redis usage

- `cache:{org_id}:{hash}` → exact-match cache entries (with TTL)
- `ratelimit:{api_key_id}:{window}` → token bucket counters
- `apikey:{prefix}` → validated key context (60s TTL)
- `quota:{org_id}:{month}` → monthly request counter

### 8.4 Data retention

Per tier:

| Tier | Request log retention | Cache TTL max |
|---|---|---|
| Hobby | 7 days | 24 hours |
| Pro | 30 days | 7 days |
| Scale | 90 days | 30 days |
| Self-hosted | customer's choice | customer's choice |

A nightly job purges expired records.

---

## 9. Frontend architecture

### 9.1 What gets built

- **Marketing site** (`tokentrimmer.com`) — landing page, pricing, blog
- **Docs** (`docs.tokentrimmer.com`) — Starlight (Astro)
- **App** (`dashboard.tokentrimmer.com`) — dashboard, settings, billing

Three Astro projects under one monorepo, deployed separately.

### 9.2 Tech choices

- **Astro 5+** for everything
- **Starlight** for docs
- **Solid.js islands** for interactive components in the dashboard (charts, filter inputs, real-time meters)
- **Tailwind CSS** for styling, with **shadcn-svelte** equivalents or **Kobalte** for accessible Solid primitives
- **uPlot** for time-series charts (lighter than Chart.js, faster on big series)
- **@tanstack/solid-query** for dashboard data fetching
- **Auth.js** with magic-link email (no passwords in v1)

### 9.3 Dashboard structure

```
app/
├── src/
│   ├── pages/
│   │   ├── index.astro              # overview
│   │   ├── costs.astro
│   │   ├── cache.astro
│   │   ├── routes.astro
│   │   ├── inspect.astro
│   │   ├── plan/
│   │   │   ├── index.astro
│   │   │   └── [id].astro
│   │   ├── reports.astro
│   │   └── settings/
│   │       ├── billing.astro
│   │       ├── api-keys.astro
│   │       ├── providers.astro
│   │       └── team.astro
│   ├── components/
│   │   ├── astro/                   # static components
│   │   └── solid/                   # interactive islands
│   ├── lib/
│   │   ├── api.ts                   # client for hosted backend
│   │   └── auth.ts
│   └── styles/
└── astro.config.mjs
```

### 9.4 Marketing site

- Pure Astro with MDX for blog
- One landing page (the launch copy from the launch package)
- `/pricing` with the four-tier table
- `/changelog` (auto-generated from GitHub releases)
- `/blog` with first technical post within 2 weeks of launch

---

## 10. Auth, billing & metering

### 10.1 Auth

- **Magic link email** via Resend (no passwords)
- **Auth.js** (next-auth's framework-agnostic core) running as a small Hono service or directly in the dashboard via Astro endpoints
- **Sessions** stored in Postgres, signed with rotating HMAC secret
- **API key auth** for the Gateway (separate from web sessions) — argon2 hashed, prefix-displayed format `tt_live_xxxx`

Optional in v2: SSO (Google, GitHub) and SAML for self-hosted Pro.

### 10.2 Billing

Stripe is the source of truth. Webhooks sync subscription state into the `subscriptions` table.

- **Checkout:** Stripe Checkout for new subscriptions
- **Portal:** Stripe Customer Portal for plan changes, payment method updates, cancel
- **Webhooks:** `customer.subscription.*`, `invoice.payment_*` events update local state
- **Failed payments:** dashboard banner + email; soft downgrade to read-only after grace period

### 10.3 Usage metering

Each request increments the org's monthly counter in Redis. At end of month, Postgres is the system of record.

Overage handling:
- Soft cap at 100% of tier (warning email)
- Hard cap at 110% of tier with overage billing enabled (default off for Hobby, default on for Pro/Scale)
- Overage rate: $0.0001/request beyond tier
- Stripe usage-based subscription item handles overage billing automatically

### 10.4 Provider credential handling

Customer's OpenAI/Anthropic/etc keys are encrypted at rest using libsodium (XChaCha20-Poly1305). Encryption key stored in Fly.io secrets, rotated annually.

Keys are decrypted only in the Gateway request path, in memory, never logged.

For OSS self-host: same scheme, key stored in environment variable.

---

## 11. Infrastructure & deployment

| Service | Hosted on | Why |
|---|---|---|
| Gateway (Rust) | Fly.io | Multi-region anycast, Rust native, persistent connections |
| Marketing site | Cloudflare Pages | Free, global CDN, perfect for static Astro |
| Docs | Cloudflare Pages | Same |
| App (dashboard) | Cloudflare Pages | Same — server endpoints via Pages Functions |
| Backend API (auth, billing, Inspect, Plan) | Fly.io (separate app) | Rust service, sibling of Gateway |
| Worker (scheduled jobs, PDF render, email) | Fly.io machine | On-demand, can scale to zero |
| Postgres | Neon | Serverless, branching for dev environments |
| Redis | Upstash | Pay-per-request, HTTP API, no servers to manage |
| Object storage | Cloudflare R2 | No egress fees |
| Email | Resend | Developer-friendly, great deliverability |
| Payments | Stripe | The only choice |
| Error tracking | Sentry | Free tier covers v1 |
| Status page | Statuspage (free) or self-hosted | Public uptime page |
| Domain & DNS | Cloudflare | Free, DDoS protection |
| Code | GitHub | OSS + private repos |
| CI | GitHub Actions | Free for OSS, generous for private |
| Container registry | GHCR (GitHub) | Free, integrated with GitHub Actions |

**Regions for v1:** Fly.io deployment in `iad` (US East), `lhr` (London), `syd` (Sydney). Anycast routes users to nearest.

**Cost estimate for v1 at MVP:** under $80/month.

**Cost estimate at first 100 paying customers:** approximately $300–$500/month, dominated by Neon and Upstash usage.

---

## 12. Open-core boundary

This is the single most important commercial decision. Document it explicitly so it doesn't drift.

### 12.1 What is open source (Apache 2.0)

- Core Rust Gateway proxy (request handling, routing engine, all provider adapters)
- Exact-match (L1) cache implementation
- Basic config-driven routing
- Local model support (Ollama, vLLM, LM Studio)
- Inspect CLI binary with Tier 1 rules
- Rules repository (Tier 1 rules, schema definitions)
- Docker image builds, docker-compose example
- All SDKs (Python, TypeScript) for talking to the Gateway
- Documentation source

### 12.2 What is closed (hosted SaaS only)

- Semantic cache (L2) with pgvector and managed embedding pipeline
- Plan engine (replay, quality risk scoring, confidence intervals)
- Dashboard (Astro app)
- Reporting (PDF generation, email digests, anomaly detection)
- Inspect Tier 2 and Tier 3 rules (the ones requiring LLM calls)
- GitHub Action runner backend
- Multi-region managed deployment
- Team features (RBAC, audit logs, SSO)
- Customer support channels

### 12.3 Why this split works

The OSS version is a complete, usable LLM gateway. Self-hosters get real value. But the hosted version offers operational simplicity and the intelligence layer (semantic cache, Plan, automated Reporting, the smarter Inspect rules).

A customer can run the OSS proxy in their own infra and pay nothing forever. They lose the optimizer and the dashboard. That's the trade.

Mintlify-style acquisition risk note: the OSS license guarantees the community can always fork. If TokenTrimmer Inc ever pivots, the Gateway lives on.

---

## 13. Repository structure

Monorepo (single GitHub org, multiple repos):

```
tokentrimmer/                                  (GitHub org)
├── tokentrimmer                               (main OSS repo)
│   ├── crates/                                (Rust workspace)
│   │   ├── core/
│   │   ├── providers/
│   │   ├── cache/
│   │   ├── routing/
│   │   └── ...
│   ├── bin/tokentrimmer/                      (the binary)
│   ├── examples/                              (docker-compose, configs)
│   ├── docs/                                  (markdown docs synced to docs site)
│   ├── README.md
│   ├── LICENSE                                (Apache 2.0)
│   └── Cargo.toml
│
├── inspect-cli                                (OSS Inspect CLI)
│   └── (Rust + tree-sitter parsers)
│
├── rules                                      (OSS rules repo)
│   └── (YAML rule files, organized by category)
│
├── inspect-action                             (OSS GitHub Action)
│   └── (action.yml + thin wrapper)
│
├── sdk-python                                 (OSS)
├── sdk-typescript                             (OSS)
│
├── cloud                                      (PRIVATE — hosted service)
│   ├── api/                                   (Rust backend: auth, billing, Plan, Inspect-hosted)
│   ├── worker/                                (Rust worker: scheduled jobs, PDF render)
│   ├── app/                                   (Astro dashboard)
│   ├── marketing/                             (Astro marketing site)
│   ├── docs-site/                             (Astro + Starlight)
│   └── infra/                                 (Fly configs, deployment scripts)
│
└── .github/
    └── (org-level templates, CODE_OF_CONDUCT, SECURITY)
```

OSS contributions go to the public repos. Private contributions (commercial features) go to `cloud`.

---

## 14. Build & deploy pipeline

### 14.1 Gateway

- Cargo builds for Linux x86_64 and ARM64
- Multi-stage Dockerfile producing minimal scratch-based image
- Pushed to GHCR on every tagged release
- Fly.io deploys via `fly deploy` from GitHub Action on merge to `main`
- Releases tagged semver, with auto-generated changelog from conventional commits

### 14.2 Frontend (Astro)

- `pnpm build` per site
- Cloudflare Pages auto-deploys on git push (separate branches per site)
- Preview deployments for every PR

### 14.3 CI checks (every PR)

- `cargo fmt --check`
- `cargo clippy -- -D warnings`
- `cargo test --workspace`
- `cargo audit` (security advisories)
- `pnpm test` for frontend
- `pnpm build` for frontend (catches build errors)

### 14.4 Releases

- Gateway releases tagged from `main` after CI passes
- Inspect rules releases independent (version pinning in Inspect binary)
- Frontend deploys continuous

---

## 15. Testing strategy

### 15.1 Gateway

- **Unit tests** in each crate for translation logic, cache key generation, routing rule evaluation
- **Integration tests** with mock provider servers (httpmock crate) covering: success, retry, fallback, streaming, error classes
- **Contract tests** against real provider sandboxes (small dollar budget, run weekly in CI)
- **Load tests** with `oha` or `wrk` against a local Gateway to verify latency budget on cache hits and cache misses

### 15.2 Inspect

- **Rule tests** — each rule has a fixture directory of "should detect" and "should not detect" examples
- **Regression tests** — golden output for known repos (e.g., open-source LangChain examples)
- **CLI tests** — end-to-end runs with snapshot output

### 15.3 Plan

- **Replay correctness** — synthetic traffic with known expected savings
- **Confidence interval coverage** — Monte Carlo validation that 95% CI actually covers truth 95% of the time
- **Quality scoring** — hand-labeled set of acceptable/unacceptable model-swap examples

### 15.4 Frontend

- Component-level tests with Vitest
- End-to-end smoke tests with Playwright covering signup → first request → first dashboard view

---

## 16. Security & privacy

### 16.1 Customer data

- Provider credentials encrypted at rest with libsodium
- API key secrets argon2-hashed
- Prompt and response bodies NOT logged by default (only metadata: token counts, model, timing, route)
- **Opt-in encrypted body capture** (hosted, per-org, off unless enabled): a customer can enable request/response body capture for `/logs` replay. When enabled, bodies are **encrypted at rest** with XChaCha20-Poly1305 under a per-org key derived from `TT_MASTER_KEY` (plaintext never lands in Postgres); the route's **redaction pass runs before storage** so masked secrets never reach the capture table; each stored body is **size-capped** (`TT_BODY_CAPTURE_MAX_BYTES`, default 256 KiB — over-cap bodies are truncated with an explicit in-payload marker, the original length recorded honestly); rows expire on a per-org **retention** window (1–30 days, default 7). Captured responses carry the `X-TokenTrimmer-Captured: true` header; the header is absent on the default capture-off path
- L2 semantic cache (paid, opt-in) stores the prompt embedding **and the full response body** in Postgres (scoped by `org_id`); it does not persist the original prompt text, but generating the embedding sends that prompt text to OpenAI (`text-embedding-3-small`). Retrieval, when enabled, sends prompt text to OpenAI for the same reason
- All data scoped by `org_id` at every query

### 16.2 Network

- TLS everywhere (HTTPS via Fly.io edge)
- Internal service-to-service via private Fly networking
- HSTS, secure cookies, CSP headers on dashboard

### 16.3 Audit & access

- All admin actions (org settings change, key rotation, member added) logged to `audit_log` table
- Hosted version: only customer's own users access their org
- Self-hosted: customer manages access

### 16.4 SOC-2 path (deferred)

Not pursued in v1. Path defined: use Vanta or Drata starting at ~$50K MRR. Required for enterprise customers but not for the indie/SMB tier.

### 16.5 Vulnerability disclosure

- `SECURITY.md` in every public repo
- `security@tokentrimmer.com` for reports
- GitHub Security Advisories for OSS components
- Public bug bounty deferred until post-launch

---

## 17. Observability

### 17.1 Tracing

- OpenTelemetry from day one
- Spans for: request received → auth → routing → cache lookup → provider call → response
- Trace ID propagated to customer in `X-TokenTrimmer-Trace-Id` response header
- Trace storage: self-hosted Jaeger initially, or Honeycomb free tier

### 17.2 Metrics

- Prometheus exposition format — **implemented**: `GET /metrics` (see gateway API reference §17)
- Metrics: `http_requests_total` + `http_request_duration_seconds` (rate / error / latency by method+endpoint+status), `cache_lookups_total` (hit rate by tier), `provider_failover_total`, `provider_request_duration_seconds` (per-provider latency), `catalog_zero_price_total`, `tt_build_info`, `process_uptime_seconds`
- Scraped by Grafana Cloud free tier or self-hosted Prometheus + Grafana

### 17.3 Logs

- Structured JSON via `tracing` crate
- Levels: ERROR / WARN / INFO / DEBUG
- Shipped to BetterStack or self-hosted Loki

### 17.4 Alerts

- PagerDuty on p1 issues (Gateway error rate > 1%, or any downtime)
- Slack/email on p2 issues (cache hit rate drop, provider latency spike)

### 17.5 Status page

- Public uptime monitoring via UptimeRobot or similar
- Page at `status.tokentrimmer.com`
- Incident postmortems published as blog posts

---

## 18. Internal cost discipline

TokenTrimmer is itself a product that uses LLMs. We must dogfood and stay efficient.

- **All Inspect Tier 2/3 calls** routed through our own Gateway (with caching, routing, etc.). This is also the most honest demo of the product.
- **Cost budget per customer per month** — Inspect alone should cost us no more than 5% of subscription revenue per customer
- **Frontier LLM calls** go through prompt caching, semantic caching, and use the cheapest acceptable model
- **Tier 2 specialized models** built as soon as a Tier 3 task gets called more than ~10,000 times/month
- **Quarterly cost audit** — review our own spend, apply our own Inspect findings to our own code

This is both fiscal discipline and the strongest possible marketing testimonial.

---

## 19. v1 roadmap & milestones

The Plan-as-v1-pillar and Reporting addition push the timeline. Here's the realistic schedule.

| Week | Milestone |
|---|---|
| 1 | Repos created, infrastructure provisioned, landing page live with waitlist |
| 2–4 | Gateway: OpenAI + Anthropic providers, basic routing, L1 cache, telemetry |
| 5 | Gateway: Gemini + Mistral + Groq + Together + OpenRouter providers |
| 6 | Gateway: Local providers (Ollama, vLLM, LM Studio), streaming hardening |
| 7 | Auth + billing + Stripe integration, API key issuance, basic dashboard |
| 8–9 | Semantic cache (pgvector pipeline), dashboard overview + costs pages |
| 10 | Inspect CLI with 10 P0 rules, Inspect repository |
| 11 | Inspect hosted backend, GitHub Action |
| 12 | Plan engine (replay + cost projection, no quality scoring yet) |
| 13 | Plan quality risk scoring (Tier 3 sampling) |
| 14 | Reporting: dashboard pages complete, weekly digest email |
| 15 | Reporting: monthly PDF executive summary, anomaly detection |
| 16 | Private alpha with 5–10 hand-picked users |
| 17–18 | Bug fix based on alpha feedback, performance tuning |
| 19 | Beta launch (Hacker News, Reddit, IH, Product Hunt) |
| 20+ | Iterate based on beta feedback |

**v1 launch target: ~5 months from kickoff.** Compresses if you cut scope (e.g., defer Inspect Tier 2 rules), extends if anything below the line slips.

**Hard checkpoint at week 8:** if Gateway + auth + billing + basic dashboard aren't working end-to-end, the timeline is wrong and scope must be cut. Don't push through.

---

## 20. Risks & open questions

### 20.1 Risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Scope creep delays launch past Helicone migration window | High | High | Hard checkpoint at week 8; cut to scope, not time |
| Inspect false positives erode trust | Medium | High | Conservative v1 rules (10–15 high-confidence only) |
| Gateway latency too high in some regions | Medium | High | Multi-region Fly.io, load testing in week 12 |
| Provider API changes break adapters | Continuous | Medium | Contract tests weekly, adapter versioning |
| Customer signs up, integrates, then provider key gets rate-limited and TT is blamed | Medium | Medium | Surface upstream rate limits clearly in dashboard |
| Open-source fork undercuts hosted business | Low–Medium | Medium | Hosted value is operational; OSS users rarely switch to self-host once running paid |
| TT itself gets hacked → all customer provider keys exposed | Low | Catastrophic | Encryption at rest, regular pen test, BCP plan |

### 20.2 Open questions to decide soon

- **Embedding model for semantic cache:** OpenAI `text-embedding-3-small` (cheap, simple) vs self-hosted BGE-small via candle (zero per-call cost, more infra). Recommend OpenAI for v1.
- **Auth provider:** Auth.js (more control, more work) vs Clerk (faster, more $) vs Supabase Auth (bundled with Postgres). Recommend Auth.js.
- **PDF generation:** Playwright in worker (heavy, slow) vs Typst (fast, clean, requires templates) vs server-side React PDF (medium). Recommend Typst — modern, fast, fits Rust ecosystem.
- **Worker queue:** Postgres-based (pg-boss-style, simple) vs Redis-based (faster, separate infra) vs Fly machines (cron-style). Recommend Postgres for v1.
- **Inspect storage of repo source:** ephemeral clone vs persistent (faster re-scans, more storage). Recommend ephemeral in v1.
- **Marketing site vs dashboard authentication boundary:** subdomains (clean, more DNS) vs paths (one origin, simpler). Recommend subdomains.

---

## 21. Appendices

### Appendix A — Example end-to-end request

Customer Python code:
```python
from openai import OpenAI

client = OpenAI(
    base_url="https://api.tokentrimmer.com/v1",
    api_key="tt_live_abc123...",
)

response = client.chat.completions.create(
    model="claude-3-5-sonnet",
    messages=[{"role": "user", "content": "Classify this as positive or negative: I loved it"}],
    extra_headers={"X-TokenTrimmer-Tag": "feature=sentiment"},
)
```

Behind the scenes:
1. Request hits Gateway in nearest region
2. Auth: `tt_live_abc123...` validated against Postgres (cached in Redis)
3. Routing engine matches rule "cheap-for-short" (total_tokens < 200)
4. Target model rewritten: `claude-3-5-sonnet` → `claude-3-5-haiku`
5. Cache lookup: semantic similarity 0.94 to previously cached request → HIT
6. Cached Haiku response returned (with `X-TokenTrimmer-Cache: hit-l2` header)
7. Telemetry written: baseline cost $0.0034, actual cost $0.0000, saved $0.0034
8. Total latency: 3ms

### Appendix B — Example Plan output

(See section 6.5)

### Appendix C — Example Inspect rule

(See section 5.3)

### Appendix D — Configuration file example

```yaml
# tokentrimmer.yaml (OSS or hosted)

providers:
  openai:
    api_key_env: OPENAI_API_KEY
  anthropic:
    api_key_env: ANTHROPIC_API_KEY
  ollama:
    base_url: http://localhost:11434

routes:
  - name: cheap-for-short
    priority: 100
    when:
      messages.total_tokens: { lt: 200 }
    then:
      target_model: anthropic/claude-3-5-haiku
      cache:
        enabled: true
        ttl: 86400
        semantic_threshold: 0.92

  - name: local-for-classification
    priority: 200
    when:
      messages.last.content:
        matches: "(classify|categorize|extract)"
      messages.total_tokens: { lt: 500 }
    then:
      target_model: ollama/llama3:8b
      fallback: openai/gpt-4o-mini

  - name: default
    priority: 0
    when: { always: true }
    then:
      target_model: "${request.model}"

cache:
  redis_url_env: REDIS_URL
  embedding:
    provider: openai
    model: text-embedding-3-small
  defaults:
    ttl: 3600
    semantic_threshold: 0.92

telemetry:
  enabled: true
  retention_days: 30  # OSS only; hosted uses tier-based
```

### Appendix E — Glossary

- **Gateway** — the Rust proxy in the request path
- **Inspect** — the static code analyzer
- **Plan** — the cost-projection simulator
- **Reporting** — dashboards, scheduled emails, PDF reports
- **Watch** — (post-v1) continuous monitoring, PR bot, IDE integration
- **L1 cache** — exact-match cache (Redis)
- **L2 cache** — semantic cache (pgvector)
- **Tier 1/2/3 rules** — Inspect rule tiers by analysis mechanism
- **Baseline model** — customer-declared flagship model used for savings calculation
- **Plan apply** — execution of an approved Plan diff
- **Org** — billing entity; one or more users
- **Route** — a rule that maps request criteria to a target provider/model + cache settings

---

**End of v1 spec.**

Companion documents (to write next):
- `inspect-rule-catalog.md` — full ~120 detection rules with detection logic and fixes
- `gateway-api-reference.md` — full OpenAI-compatible API surface and TokenTrimmer extensions
- `provider-adapter-guide.md` — how to add a new provider
- `plan-replay-design.md` — deeper dive on replay correctness and confidence intervals
