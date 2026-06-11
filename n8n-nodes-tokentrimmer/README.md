# n8n-nodes-tokentrimmer

An [n8n](https://n8n.io) community node for the [TokenTrimmer](https://tokentrimmer.com) LLM gateway. It adds a **TokenTrimmer Chat** node that sends OpenAI-shaped chat completions through the gateway, plus a **TokenTrimmer API** credential. The differentiator: every execution's output carries a `costInfo` object parsed from the gateway's `x-tokentrimmer-*` response headers — including `savedUsd` — so the money each workflow run saved is visible right in the n8n output panel and available to downstream nodes.

Example `costInfo` (values from the [Gateway API reference](https://github.com/tokentrimmer/tokentrimmer/blob/main/docs/04-gateway-api-reference.md) §6.2 examples):

```json
{
  "costUsd": 0.0034,
  "baselineCostUsd": 0.0218,
  "savedUsd": 0.0184,
  "savingsPct": 84.4,
  "providerCacheSavedUsd": 0.0009,
  "modelUsed": "claude-3-5-haiku-20241022",
  "provider": "anthropic",
  "cache": "hit-l1",
  "traceId": "5f3a1c..."
}
```

## Install

> **Not yet on npm** — the package publishes at launch. Until then, install from source.

**Once published** (n8n self-hosted): Settings → Community Nodes → Install → enter `n8n-nodes-tokentrimmer` → Install. See the [n8n community nodes docs](https://docs.n8n.io/integrations/community-nodes/installation/) for details.

**From source** (works today):

```bash
git clone https://github.com/tokentrimmer/tokentrimmer.git
cd tokentrimmer/n8n-nodes-tokentrimmer
npm install && npm run build

# Link it into your n8n instance:
cd ~/.n8n/nodes
npm install /path/to/tokentrimmer/n8n-nodes-tokentrimmer
```

Then restart n8n. The **TokenTrimmer Chat** node appears in the node picker.

## Credential setup

Create a **TokenTrimmer API** credential:

| Field | Value |
|---|---|
| API Key | Hosted: a TokenTrimmer key (`tt_live_*` / `tt_test_*`). Self-hosted pass-through: your provider API key, forwarded upstream. |
| Base URL | Gateway origin **without** `/v1` — the node appends it. Default `https://api.tokentrimmer.com`; self-host: `http://localhost:8080`. If your n8n runs in Docker and the gateway on the host, use `http://host.docker.internal:8080`. |

The credential test probes `GET /v1/models`. Tip: `tt_test_*` sandbox keys return synthetic responses with all cost headers populated as if real ([API reference](https://github.com/tokentrimmer/tokentrimmer/blob/main/docs/04-gateway-api-reference.md) §8) — handy for trying the node without spend.

## Node usage

**TokenTrimmer Chat** calls `POST {baseUrl}/v1/chat/completions` (non-streaming).

| Parameter | Meaning |
|---|---|
| Model | Model ID, e.g. `claude-haiku-4-5`; use `<provider>/<model>` to disambiguate. A routing rule may rewrite it. |
| Input Mode | `Prompt` (single user prompt + optional system prompt) or `Messages (JSON)` (raw OpenAI messages array). |
| Prompt / System Prompt | Expression-friendly, e.g. `{{ $json.chatInput }}`. |
| Messages (JSON) | Passed through untouched — string or array-of-parts content both work. |

Options (request headers use these exact names):

| Option | Sent as |
|---|---|
| Tag | `X-TokenTrimmer-Tag` request header (per-feature cost attribution on the hosted dashboard) |
| Cost Limit (USD) | `X-TokenTrimmer-Cost-Limit-Usd` request header — the gateway rejects with **402** if the estimated cost exceeds it |
| Cache Override | `X-TokenTrimmer-Cache` request header (`bypass` / `force-write` / `read-only` / `disabled`) |
| Max Tokens | `max_tokens` in the request body |
| Temperature | `temperature` in the request body |

## Output

Each output item is the OpenAI-shaped completion (`id`, `choices`, `usage` incl. `cached_tokens`, …) plus a top-level `costInfo` object:

| Field | Source |
|---|---|
| `costUsd` | `x-tokentrimmer-cost-usd` response header |
| `baselineCostUsd` | `x-tokentrimmer-baseline-cost-usd` |
| `savedUsd` | `x-tokentrimmer-saved-usd` (savings caused by TokenTrimmer: routing + TT cache) |
| `savingsPct` | **Computed by the node** (`savedUsd / baselineCostUsd × 100`) — not a gateway header. `null` when baseline is missing or zero. |
| `providerCacheSavedUsd` | `x-tokentrimmer-provider-cache-saved-usd` |
| `modelUsed` | `x-tokentrimmer-model-used` |
| `provider` | `x-tokentrimmer-provider` |
| `cache` | `x-tokentrimmer-cache` (`hit-l1` / `hit-l2` / `neg-hit` / `miss` / `none` / `sandbox`) |
| `traceId` | `x-tokentrimmer-trace-id` |

All fields are nullable — cost headers are absent on early-rejected (4xx) responses.

## Development

```bash
npm install
npm run typecheck   # tsc on src + tests
npm run build       # emits dist/ (CommonJS) + copies the node icon
npm test            # vitest unit tests for the pure request/header logic
```

## License

Apache-2.0 — see [LICENSE](https://github.com/tokentrimmer/tokentrimmer/blob/main/LICENSE).
