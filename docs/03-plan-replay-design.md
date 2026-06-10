# TokenTrimmer Plan — Replay & Simulation Design

**Status:** v1 spec
**Audience:** Core team building Plan; users wanting to understand methodology
**Companion to:** Architecture spec section 6

---

## Purpose

Plan answers one question: *"What would have happened if I had used this config instead?"*

This document defines how Plan answers that question rigorously. It covers the replay engine, cost projection math, confidence intervals, quality risk scoring, sampling strategy, and the projected-vs-actual reconciliation loop that builds trust over time.

---

## 1. Design principles

1. **Honesty over impressiveness.** Plan never reports a point estimate without confidence bounds. A confident range that includes the truth is better than a single number that doesn't.
2. **Real traffic, not synthetic.** Plan replays actual historical requests from the customer's own Gateway traffic. No synthetic benchmarks, no vendor-supplied numbers.
3. **Quality is a first-class output.** Cost savings are meaningless without quality preservation. Every Plan reports both.
4. **Cheap to run.** Customers should run Plan freely. Replay must not call live providers except for the sampled quality-check subset.
5. **Closed loop with reality.** Every applied Plan is later compared against the actual outcome. Divergence informs both the user and our calibration.
6. **No raw data leaks.** Plan works from stored telemetry (token counts, embeddings, metadata). Raw prompts/responses are accessed only with explicit customer opt-in.

---

## 2. Mental model

Think of Plan as **Terraform plan for LLM configs**.

```
Current state (active Gateway config)
   +
Proposed diff (new routes, cache settings, model swaps)
   ↓
Replay historical window
   ↓
Projection: cost, latency, cache hit rate, quality risk
   +
Confidence intervals (bootstrap)
   ↓
Render to user (CLI or dashboard)
   ↓
User approves or rejects
   ↓
[if approved]
Apply diff → Gateway hot-reloads → Watch monitors
   ↓
Reconciliation report after N days: projected vs actual
```

---

## 3. Inputs

A Plan run takes:

| Input | Source | Notes |
|---|---|---|
| Active config | `routes`, `cache_settings` tables for org | Read-only |
| Proposed diff | User-supplied | JSON patch or YAML diff |
| Time window | User-supplied | Default: last 30 days; min 7 days, max 90 days |
| Sample mode | User-supplied | `full` (100%), `sample:N%`, `fast` (downsample to 10K) |
| Quality check budget | User-supplied | Default: 1,000 requests sampled for quality re-run |
| Baseline model | Org config | For savings calculation |

---

## 4. Telemetry schema (the replay substrate)

Plan depends on what Gateway records. The relevant telemetry per request:

```sql
CREATE TABLE request_logs (
  id               UUID PRIMARY KEY,
  org_id           UUID,
  ts               TIMESTAMPTZ,
  -- request metadata (no raw bodies)
  provider         TEXT,
  model            TEXT,
  input_tokens     INT,
  output_tokens    INT,
  cached_tokens    INT,
  -- routing
  matched_route_id UUID,
  -- cache
  cached           BOOLEAN,
  cache_layer      TEXT,        -- 'l1' | 'l2' | null
  -- cost
  cost_usd         NUMERIC(12,6),
  baseline_cost_usd NUMERIC(12,6),
  -- timing
  latency_ms       INT,
  upstream_latency_ms INT,
  -- request shape (for replay)
  last_user_message_embedding vector(1536),   -- for semantic cache replay
  has_tools        BOOLEAN,
  has_response_format BOOLEAN,
  has_streaming    BOOLEAN,
  message_count    INT,
  -- response shape
  finish_reason    TEXT,
  status           INT,
  -- correlation
  tag              TEXT,
  trace_id         UUID
);
```

**Critical fields for Plan:**
- `input_tokens`, `output_tokens` — for cost recomputation under different models
- `last_user_message_embedding` — for semantic cache hit projection
- `matched_route_id` — to determine which proposed-diff routes affect which historical requests
- `provider`, `model` — current routing baseline
- `tag` — for per-feature attribution in Plan output
- `status` — to exclude failed requests from quality analysis

If a field isn't recorded for a historical request, the replay must either skip that request or fall back to estimation (and reflect the uncertainty in confidence intervals).

---

