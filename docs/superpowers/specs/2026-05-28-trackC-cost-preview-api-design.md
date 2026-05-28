# Track C — Cost Preview API (`POST /v1/preview`)

**Status:** Draft 1
**Track:** C of six-track expansion
**Date:** 2026-05-28
**Depends on:** none (reuses existing Gateway pricing tables + Plan engine)
**Consumed by:** Track A (MCP), Track B (proxy), Track D (CLI hooks)

---

## 1. Problem

Customers don't know the cost of an LLM call before making it. The TokenTrimmer Gateway tells them after — `X-TokenTrimmer-Cost-Usd` is a response header. By then the spend has happened, and the only available levers are caching (post-hoc) and switching models (next-call).

`POST /v1/preview` accepts the same request shape that would be sent to `/v1/chat/completions` and synchronously returns: projected cost on the current model, projected cost on each cheaper-equivalent model with a quality risk band, and projected savings if served from L1 / L2 cache.

This unlocks: IDE cost overlays (Track A/B), pre-call budgets (CLI shells), and "did you mean to use Opus for this?" guardrails (Track A).

## 2. Goals

1. Single endpoint. Same auth (API key + org scope) as the rest of the Gateway.
2. p50 < 30ms — same SLA as cache-miss `/v1/chat/completions` (it does less work than that).
3. Deterministic: same input → same output. No randomness, no LLM calls in the preview path.
4. No external HTTP calls. Pricing tables are in-process. Quality risk-band lookup is in-process (last 90 days of Plan engine outputs for this org).
5. Backwards compatible: never breaks if a request field is unknown (returns "best estimate" + a `warnings[]`).

## 3. Non-goals

- Streaming. Preview returns a single JSON body.
- Authenticated bypass for testing. Use `tt_test_*` sandbox keys for that.
- Re-running the request to measure actual quality. The Plan engine already does that on opt-in workloads; we surface its existing outputs.

## 4. Architecture

### 4.1 Public-repo (Gateway) side

```
crates/core/
└── src/
    ├── server.rs                       [modified — register POST /v1/preview]
    └── routes/
        └── preview.rs                  [new — handler]

crates/preview/                          [new crate — pure preview logic]
└── src/
    ├── lib.rs
    ├── pricing.rs                      [reuses crates/providers/<name>/pricing tables]
    ├── token_estimator.rs              [tiktoken-style estimation per model family]
    ├── cache_projection.rs             [L1+L2 hit-rate-weighted savings using historical org data]
    ├── route_suggestions.rs            [cheaper-equivalent models per task class]
    └── types.rs                        [PreviewRequest, PreviewResponse, Suggestion]
```

### 4.2 Cloud-repo side (read-only)

```
cloud/crates/api/src/preview.rs         [new — wrapper that adds org context: 90d Plan engine
                                         quality scores + per-org cache hit rates]
```

### 4.3 Request shape

```json
POST /v1/preview
Authorization: Bearer tt_live_...

{
  "model": "claude-sonnet-4-6",
  "messages": [{"role": "user", "content": "Classify this email as spam or not: <body>"}],
  "max_tokens": 1024,
  "tools": [...],            // optional, affects token estimate
  "stream": false             // ignored; preview is always synchronous
}
```

### 4.4 Response shape

```json
{
  "current": {
    "model": "claude-sonnet-4-6",
    "provider": "anthropic",
    "input_tokens_estimated": 47,
    "output_tokens_estimated": 12,        // capped at max_tokens
    "cost_usd": 0.000189,
    "estimation_confidence": "high"        // high | medium | low
  },
  "cache_projections": {
    "l1_hit_savings_usd": 0.000189,        // full savings if L1 hits
    "l1_hit_probability": 0.34,            // 30d hit rate for this org+model
    "l2_hit_savings_usd": 0.000189,
    "l2_hit_probability": 0.18,
    "weighted_savings_usd": 0.000098       // weighted expected savings
  },
  "route_suggestions": [
    {
      "route": "swap-to-haiku-4-5",
      "model": "claude-haiku-4-5",
      "cost_usd": 0.000023,
      "savings_usd": 0.000166,
      "quality_risk_band": "LOW",          // HIGH | MEDIUM | LOW | UNKNOWN
      "rationale": "Classification tasks ≤ 512 tokens have <2% quality regression on Haiku per 90d Plan replays in your org.",
      "applicable": true                    // false if your /routes config already routes this
    },
    {
      "route": "swap-to-flash-2-5",
      "model": "gemini-flash-2.5",
      "cost_usd": 0.000019,
      "savings_usd": 0.000170,
      "quality_risk_band": "MEDIUM",
      "rationale": "Insufficient samples in your traffic (n=3 last 90d). Risk-band derived from global Plan engine corpus.",
      "applicable": true
    }
  ],
  "warnings": [],                          // e.g., "unknown model parameter X ignored"
  "trace_id": "01J2Y9..."
}
```

