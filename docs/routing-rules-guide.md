# Routing rules

Routing is the TokenTrimmer differentiator: per-org rules that rewrite an
incoming request to a cheaper (or more capable) model *before* it is dispatched
— with optional cross-provider failover, a pre-dispatch estimated-cost admission
cap, and a
cache-opt-out for sensitive traffic. Rules are evaluated on the gateway's hot
path for every chat/embeddings request.

This guide covers what a route is, the conditions you can match on, the actions
a match can take, and how the engine picks a winner. Manage routes from the CLI
with [`tt route`](#managing-routes-with-tt-route) or directly over the gateway's
`/v1/routes` API (see `docs/04-gateway-api-reference.md` §10.7).

## What a route is

A route is a `when` / `then` pair plus some metadata:

```json
{
  "name": "gpt-4o->mini-for-short",
  "priority": 100,
  "enabled": true,
  "when": { "model_in": ["gpt-4o"], "input_tokens_lt": 200 },
  "then": { "target_model": "gpt-4o-mini" }
}
```

| Field      | Meaning                                                                      |
|------------|------------------------------------------------------------------------------|
| `name`     | Human-readable label, surfaced in dashboards and `tt route list`.            |
| `priority` | Higher wins on tie-break. The engine evaluates rules in descending priority. |
| `enabled`  | Disabled routes never match.                                                 |
| `when`     | AND-ed match conditions. Empty / omitted fields match anything.              |
| `then`     | What to do when matched.                                                     |

Each created route gets a stable `id` (a UUID), echoed back on create and used
for `tt route show <id>` / `tt route rm <id>` and for telemetry attribution
(`request_logs.matched_route_id`).

## Conditions (`when`)

All conditions in a `when` block are **AND-ed**: a route matches only when every
condition it specifies is satisfied. A field that is absent (or an empty list)
is ignored, so a route with an empty `when` matches every request.

| Condition                | Type        | Matches when…                                                                 |
|--------------------------|-------------|-------------------------------------------------------------------------------|
| `model_in`               | `[string]`  | the requested model is in this list. Empty list = any model.                  |
| `input_tokens_lt`        | `int`       | the estimated input-token count is **less than** this.                        |
| `input_tokens_gt`        | `int`       | the estimated input-token count is **greater than** this.                     |
| `tag_equals`             | `string`    | the request's `X-TokenTrimmer-Tag` header equals this value.                  |
| `has_images`             | `bool`      | the request carries at least one image input part (`true`), or none (`false`). |
| `has_audio`              | `bool`      | the request carries at least one audio input part (`true`), or none (`false`). |
| `prompt_contains_any_of` | `[string]`  | the request's user+system text contains **any** keyword (case-insensitive substring). |
| `estimated_cost_gt`      | `float` USD | the request's estimated cost is **greater than** this.                        |
| `estimated_cost_lt`      | `float` USD | the request's estimated cost is **less than** this.                           |
| `upstream_latency_ms_p95_gt` | `int` ms | the gateway's **live observed** p95 upstream latency for the requested model is **greater than** this. |
| `not_reasoning_class`        | `bool`   | `true` matches only requests **not** classified as Math / Code / Legal / Medical; `false` (or absent) matches everything. |

Notes that change behavior in practice:

- **Token counts are estimates supplied by the gateway**, not a re-tokenization
  inside the engine. `input_tokens_lt: 200` is "short prompts"; `input_tokens_gt`
  is "long prompts" (route those to a bigger context window).
- **Cost conditions need a known cost.** If the requested model has no pricing,
  the cost is unknown and `estimated_cost_gt` / `estimated_cost_lt` never match —
  the engine's general "unknown data → don't match" stance.
- **`has_images` / `has_audio` are capability-aware.** Creating a route that
  requires image or audio input (`has_images: true` / `has_audio: true`) is
  rejected at create time (`400`) if the `target_model` lacks the corresponding
  capability: `vision` for images, `audio` for audio. The two are independent;
  Vision support is not evidence of Audio input support. An unknown target is
  permissive.
- **`prompt_contains_any_of` is case-insensitive** and matches any one of the
  keywords. It is useful for keeping sensitive topics on a local/private model.
- **`upstream_latency_ms_p95_gt` is backed by a live, in-process signal.** The
  gateway keeps a bounded rolling window of the upstream latencies *it itself*
  observes per `(provider, model)` and checks that window's live p95 at route
  time — so the condition shifts traffic off a primary that is *currently* slow.
  It is **per-instance** (each gateway replica routes on what it has observed) and
  **cold-start safe**: until the window has enough recent samples for the model,
  the p95 is unknown and the condition does **not** match — a fresh or unknown
  primary is never gated on a fabricated signal. Not evaluable in Plan replay
  (historical logs have no in-process window), so a Plan never projects savings
  for a latency-gated route.
- **`not_reasoning_class` uses the gateway's reasoning-class classifier.** When
  `true`, the route fires only when the request is **not** classified as one of
  the four reasoning-is-the-work categories (Math / Code / Legal / Medical) —
  keeping cheaper models off the traffic where quality most matters. The
  classifier runs only when at least one active route sets
  `not_reasoning_class: true`; with no such route the classifier is skipped
  entirely (zero overhead). Used by `tt route catalog enable` to guard every
  curated down-route.

## Actions (`then`)

| Field          | Type        | Effect                                                                                       |
|----------------|-------------|----------------------------------------------------------------------------------------------|
| `target_model` | `string`    | rewrite the request to this model. **Required.** May cross providers (the target is dispatched on its own provider). |
| `fallbacks`    | `[string]`  | ordered fallback model ids, tried in order when the primary dispatch fails with a fallback-eligible error (provider down / 5xx / timeout). May cross providers. Empty = no failover. |
| `disable_cache`| `bool`      | matched requests skip L1 + L2 entirely — no lookup, no insert — for privacy/sensitive traffic that must not persist in the shared cache. Omitted when false. |
| `max_cost_usd` | `float` USD | a pre-dispatch estimated-cost admission cap. After the rewrite, if the rerouted model's estimated cost exceeds it, the gateway rejects the request with `402` instead of dispatching. It does not reserve or settle provider usage, so it is not a runtime spend or invoice ceiling. |
| `batch`        | `bool`      | **ADVISORY** batch-eligibility marker. The gateway dispatches synchronously today (no async Batch Lane yet): the request is served and billed normally, the request-log row is tagged batch-eligible, the **forgone** Batch-API discount (priced from the served model's real catalog batch rate — never a hardcoded 0.5×) is reported on `X-TokenTrimmer-Batch-Forgone-Usd`, and a `batch_deferred_unavailable` warning is emitted. Hard-ineligible: streaming requests and interactive clients (`X-TokenTrimmer-Interactive`, set by `tt chat` / the `/tools` loop) are cleared with `batch_ineligible:<reason>`; a served model with no catalog batch tier gets `batch_not_available:<model>` and no claim. Omitted when false. |
| `minify_json`  | `bool`      | **minified-JSON output steering** (research Phase 3.1, default `false`). The gateway appends a deterministic, conditionally-phrased system-suffix instruction ("When responding with JSON, emit it minified…") — lossless by construction (inter-token whitespace carries no meaning) and inert for non-JSON answers. Never injected under provider-native strict structured output (`response_format: json_schema` honored natively — `minify_skipped:structured_output`, no claim). The per-response saving is an **ESTIMATE** (the emitted JSON re-rendered pretty and re-tokenized with the served model's tokenizer), surfaced on its own `X-TokenTrimmer-Minify-Saved-Est-Usd` header / `request_logs.minify_saved_est_usd` column and **never** folded into `Saved-Usd`. Non-JSON responses and streaming (v1) book `$0`. Warnings token `output_minified`. Omitted when false. |
| `reasoning_max_effort` | `string` | cap OpenAI-style `reasoning_effort` at `"low"` or `"medium"` (research Phase 3.2, default off). **Lower-only** (`minimal < low < medium < high`; never raises) — an absent effort on a catalog-Reasoning-capable model is treated as the provider default (`medium`) and lowered when the cap is `low`. **HARD class gate:** requests classified math/code/legal/medical are never capped (`reasoning_cap_skipped:class:<c>`) — capping where reasoning IS the work yields confidently-wrong answers. Books `$0` per request (unspent thinking tokens are only statistically visible); the event is metered (`reasoning_capped_total`) and the judge-tax-netted route savings report carries the truth. Warnings token `reasoning_capped:reasoning_effort:<cap>` when applied. |
| `reasoning_budget_tokens` | `int` | cap Anthropic-style extended-thinking spend: an ENABLED `thinking` config whose `budget_tokens` exceeds this is lowered to it (never expressed via `max_tokens` — Anthropic's `max_tokens` INCLUDES thinking; never enables thinking on a request that didn't ask for it). Minimum `1024` (Anthropic's floor, validated at create time). Same class gate / `$0` booking as `reasoning_max_effort`. Warnings token `reasoning_capped:thinking_budget:<cap>` when applied. |
| `format_switch`| `string`    | **opt-in, contract-changing**: instruct the model to emit `"csv"` (flat-uniform record asks) or `"bare"` (single-value asks) instead of verbose JSON — output tokens bill ~4-5× input, so trimming the emission is the highest-rate lever. **This changes what your parser receives** — only enable it on routes whose callers expect it; every switched response is advertised with a `format_switch:<csv\|bare>` warnings token (cache hits included). Eligibility is enforced in code from the request's `response_format` **schema shape** (csv: an array of all-scalar objects, or an object wrapping exactly one such array; bare: a single-scalar-property object or root scalar). No schema, `json_object`, **strict** `json_schema` (grammar lock), nested schemas, streaming, tools, and `n>1` all no-op with a `format_switch_skipped:<reason>` token — when unsure, the gateway does nothing. The emission is strip-validated (fences/prose preamble removed, shape checked); a non-conforming body **fails open**: the untouched body is served with `format_switch_failed:<label>`, $0 booked, and the response is never cached under the switched key. Savings are a **labeled estimate** (`X-TokenTrimmer-Format-Switch-Saved-Est-Usd`), never folded into `Saved-Usd`. Mutually exclusive with `diff`; values other than `csv`/`bare` are rejected at create time. Omitted when unset. |
| `diff`         | `bool`      | **opt-in delta/diff responses** for edit/iteration routes. When the prior artifact is identifiable **from the request** — an explicit `tt_extras.diff_prior` string echo (preferred; consumed by the gateway and stripped before upstream dispatch) or the last assistant message in the history (the common edit-loop shape) — the gateway instructs the model to emit an anchored search/replace patch, applies + validates it (each anchor must match exactly once; the result must parse as JSON when the caller set a JSON `response_format`), and returns the **full reconstructed artifact**: your client sees a normal completion while the provider reports only the short patch usage. TokenTrimmer cost/savings fields are catalog-priced gateway estimates, not provider billing evidence. ANY validation failure **fails closed** to a full re-emit of your exact original request (`diff_failed:<reason>`; the retry is real spend, folded into `Cost-Usd` and itemized on `X-TokenTrimmer-Diff-Failed-Cost-Usd`; if even the re-emit dispatch errors, the raw patch is served marked `diff_degraded` — never a 5xx). The measured saving (`X-TokenTrimmer-Diff-Saved-Usd` — real tokenizer counts on both sides) rides `Saved-Usd`. No identifiable prior / prior under 200 chars / streaming / tools / `n>1` / strict `json_schema` all no-op with `diff_skipped:<reason>`. Diff traffic never uses the L1/L2 cache. Mutually exclusive with `format_switch`. Omitted when false. |
| `format_switch`| `string`    | **opt-in, contract-changing**: instruct the model to emit `"csv"` (flat-uniform record asks) or `"bare"` (single-value asks) instead of verbose JSON — output tokens bill ~4-5× input, so trimming the emission is the highest-rate lever. **This changes what your parser receives** — only enable it on routes whose callers expect it; every switched response is advertised with a `format_switch:<csv\|bare>` warnings token (cache hits included). Eligibility is enforced in code from the request's `response_format` **schema shape** (csv: an array of all-scalar objects, or an object wrapping exactly one such array; bare: a single-scalar-property object or root scalar). No schema, `json_object`, **strict** `json_schema` (grammar lock), nested schemas, streaming, tools, and `n>1` all no-op with a `format_switch_skipped:<reason>` token — when unsure, the gateway does nothing. The emission is strip-validated (fences/prose preamble removed, shape checked; CSV records are one-per-line — the instruction forbids embedded newlines in field values); a non-conforming OR truncated body (`finish_reason` other than `"stop"` — a token-limit cut on a record boundary would pass shape checks while silently dropping records) **fails open**: the untouched body is served with `format_switch_failed:<label>`, $0 booked, and the response is never cached under the switched key. Savings are a **labeled estimate** (`X-TokenTrimmer-Format-Switch-Saved-Est-Usd`), never folded into `Saved-Usd`. On a canary route (`traffic_pct`) the control arm is served fully unchanged — the switch runs on the canary arm only. Mutually exclusive with `diff`; values other than `csv`/`bare` are rejected at create time. Omitted when unset. |
| `diff`         | `bool`      | **opt-in delta/diff responses** for edit/iteration routes. When the prior artifact is identifiable **from the request** — an explicit `tt_extras.diff_prior` string echo (preferred; consumed by the gateway and stripped before upstream dispatch) or the last assistant message in the history (the common edit-loop shape) — the gateway instructs the model to emit an anchored search/replace patch, applies + validates it (each anchor must match exactly once; the result must parse as JSON when the caller set a JSON `response_format`), and returns the **full reconstructed artifact**: your client sees a normal completion while the provider reports only the short patch usage. TokenTrimmer cost/savings fields are catalog-priced gateway estimates, not provider billing evidence. ANY validation failure — including a truncated patch emission (`finish_reason` other than `"stop"`, which could parse as a valid-but-incomplete patch) — **fails closed** to a full re-emit (`diff_failed:<reason>`; the retry is real spend, folded into `Cost-Usd` and itemized on `X-TokenTrimmer-Diff-Failed-Cost-Usd`; if even the re-emit dispatch errors, the raw patch is served marked `diff_degraded` — never a 5xx). The re-emit is the dispatched request minus the patch instruction, so it carries every guardrail the patch dispatch carried (`redact` redaction included — safe to combine `redact` + `diff`). The measured saving (`X-TokenTrimmer-Diff-Saved-Usd` — real tokenizer counts on both sides) rides `Saved-Usd`. No identifiable prior / prior under 200 chars / a non-string `tt_extras.diff_prior` (`bad_prior_type` — never silently swapped for the history) / streaming / tools / `n>1` / strict `json_schema` all no-op with `diff_skipped:<reason>`. Diff traffic never uses the L1/L2 cache. On a canary route (`traffic_pct`) the control arm is served fully unchanged — the lever runs on the canary arm only. **Grammar limit (fails safe):** a document whose to-be-edited region contains a line that is exactly a patch marker after trimming (e.g. a 7-char `=======` markdown setext underline) cannot be quoted in any valid patch — every edit touching it fails closed and pays the re-emit retry tax; don't enable `diff` on routes serving such documents. Mutually exclusive with `format_switch`. Omitted when false. |
| `auto_pause`   | `bool`      | **opt-in quality auto-pause** (default `false`). When the route's recent paired-judge pass-rate regresses below the floor, the gateway sticky-pauses the route's rewrite. See [Auto-pause](#auto-pause-quality-circuit-breaker) below. |
| `pause_floor_pass_rate` | `float` | pass-rate floor as a fraction in `(0, 1]`. Default `0.90`. Validated at create time even when `auto_pause` is false. |
| `pause_min_verdicts`    | `int`   | minimum classified verdicts (acceptable + degraded) in the window before the floor can trigger. Default `20`; must be ≥ 1. |

A vision-capable target is required whenever the `when` block gates on
`has_images` / `has_audio` (see above).

## Auto-pause (quality circuit breaker)

A down-route trades cost for quality, and the gateway's sampled paired A/B
judge (`TT_JUDGE_*`) scores a deterministic slice of its rerouted traffic
(`acceptable` / `degraded` / `unclear`). Output-shaped responses (a validated
`format_switch` or an applied `diff`) sample the same judge even when the
route did not change the model's price — the served (switched/reconstructed)
answer is scored against a reference produced from the pre-instruction
original request, so a shaping route with `auto_pause: true` gets the quality
circuit breaker for free (note the judge may penalize the CSV/bare format
itself — conservative in the quality-safe direction). With `auto_pause: true`
on the route,
the gateway watches the **most recent 100 classified verdicts** for that
route; when at least `pause_min_verdicts` (default 20) have accumulated and
the pass-rate (`acceptable / (acceptable + degraded)` — `unclear` never
counts) drops **strictly below** `pause_floor_pass_rate` (default `0.90`), the
route is **paused** automatically.

The circuit breaker is doubly opt-in and needs two things at boot to exist at
all: the judge must be enabled (`TT_JUDGE_ENABLED=1` — no judge, no verdicts,
nothing to evaluate) **and** the gateway must run with `DATABASE_URL` set (the
verdict window and the pause record are Postgres-backed). The shipped binary
wires the evaluator automatically when both hold; `auto_pause: true` on a
route still validates without them, but only manual pause/resume is in effect
until they do.

What a pause means:

- **Matched but not rewritten.** The route still matches — requests attribute
  to it (`X-TokenTrimmer-Route-Matched`, a `route_paused:<name>` warnings
  token, `request_logs.route_paused = true`) — but the rewrite and every
  other **cost** lever (`fallbacks`, `flex`, `compress`, `traffic_pct`,
  `shadow_model`, `max_cost_usd`, `minify_json`, `reasoning_max_effort`,
  `reasoning_budget_tokens`) are suppressed. Requests flow to their
  originally-requested model: the **expensive, quality-safe** direction, so a
  malfunctioning quality gate can only ever cost you money, never quality.
- **Safety levers stay on.** `redact` and `disable_cache` keep applying while
  paused — pausing a quality gate never disables a privacy guardrail.
- **Sticky.** The pause persists (its own `route_pauses` row, untouched by
  dashboard edits to the route) until an explicit
  `POST /v1/routes/:id/resume?expected_revision=N`. A paused route stops rewriting, so it stops
  being judged, so its verdict window freezes — it can never un-pause itself.
  One exception by construction: **deleting the route deletes its pause
  record**, so the delete-and-re-create edit flow yields a fresh, unpaused
  route — re-pause it explicitly if the quality concern still stands.
- **Resume restarts the evidence.** A resume stamps a `resumed_at` watermark
  on the retained pause record, and the evaluator only counts verdicts
  recorded **after** it. The just-resumed route therefore needs
  `pause_min_verdicts` fresh classified verdicts before the floor can trigger
  again — its frozen, mostly-degraded pre-pause window can never instantly
  re-pause it. (At the default ~2% sample rate, accumulating 20 fresh verdicts
  takes on the order of a thousand matched requests — quality confidence
  rebuilds at the same pace it was earned.)
- **No saving is faked.** A paused passthrough books zero routing saving
  (served model == requested model), and the route-level
  `GET /v1/routes/:id/savings` report nets the judge/shadow measurement tax
  out of the route's gross saving — itemized, never silently subtracted.
- **Convergence.** Pause/resume invalidates the acting replica's route cache
  immediately; other replicas converge within the 60-second route-cache TTL.
- A forced `X-TokenTrimmer-Route` header does **not** bypass a pause.

`POST /v1/routes/:id/pause?expected_revision=N` provides the same sticky pause manually. Both
auto and manual pauses are visible as `"paused": true` on
`GET /v1/routes` / `GET /v1/routes/:id`, and resumable only via
`POST /v1/routes/:id/resume?expected_revision=N`. Both pause and resume require
the positive generation token returned by a recent route management read, so a
delayed request cannot affect a replacement route that reused an id. Endpoint details live in the
[gateway API reference](04-gateway-api-reference.md#107-self-hosted-gateway-routes-api).

## How a route is chosen

1. The engine holds the org's **enabled** routes sorted by **descending
   priority**.
2. It iterates and returns the **first** route whose `when` block matches the
   request — **first match wins**, ties broken by priority order.
3. If no enabled route matches, the request is dispatched **unchanged**.

Because evaluation stops at the first match, ordering matters: put more specific,
higher-value rules at a higher `priority` than broad catch-alls. A `priority`
default of `100` is applied when you don't set one.

## Managing routes with `tt route`

`tt route` talks to the gateway's `/v1/routes` API using your resolved API key
(see [`tt login`](tt-cli-commands.md#authentication)). All four subcommands require a key.

```bash
tt route list                 # table of NAME, ROUTE, PRIO, STATUS
tt route show <id>            # the full JSON for one route
tt route rm <id>              # delete one route by id
tt route add ...              # create a route (see below)
```

### `tt route add`

The CLI maps friendly flags onto the `when` / `then` JSON above. A target is
required: pass `--always <model>` (match-all) **or** `--from <m> --to <m>`
(rewrite one model to another) — not both.

| Flag                          | Maps to                                          |
|-------------------------------|--------------------------------------------------|
| `--always <model>`            | `then.target_model`, with an empty `when` (match all) |
| `--from <model>`              | `when.model_in = [model]`                         |
| `--to <model>`                | `then.target_model`                              |
| `--when-has-images`           | `when.has_images = true`                          |
| `--when-has-audio`            | `when.has_audio = true`                           |
| `--when-tag <value>`          | `when.tag_equals`                                |
| `--when-prompt-contains <kw>` | appended to `when.prompt_contains_any_of` (repeatable) |
| `--when-cost-gt <usd>`        | `when.estimated_cost_gt`                         |
| `--when-cost-lt <usd>`        | `when.estimated_cost_lt`                         |
| `--when-p95-gt <ms>`          | `when.upstream_latency_ms_p95_gt`               |
| `--max-cost <usd>`            | `then.max_cost_usd`                              |
| `--disable-cache`             | `then.disable_cache = true`                      |
| `--batch`                     | `then.batch = true` — advisory batch-eligibility marker (Batch Lane forgone-savings attribution; never applied to streaming/interactive requests) |
| `--fallback <model>`          | appended to `then.fallbacks` (repeatable)        |
| `--priority <n>`              | `priority` (default `100`)                       |
| `--name <name>`               | `name` (default `<from>-><target>` or `all-><target>`) |
| `--disabled`                  | create the route `enabled: false`                |

> `tt route add` does **not** expose `input_tokens_lt` / `input_tokens_gt`. To
> match on token count, POST the JSON directly to `/v1/routes` (the field names
> are in the conditions table above).

There is **no update** — to change a route, `rm` it and `add` it again
(the gateway serves no `PATCH /v1/routes/:id`).

## Examples

Route every short `gpt-4o` prompt to `gpt-4o-mini`:

```bash
tt route add --from gpt-4o --to gpt-4o-mini --name short-gpt4o
# (then POST input_tokens_lt to /v1/routes if you want to gate on length)
```

Send all traffic to a cheap model, but fail over to two others if it's down:

```bash
tt route add --always gpt-4o-mini \
  --fallback claude-haiku-4-5 --fallback gemini-3.5-flash \
  --name cheap-with-failover
```

Keep sensitive prompts on a local model and out of the shared cache:

```bash
tt route add --always ollama/llama3 \
  --when-prompt-contains confidential --when-prompt-contains salary \
  --disable-cache --priority 200 --name sensitive-local
```

Reject any request whose post-rewrite estimate exceeds 10¢:

```bash
tt route add --from gpt-4o --to gpt-4o --max-cost 0.10 --name cost-cap
```

Route vision requests to a vision-capable model (validated at create time):

```bash
tt route add --from gpt-4o --to gpt-4o --when-has-images --name vision-passthrough
```

## Where it runs

- **Live gateway:** `crates/routing` — `RoutingEngine::evaluate` /
  `evaluate_with_cost`, called by the chat/embeddings handlers in `crates/core`.
- **Plan replay:** `tt_plan_core` mirrors the same condition shape so a Plan
  projection and the live gateway agree on which route would fire for a request.
- **Storage:** the gateway reads routes from Postgres (the `routes` table the
  dashboard / `tt route` writes) behind a ~60-second per-org cache, so a new
  route takes up to about a minute to take effect on the hot path.

## Requirements

Route management requires a gateway with a routing store configured (a
`DATABASE_URL`-backed deployment). A gateway with no DB pool runs unrouted and
returns `503 route management is not configured on this gateway`. Anonymous,
sandbox (`tt_test_*`), and dogfood callers cannot manage routes (`401`).

See `docs/04-gateway-api-reference.md` §10.7 for the raw `/v1/routes` API.