## 5. The replay engine

### 5.1 Pseudocode

```
function plan_replay(active_config, proposed_diff, window):
    proposed_config = apply_diff(active_config, proposed_diff)
    requests = fetch_request_logs(org_id, window)

    if len(requests) < MIN_SAMPLE_SIZE:
        return PlanResult.insufficient_data(requests)

    projections = []
    for req in requests:
        # 1. Determine new route
        new_route = route_match(req, proposed_config.routes)
        new_model = resolve_model(new_route, req)
        new_provider = provider_for(new_model)

        # 2. Project cache outcome
        cache_outcome = project_cache(req, new_route, proposed_config.cache)

        # 3. Project cost
        if cache_outcome.hit:
            cost = 0
        else:
            cost = compute_cost(
                new_model,
                req.input_tokens,
                req.output_tokens,
                cache_outcome.cached_tokens
            )

        # 4. Project latency
        latency = project_latency(new_provider, new_model, req)

        # 5. Record divergence flags for quality sampling
        quality_eligible = (
            new_model != req.model  # model changed
            and not cache_outcome.hit  # didn't get cached response
            and req.status == 200  # successful baseline
        )

        projections.append(Projection(
            req_id=req.id,
            new_route=new_route,
            new_model=new_model,
            new_provider=new_provider,
            cost=cost,
            baseline_cost=req.baseline_cost_usd,
            latency=latency,
            cache_outcome=cache_outcome,
            quality_eligible=quality_eligible,
        ))

    # 6. Aggregate
    aggregates = aggregate(projections)

    # 7. Confidence intervals (bootstrap)
    cis = bootstrap_ci(projections, iterations=10_000)

    # 8. Quality sampling (separate flow, returns later)
    quality_sample = sample_for_quality_check(
        projections,
        budget=plan_run.quality_check_budget
    )
    quality_result = await run_quality_check(quality_sample)

    return PlanResult(
        aggregates=aggregates,
        confidence_intervals=cis,
        quality_risk=quality_result.risk_band,
        quality_details=quality_result,
        per_route_breakdown=group_by_route(projections),
        sample_size=len(projections),
    )
```

### 5.2 Step-by-step explanation

**Step 1 — Route matching.** Apply the proposed config's routing rules to the request's recorded shape. Routes match on conditions like token count, model name, tags, time-of-day — all available from telemetry. If a request would have matched a different route, the projection diverges.

**Step 2 — Model resolution.** Routes resolve to a target model. May involve fallback chains; Plan models the primary path (fallbacks projected separately as "what if primary fails" sensitivity analysis).

**Step 3 — Cache projection.** Trickiest part. See section 6.

**Step 4 — Cost computation.** Token counts × per-model pricing. Use cached pricing tables. Account for cached-token discount when cache hit.

**Step 5 — Latency projection.** Look up p50/p95/p99 from historical traffic for the same provider+model. If insufficient history, mark as "unknown" and exclude from latency aggregates.

**Step 6 — Quality flagging.** Requests where the projected model differs from the actually-served model are candidates for quality re-running. Section 7 covers this.

**Step 7 — Aggregation.** Sum cost, weighted-average latency, count cache hits, etc. Aggregate per-route and per-tag.

**Step 8 — Confidence intervals.** Bootstrap resampling. Section 8 covers this.

**Step 9 — Quality sampling.** Async, separate execution path. Section 7 covers this.

---

## 6. Cache projection

The hardest part of replay. We need to determine whether a request would have hit the proposed cache config without re-running anything.

### 6.1 Exact-match cache (L1)

Easy. For each request, compute the proposed cache key (SHA-256 of normalized request shape). Look for prior requests with the same key within the proposed TTL window.

```
function project_l1_cache(req, cache_config, all_requests):
    key = exact_cache_key(req, cache_config.included_fields)
    prior = find_prior_request_with_key(all_requests, key, before=req.ts)
    if prior and (req.ts - prior.ts) <= cache_config.ttl:
        return CacheOutcome(hit=True, layer='l1', source_req=prior.id)
    return CacheOutcome(hit=False)
```

Note: the request that *creates* a cache entry pays full cost; subsequent hits are free. The first request in any cache-eligible group is always a miss.

### 6.2 Semantic cache (L2)