## 5. Token estimation

Estimate input tokens via per-model tokenizer:
- OpenAI / Anthropic: tiktoken-rs with `cl100k_base` (close enough for cost preview; final billing uses provider report).
- Gemini: char-count / 4.0 heuristic (Google's own tokenizer is not open).
- Local providers: char-count / 4.0.

Output tokens: `min(max_tokens or model_default, average_response_tokens_for_classified_task)`. Classification of task (chat / extract / code / classification) is a cheap regex over the last message — same logic as Inspect Tier-1 rules.

`estimation_confidence`:
- `high`: tokenizer matched provider's; classification regex hit.
- `medium`: tokenizer was a heuristic.
- `low`: tokenizer was a heuristic AND classification didn't match — output estimate may be wildly off.

## 6. Route suggestion algorithm

For each provider/model in the registry cheaper than the current model:
1. Compute cost on that model.
2. Look up 90d quality band from Plan engine for this org + task classification.
3. If no per-org data → fall back to global Plan engine output (other orgs' opt-in workloads).
4. If still nothing → `quality_risk_band: "UNKNOWN"`.
5. Mark `applicable: false` if the org's `routes` config already maps this request to the suggested model.
6. Sort suggestions by `savings_usd` desc; cap at 3.

## 7. Cache projection

For the org:
- `l1_hit_probability` = 30d L1 hit rate for `model = current.model`. If org has < 100 requests in 30d → use global median (~0.20).
- `l2_hit_probability` = same shape but for L2.
- `weighted_savings_usd` = `current.cost_usd * (l1_hit_probability + (1 - l1_hit_probability) * l2_hit_probability)`.

## 8. Auth + quotas

- Same `tt_live_*` API key middleware as `/v1/chat/completions`.
- Counts against the org's monthly request quota at 0.1× weight (preview is cheap; we want to encourage use).
- `tt_test_*` keys return deterministic synthetic preview (no Plan engine call) — useful for local dev.

## 9. Testing

| Layer | Tests |
|---|---|
| Unit (pricing) | Snapshot per model family — Sonnet, Haiku, Opus, GPT-4o, Flash. |
| Unit (token_estimator) | Known prompts → known token counts (tiktoken-rs is deterministic). |
| Unit (cache_projection) | Synthetic 30d hit rate → expected weighted savings. |
| Unit (route_suggestions) | Plan engine fixture with known risk bands → expected suggestion list. |
| Integration | `POST /v1/preview` round-trip via httpmock cloud-API; assert shape + status. |
| Auth | Missing key → 401. Invalid key → 401. Expired sub → 403. |
| Sandbox | `tt_test_*` returns deterministic body. |

## 10. Rollout

1. Day 0: ship endpoint with token estimation + pricing + cache projection. Route suggestions return empty `[]` for orgs with < 100 requests.
2. Day 14: unlock per-org route suggestions once 14d of Plan engine outputs accumulate.
3. Day 30: add `quality_risk_band: "UNKNOWN"` → `"LOW"` fallback for tasks the global corpus has reliably solved.

## 11. References

- Existing pricing tables: `crates/providers/<name>/src/pricing.rs`
- Existing Plan engine outputs: `crates/plan-core/src/quality.rs` and `plan_runs` table
- Existing cache hit-rate query in cloud repo: `apps/dashboard/src/lib/cache.ts`
