# Routing rules

Routing is the TokenTrimmer differentiator: per-org rules that rewrite an
incoming request to a cheaper (or more capable) model *before* it is dispatched
— with optional cross-provider failover, a hard per-request cost ceiling, and a
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

Notes that change behavior in practice:

- **Token counts are estimates supplied by the gateway**, not a re-tokenization
  inside the engine. `input_tokens_lt: 200` is "short prompts"; `input_tokens_gt`
  is "long prompts" (route those to a bigger context window).
- **Cost conditions need a known cost.** If the requested model has no pricing,
  the cost is unknown and `estimated_cost_gt` / `estimated_cost_lt` never match —
  the engine's general "unknown data → don't match" stance.
- **`has_images` / `has_audio` are capability-aware.** Creating a route that
  requires image or audio input (`has_images: true` / `has_audio: true`) is
  rejected at create time (`400`) if the `target_model` lacks the `vision`
  capability — you can't route a vision request to a text-only model. An unknown
  target is permissive.
- **`prompt_contains_any_of` is case-insensitive** and matches any one of the
  keywords. It is useful for keeping sensitive topics on a local/private model.

## Actions (`then`)

| Field          | Type        | Effect                                                                                       |
|----------------|-------------|----------------------------------------------------------------------------------------------|
| `target_model` | `string`    | rewrite the request to this model. **Required.** May cross providers (the target is dispatched on its own provider). |
| `fallbacks`    | `[string]`  | ordered fallback model ids, tried in order when the primary dispatch fails with a fallback-eligible error (provider down / 5xx / timeout). May cross providers. Empty = no failover. |
| `disable_cache`| `bool`      | matched requests skip L1 + L2 entirely — no lookup, no insert — for privacy/sensitive traffic that must not persist in the shared cache. Omitted when false. |
| `max_cost_usd` | `float` USD | a hard per-request ceiling. After the rewrite, if the rerouted model's estimated cost still exceeds this, the gateway rejects the request with `402` instead of dispatching. |

A vision-capable target is required whenever the `when` block gates on
`has_images` / `has_audio` (see above).

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
(see [`tt login`](#authentication)). All four subcommands require a key.

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
| `--max-cost <usd>`            | `then.max_cost_usd`                              |
| `--disable-cache`             | `then.disable_cache = true`                      |
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