Harder. We use the stored embedding of each request's last user message. For each request, search prior requests for any with cosine similarity ≥ threshold.

```
function project_l2_cache(req, cache_config, all_requests, vector_index):
    # Only consider requests that didn't already hit L1
    candidates = vector_index.search(
        embedding=req.last_user_message_embedding,
        threshold=cache_config.semantic_threshold,
        before_ts=req.ts,
        within=cache_config.ttl
    )
    if candidates:
        # Use earliest matching prior request as cache source
        source = candidates[0]
        return CacheOutcome(hit=True, layer='l2', source_req=source.id)
    return CacheOutcome(hit=False)
```

Cost of replay: building the in-memory vector index for the window is cheap (pgvector HNSW can do this in seconds for ~1M requests). Per-request lookup is sub-millisecond.

**Edge cases:**
- **Threshold sensitivity.** Hit rate is highly sensitive to the similarity threshold. Plan reports projected hit rate per threshold (a sensitivity sweep over 0.85, 0.90, 0.92, 0.95) so users can see the tradeoff.
- **Cache poisoning risk.** If two semantically similar requests have different *correct* answers (e.g., user-specific contexts), caching the first one's answer for the second is wrong. Section 6.3 addresses this.
- **No embedding stored.** If a request lacks `last_user_message_embedding` (older traffic, opted out), it's excluded from L2 projection. Reflected in sample-size disclosure.

### 6.3 Cache poisoning detection

When projecting L2 hits, we should flag requests where caching might produce *wrong* answers. Heuristics:

1. **User-specific tag mismatch.** If `tag` includes a user identifier and proposed and source have different users, flag.
2. **High token-count delta.** If proposed and source have significantly different input token counts despite similar last-message embeddings, the surrounding context likely differs — flag.
3. **Output divergence in similar pairs.** Sample pairs from history that were semantically similar and check whether their actual outputs differed substantially. If frequent divergence, lower the projected hit rate and raise the quality risk band.

Flagged "would-be cache hits" are added to the quality sample pool.

### 6.4 Provider-native prompt caching

Anthropic and OpenAI prompt caches are different from TokenTrimmer's L1/L2. They apply within the provider and reduce input-token cost on the cached prefix.

Plan can project these:
- For Anthropic: if the proposed config adds `cache_control` to a long static system prompt, and the request's `input_tokens` includes that prefix, the cached-token-count projection becomes `prefix_tokens × hit_rate`, where hit_rate is estimated from prior recurrence of identical system prompts.
- For OpenAI: similar logic; OpenAI applies prompt caching automatically when prefixes are stable.

These projections are reported separately as "provider cache savings" alongside TokenTrimmer L1/L2 savings.

---

## 7. Quality risk scoring

A cheaper model only saves money if it produces acceptable output. Plan must estimate this without rerunning every request.

### 7.1 Sampling

For the subset of requests where the proposed config swaps to a cheaper model (`quality_eligible` flag), Plan samples a configurable number (default: 1,000) using stratified sampling:

- Stratify by `tag` (so each feature gets representation)
- Stratify by request-size buckets (so we sample the full distribution)
- Always include any requests flagged for cache poisoning risk
- Always include a deterministic "regression test" set the user defines

### 7.2 Re-running

For each sampled request, Plan:

1. Reconstructs the request from telemetry (this requires the user to have opted into full request logging — see section 11)
2. Sends it to the proposed cheaper model
3. Captures the response

This is the only step in Plan that incurs live provider cost. Budget-bounded per Plan run.

### 7.3 Scoring divergence

For each sampled pair (baseline response, cheaper-model response), an LLM-as-judge scores divergence:

```
judge_prompt:
  "Given the original request, the baseline model's response,
   and the alternative model's response, classify the alternative's
   acceptability for the user's apparent goal:
   - ACCEPTABLE: equivalent or better
   - MARGINAL: minor quality drop, may or may not matter
   - DEGRADED: clear quality drop
   - WRONG: incorrect or harmful

   Respond with one classification and a one-sentence rationale."
```

The judge is itself an LLM call. Use a Tier 2 specialized judge model if available; otherwise use a frontier model with low temperature.

### 7.4 Risk band aggregation

```
function compute_risk_band(scores):
    n = len(scores)
    acceptable = count(scores, "ACCEPTABLE")
    marginal = count(scores, "MARGINAL")
    degraded = count(scores, "DEGRADED")
    wrong = count(scores, "WRONG")

    acceptable_rate = acceptable / n
    bad_rate = (degraded + wrong) / n

    if bad_rate > 0.05:
        return "HIGH"
    if bad_rate > 0.02 or acceptable_rate < 0.90:
        return "MEDIUM"
    return "LOW"
```

Thresholds are conservative defaults; user can configure.

### 7.5 Output

Quality result includes:
- Risk band (LOW / MEDIUM / HIGH)
- Sample size and stratification details
- Distribution of classifications
- Top N "WRONG" examples with diffs (subject to opt-in for raw content)
- Suggestion: "to lower risk, narrow this route's conditions" (with specific narrowing suggestions)

### 7.6 No quality sampling possible

If the user hasn't opted into request logging, Plan cannot re-run requests against the proposed model. In this case:

- Quality risk is reported as `UNVERIFIED`
- Plan suggests: "Enable request body logging on a single API key to allow quality validation, or run an A/B test with the proposed config on a small percentage of traffic."

---

## 8. Confidence intervals

Every aggregate metric in a Plan output gets a confidence interval. The method is non-parametric bootstrap.

### 8.1 Bootstrap algorithm

```
function bootstrap_ci(projections, metric_fn, iterations=10_000, ci_level=0.95):
    n = len(projections)
    samples = []
    for i in 1..iterations:
        resample = random_choices(projections, k=n, replace=True)
        samples.append(metric_fn(resample))

    samples.sort()
    alpha = (1 - ci_level) / 2  # 0.025 for 95% CI
    lower = samples[int(iterations * alpha)]
    upper = samples[int(iterations * (1 - alpha))]
    point = metric_fn(projections)
    return ConfidenceInterval(point=point, lower=lower, upper=upper, level=ci_level)
```

### 8.2 Metrics that get CIs

- Total cost
- Total savings (baseline_cost - cost)
- Cache hit rate
- Latency p50, p95, p99 (use percentile bootstrap)
- Per-route savings (when route has > 100 projected requests)

### 8.3 Reporting CIs honestly

CLI output:
```
Cost:           $4,247.13 → $1,683.42  (-60.4%, save $2,563.71/mo)
                95% CI: [$2,401.18, $2,726.24]
```

If CI is wide (relative width > 30%), Plan shows a warning:
```
⚠ High uncertainty — confidence interval spans 38% of projected value.
  Reasons:
    - Small sample size (12 requests matched this route)
    - High variance in request size distribution
  Suggestion: extend the time window or wait for more traffic.
```

### 8.4 Why bootstrap

Closed-form CIs (normal approximation) assume independence and normality, both of which break for LLM traffic:
- Requests are clustered (users come in bursts)
- Token distributions are heavy-tailed
- Cache hits are not Bernoulli (one cached request enables many future hits)

Bootstrap makes no assumptions and gives valid CIs as long as the sample is representative.

### 8.5 Honest reporting of sample bias

Plan also reports:
- **Sample size** (number of requests in the replay)
- **Sample completeness** (% of historical traffic with complete telemetry for replay)
- **Per-route coverage** (do all proposed routes have enough historical matches?)
- **Time coverage** (any gaps in the window?)

These are surfaced in CLI under "Caveats" and in dashboard as a data-quality badge.

---

## 9. Apply flow

If the user approves a Plan:

```
1. Plan record marked status='applied', timestamp recorded
2. Atomic Postgres transaction:
   - Insert/update routes per diff
   - Update cache_settings per diff
3. Publish config-changed event to Gateway instances (Redis pub/sub or direct HTTP)
4. Gateway instances hot-reload config (target: under 30 seconds for all instances)
5. Audit log entry created
6. Email confirmation sent to all org owners
7. Watch begins reconciliation tracking
```

If the diff is invalid (conflicting rules, references undefined providers), apply is rejected with specific error before any Gateway change.

Rollback: every applied Plan creates a reverse diff that can be applied as a new Plan. Plans are atomic — entire diff applies or none does.

---

## 10. Reconciliation (projected vs actual)

The closed loop. After a Plan is applied, we measure whether reality matched the projection.

### 10.1 Reconciliation report

Generated 7 days after apply, then again at 30 days. Compares:

| Metric | Projected | Actual | Delta |
|---|---|---|---|
| Daily cost | $48.17 | $51.34 | +6.6% |
| Cache hit rate | 41.2% | 38.7% | -2.5pp |
| Latency p50 | 287ms | 304ms | +6% |

Plus:
- Quality observations from real traffic (any error rate change, customer feedback signals if integrated, increased retry rates)
- Inspect findings that were validated/invalidated

### 10.2 Calibration

Aggregated across all customers and Plans, we track:
- How often actual fell within projected 95% CI (should be ~95%)
- Median absolute error per metric
- Systematic bias direction

If CIs are systematically too narrow (actual outside CI > 5% of the time), we widen the bootstrap iterations or add a calibration adjustment.

This calibration data isn't customer-visible but informs our methodology and is referenced in our public methodology docs.

### 10.3 User-visible "trust score"

Per-customer, Plan tracks its own accuracy:
```
TokenTrimmer Plan has projected your costs within ±8% over the last 10 applied changes.
```

This shows up in the apply flow as social proof: "Plan has been accurate to ±8% on your traffic — apply with confidence."

---

## 11. Privacy & data handling

Plan reads telemetry; quality sampling reads (sometimes) raw request bodies.

| Operation | Data required | Privacy posture |
|---|---|---|
| Cost projection | Token counts, model, route, timestamp | No raw content |
| Cache projection | Stored embeddings only | No raw content |
| Latency projection | Timestamps, model, provider | No raw content |
| Quality sampling | Raw request body, raw response body | Requires opt-in per API key |

**Opt-in flow:** API key settings include "Enable body logging for quality analysis" toggle. Default OFF. When ON:
- Bodies stored encrypted at rest
- Retention follows tier defaults
- Visible in dashboard with "raw body" indicator on log entries
- Customer can purge any time

**For self-hosted Gateway:** opt-in is purely local; nothing leaves the customer's infrastructure unless they choose to use the hosted Plan service.

---

## 12. Cost discipline for Plan itself

Plan must be cheap to run, or customers won't use it.

**Per-Plan cost budget (TokenTrimmer's side):**
- Replay computation: minimal (CPU + Postgres + pgvector)
- Quality sampling: 1,000 sample × avg 4K tokens × cheapest acceptable model ≈ $1–$5 per Plan
- Judge calls: 1,000 × judge prompt ≈ $1–$10 per Plan

Total per Plan: under $15. Allowed per tier:
- Hobby: 2 Plans/month with quality sampling
- Pro: 20 Plans/month
- Scale: unlimited

Plans without quality sampling (cost-only) are free at all tiers.

---

## 13. Performance targets

| Operation | Target |
|---|---|
| Replay 100K requests (no quality sampling) | < 30s |
| Replay 1M requests (no quality sampling) | < 5min |
| Quality sampling of 1,000 requests | < 2min (parallel provider calls) |
| Plan apply (config write + propagation) | < 30s for all Gateway instances |
| CLI `tt plan` end-to-end (cost projection only) | < 10s for typical workloads |

If replay slows past targets, optimization order: index tuning → in-memory vector index → ClickHouse migration for telemetry.

---

## 14. CLI surface

```
$ tt plan --diff config/proposed.yaml --window 30d

Loading active config...                                      ✓
Fetching historical traffic (last 30 days)...                ✓
  1,234,567 requests over 30 days, 100% with complete telemetry
Computing route assignments under proposed config...          ✓
Projecting cache outcomes...                                  ✓
Computing cost projections...                                 ✓
Sampling 1,000 requests for quality verification...           ⠹
  Running 1,000 alternate-model requests (est. cost $3.20)
  Scoring divergence with judge model (est. cost $4.50)
Quality scoring complete                                      ✓
Computing confidence intervals (bootstrap, 10,000 iter)...    ✓

═══════════════════════════════════════════════════════════════
Plan Summary
═══════════════════════════════════════════════════════════════

Proposed changes:
  ~ route: messages.total_tokens < 200 → claude-3-haiku  (was: claude-3-5-sonnet)
  + cache: semantic, ttl=86400, threshold=0.92

Window: 2026-04-25 to 2026-05-25 (30 days)
Sample: 1,234,567 requests, 100% complete

Projected impact:
  Cost
    Current:    $4,247.13/mo
    Projected:  $1,683.42/mo  (-60.4%, save $2,563.71/mo)
    95% CI:     [$2,401.18, $2,726.24]

  Latency
    p50:        412ms → 287ms  (-30.3%)
    p95:        1,847ms → 1,203ms  (-34.9%)

  Cache hit rate
    Projected:  41.2%  (95% CI: 38.7%–43.6%)

Quality risk: LOW
  Sample: 1,000 requests
  Acceptable:  96.8%
  Marginal:    2.4%
  Degraded:    0.7%
  Wrong:       0.1%

  See "Flagged outputs" below for the 8 problematic samples.

Per-route breakdown:
  cheap-for-short      saved $2,401.50  (94% of total savings)
  semantic-cache       saved $162.21    (6%)

Caveats:
  - 47 requests had no recorded embedding; excluded from L2 projection.
  - Confidence interval is narrower for cost than for cache hit rate
    because cache hit rate has higher variance.

Apply this plan? [y/N]
```

---

## 15. Open questions

1. **Should Plan re-run failed requests too?** Currently excludes `status != 200`. Argument for: a cheaper model might also fail differently. Argument against: noise. Recommend exclude in v1, revisit if customer asks.

2. **How to handle requests that touched multiple routes?** A request might have hit a fallback chain in baseline. Plan currently models primary route only. Add fallback-chain modeling in v2.

3. **Tag-aware quality sampling?** Should we sample more from high-value tags? Recommend yes — let customer mark "critical" tags for higher sampling weight.

4. **Cross-org pattern learning for quality scoring?** If we learn that swapping Sonnet → Haiku on classification tasks is generally safe across all customers, can we lower the sample size needed? Big privacy implications; defer to v3.

5. **Auto-tune option?** Should Plan also *suggest* configs, not just evaluate user-supplied ones? Yes — call this "Plan Auto" in v2. User says "find me $500/month in savings"; Plan iteratively proposes diffs and ranks them.

6. **Real-time Plan?** Can Plan run continuously in the background, surfacing opportunities as they arise? Yes — that's Watch's job. Plan is the user-initiated, deeper analysis; Watch is the always-on equivalent.

---

## 16. Worked example

A customer's config change goes through Plan:

**Existing config:**
```yaml
routes:
  - name: default
    when: { always: true }
    then:
      target_model: ${request.model}
```

(In other words: pass through everything as-is.)

**Proposed diff:**
```yaml
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
  - name: default
    priority: 0
    when: { always: true }
    then:
      target_model: ${request.model}
```

**What Plan does:**

1. Fetch last 30 days of `request_logs` for the org: 1.2M requests
2. For each request, evaluate proposed routes:
   - 60% of requests are < 200 tokens → match `cheap-for-short` → projected model becomes `claude-3-5-haiku`
   - 40% remain on `default` → unchanged
3. For each `cheap-for-short` request, project cache outcome:
   - Build pgvector index over the 720K matching requests' embeddings
   - For each, look back for prior similar embeddings within TTL
   - ~41% project as cache hits
4. Compute cost per request:
   - Cache hits: $0
   - Cache misses with new model: input_tokens × Haiku input price + output_tokens × Haiku output price
   - Aggregate: $1,683.42 vs baseline $4,247.13
5. Sample 1,000 cheap-for-short requests stratified by tag and size:
   - Fetch raw bodies (opted-in)
   - Re-run against Haiku
   - Score divergence with judge model
   - 96.8% acceptable → risk band LOW
6. Bootstrap 10,000 iterations for CI on each aggregate metric
7. Render report

**User reviews, sees LOW risk, applies.**

7 days later, reconciliation:
- Actual cost was $1,752.30 vs projected $1,683.42 (+4.1%, within CI)
- Cache hit rate actually 39.1% vs projected 41.2% (within CI)
- No customer-facing quality complaints reported
- Trust score updates: "TokenTrimmer Plan has projected your costs within ±5% over the last 11 applied changes."

---

**End of Plan replay design.**
