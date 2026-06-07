# TokenTrimmer Gateway — API Reference

**Status:** v1 spec
**Base URL (hosted):** `https://api.tokentrimmer.com/v1`
**Base URL (self-hosted, default):** `http://localhost:8080/v1`

---

## Purpose

This is the public API contract for the TokenTrimmer Gateway. It defines what customers integrate against. The Gateway speaks the OpenAI Chat Completions and Embeddings API surface, with TokenTrimmer-specific extensions exposed via HTTP headers.

The promise to customers: **change one line — your `base_url` — and your existing OpenAI SDK code works.** Everything else is opt-in.

---

## 1. Compatibility statement

Gateway implements the following OpenAI API endpoints, with the OpenAI request/response schema as the source of truth:

| Endpoint | Method | Status |
|---|---|---|
| `/v1/chat/completions` | POST | ✓ v1 |
| `/v1/embeddings` | POST | ✓ v1 |
| `/v1/models` | GET | ✓ v1 |
| `/v1/completions` (legacy) | POST | ✗ not supported |
| `/v1/images/generations` | POST | ✗ not supported (v2 candidate) |
| `/v1/audio/transcriptions` | POST | ✗ not supported (v2 candidate) |
| `/v1/audio/speech` | POST | ✗ not supported (v2 candidate) |
| `/v1/files` | POST | ✗ not supported |
| `/v1/batches` | POST | ✗ not supported (v2 candidate) |
| `/v1/assistants` | * | ✗ not supported |

Gateway routes requests to any supported provider behind a unified OpenAI-format surface. Providers in v1:

- OpenAI
- Anthropic
- Google Gemini
- Mistral
- Groq
- Together AI
- OpenRouter
- Ollama (local)
- vLLM (local)
- LM Studio (local)
- Any OpenAI-compatible endpoint

Model identifiers follow `<provider>/<model>` convention when disambiguation is needed:

```
gpt-4o                        # OpenAI (provider inferred from default)
openai/gpt-4o                 # explicit OpenAI
anthropic/claude-3-5-sonnet
google/gemini-1.5-pro
groq/llama-3.1-70b-versatile
together/meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo
ollama/llama3:8b              # local
```

When the model name is unique across providers, the bare name works. Routes can also rewrite the model based on conditions (see configuration docs).

---

## 2. Authentication

### 2.1 Hosted mode

Use a TokenTrimmer API key in the `Authorization` header (Bearer scheme):

```http
POST /v1/chat/completions
Host: api.tokentrimmer.com
Authorization: Bearer tt_live_abc123...
Content-Type: application/json
```

TokenTrimmer keys are prefixed:
- `tt_live_*` — production
- `tt_test_*` — sandbox (no real provider calls, returns synthetic responses for testing)

OpenAI SDKs that pass the key via `api_key` parameter work without modification — the SDK forwards it as `Authorization: Bearer <key>`.

### 2.2 Self-hosted mode

Two options:

**Pass-through mode** (default): the customer's provider key is forwarded as-is to the upstream provider. The customer provides the provider API key in the request (via `Authorization` header) or in Gateway config.

**Managed mode:** customer issues TokenTrimmer-style local API keys (configured in YAML), Gateway resolves them to upstream provider credentials internally. Same UX as hosted.

### 2.3 Provider credentials

In hosted mode, provider credentials (OpenAI key, Anthropic key, etc.) are stored encrypted in the customer's TokenTrimmer org settings and selected based on the routed provider.

In self-hosted mode, provider credentials come from environment variables by default:

| Provider | Env var |
|---|---|
| OpenAI | `OPENAI_API_KEY` |
| Anthropic | `ANTHROPIC_API_KEY` |
| Google Gemini | `GEMINI_API_KEY` |
| Mistral | `MISTRAL_API_KEY` |
| Groq | `GROQ_API_KEY` |
| Together | `TOGETHER_API_KEY` |
| OpenRouter | `OPENROUTER_API_KEY` |

Local providers (Ollama, vLLM, LM Studio) don't require keys.

---

## 3. Chat completions

### 3.1 Endpoint

```
POST /v1/chat/completions
```

### 3.2 Request body

Follows the OpenAI Chat Completions schema. Full reference:

```json
{
  "model": "claude-3-5-sonnet",
  "messages": [
    { "role": "system", "content": "You are a helpful assistant." },
    { "role": "user", "content": "What is the capital of France?" }
  ],
  "temperature": 0.7,
  "top_p": 1.0,
  "max_tokens": 1024,
  "stream": false,
  "tools": [],
  "tool_choice": "auto",
  "response_format": { "type": "text" },
  "stop": null,
  "presence_penalty": 0,
  "frequency_penalty": 0,
  "n": 1,
  "seed": null,
  "user": "user_abc123"
}
```

**Required:** `model`, `messages`.

**Provider-specific parameter handling:**

- Parameters not supported by the routed provider are silently dropped, with a `X-TokenTrimmer-Warnings` response header noting the drop.
- Parameters with different ranges across providers (e.g., temperature) are clamped to the provider's valid range, with a `temperature_clamped` warning (e.g. Anthropic caps `temperature` at 1.0).
- For Anthropic-routed requests, `max_tokens` is required by Anthropic but optional here; Gateway defaults to 4096 if omitted.

### 3.3 Response (non-streaming)

```json
{
  "id": "chatcmpl-abc123",
  "object": "chat.completion",
  "created": 1716598234,
  "model": "claude-3-5-sonnet-20241022",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "The capital of France is Paris."
      },
      "finish_reason": "stop"
    }
  ],
  "usage": {
    "prompt_tokens": 23,
    "completion_tokens": 8,
    "total_tokens": 31,
    "cached_tokens": 0
  }
}
```

**TokenTrimmer additions:**
- `usage.cached_tokens` is always present (0 if no caching applied)
- `model` reflects the *actually used* model, which may differ from the requested model if a route rewrote it

### 3.4 Response (streaming)

When `stream: true`, the response is an SSE stream of `ChatCompletionChunk` events:

```
data: {"id":"chatcmpl-abc","object":"chat.completion.chunk","created":1716598234,"model":"claude-3-5-sonnet-20241022","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}

data: {"id":"chatcmpl-abc","object":"chat.completion.chunk","created":1716598234,"model":"claude-3-5-sonnet-20241022","choices":[{"index":0,"delta":{"content":"The "},"finish_reason":null}]}

... (more chunks) ...

data: {"id":"chatcmpl-abc","object":"chat.completion.chunk","created":1716598234,"model":"claude-3-5-sonnet-20241022","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":23,"completion_tokens":8,"total_tokens":31,"cached_tokens":0}}

data: [DONE]
```

Usage is included on the final chunk (this differs from OpenAI's default; can be toggled with `stream_options: {"include_usage": false}` to suppress).

**Cached responses + streaming:** if a request hits the cache and the client requested streaming, Gateway "fake-streams" the cached response back in chunks. This preserves UX consistency.

### 3.5 Tool calls

Standard OpenAI tool-call format. Gateway translates to provider-native formats internally.

```json
{
  "model": "gpt-4o",
  "messages": [
    { "role": "user", "content": "What's the weather in Tokyo?" }
  ],
  "tools": [
    {
      "type": "function",
      "function": {
        "name": "get_weather",
        "description": "Get current weather for a location",
        "parameters": {
          "type": "object",
          "properties": {
            "location": { "type": "string" }
          },
          "required": ["location"]
        }
      }
    }
  ]
}
```

Response with tool call:

```json
{
  "choices": [{
    "index": 0,
    "message": {
      "role": "assistant",
      "content": null,
      "tool_calls": [{
        "id": "call_abc",
        "type": "function",
        "function": {
          "name": "get_weather",
          "arguments": "{\"location\":\"Tokyo\"}"
        }
      }]
    },
    "finish_reason": "tool_calls"
  }]
}
```

### 3.6 Vision / multimodal

Image inputs follow OpenAI format:

```json
{
  "model": "gpt-4o",
  "messages": [{
    "role": "user",
    "content": [
      { "type": "text", "text": "What's in this image?" },
      { "type": "image_url", "image_url": { "url": "https://..." } }
    ]
  }]
}
```

Gateway translates to provider-specific image handling. Base64 data URLs and HTTPS URLs both supported.

### 3.7 Structured outputs

`response_format` supported per provider:

```json
{
  "response_format": { "type": "json_object" }
}
```

Or with schema:

```json
{
  "response_format": {
    "type": "json_schema",
    "json_schema": {
      "name": "weather_response",
      "schema": { ... }
    }
  }
}
```

If routed to a provider that doesn't support schema mode, Gateway rewrites `response_format` to `json_object` (dropping the schema) before dispatch and emits `X-TokenTrimmer-Warnings: response_format_downgrade`. (Providers that reject `response_format` outright — e.g. Anthropic — instead drop it and emit `param_dropped:response_format`.)

---

## 4. Embeddings

### 4.1 Endpoint

```
POST /v1/embeddings
```

### 4.2 Request

```json
{
  "model": "text-embedding-3-small",
  "input": "The quick brown fox",
  "dimensions": 1536,
  "encoding_format": "float"
}
```

`input` accepts a string or array of strings. `dimensions` is optional (some providers support truncation).

### 4.3 Response

```json
{
  "object": "list",
  "data": [{
    "object": "embedding",
    "index": 0,
    "embedding": [0.001, -0.023, ...]
  }],
  "model": "text-embedding-3-small",
  "usage": {
    "prompt_tokens": 5,
    "total_tokens": 5
  }
}
```

---

## 5. Models

### 5.1 List available models

```
GET /v1/models
```

Returns all models accessible to the authenticated key, across all configured providers.

```json
{
  "object": "list",
  "data": [
    {
      "id": "gpt-4o",
      "object": "model",
      "created": 1715367049,
      "owned_by": "openai",
      "tokentrimmer": {
        "provider": "openai",
        "pricing": {
          "input_per_million": 2.50,
          "output_per_million": 10.00,
          "cached_input_per_million": 1.25
        },
        "capabilities": ["text", "vision", "tools", "json_mode", "streaming"]
      }
    },
    {
      "id": "claude-3-5-sonnet",
      "object": "model",
      "created": 1717113600,
      "owned_by": "anthropic",
      "tokentrimmer": {
        "provider": "anthropic",
        "pricing": {
          "input_per_million": 3.00,
          "output_per_million": 15.00,
          "cached_input_per_million": 0.30
        },
        "capabilities": ["text", "vision", "tools", "streaming", "prompt_caching"]
      }
    }
  ]
}
```

The `tokentrimmer` object is the TokenTrimmer extension.

---

## 6. TokenTrimmer extension headers

All TokenTrimmer-specific behaviors are controlled via HTTP headers, so the request body stays OpenAI-pure.

### 6.1 Request headers

| Header | Purpose | Status | Example |
|---|---|---|---|
| `X-TokenTrimmer-Tag` | Free-form tag for cost attribution | Honored | `feature=chat-support,user=u_123` |
| `X-TokenTrimmer-Cost-Limit-Usd` | Reject (402) if estimated cost > limit | Honored | `0.05` |
| `X-TokenTrimmer-Cache` | Override cache behavior for this request (overrides the request-body `tt_extras.cache`; a privacy route's `disable_cache` still wins). | Honored | `bypass` / `force-write` / `read-only` / `disabled` |
| `X-TokenTrimmer-Route` | Force a specific named route, ignoring its conditions (unknown name → `400`; chat completions only). | Honored | `cheap-for-short` |
| `X-TokenTrimmer-Provider` | Pin the upstream provider for this request (routing still sets the model). Requires that provider's stored credential for cross-provider pins (else `400`); disables route fallbacks. Unknown provider → `400`. | Honored | `anthropic` |
| `X-TokenTrimmer-Fallback` | Comma-separated fallback chain (bare model ids) overriding the route's chain. Unresolvable or uncredentialed entries are skipped. Ignored when `X-TokenTrimmer-Provider` is set (a pin disables failover). | Honored | `gpt-4o-mini,claude-3-5-sonnet` |
| `X-TokenTrimmer-Timeout-Ms` | Per-request upstream timeout in ms (1–600000); `408` on expiry. Invalid/over-max values are ignored (the global 600s limit still applies). | Honored | `30000` |
| `X-TokenTrimmer-Trace-Parent` | W3C traceparent for distributed tracing | Planned (not yet honored) | (standard format) |

### 6.2 Response headers

| Header | Present | Example |
|---|---|---|
| `X-TokenTrimmer-Trace-Id` | every response | `5f3a1c...` |
| `X-TokenTrimmer-Latency-Ms` | every response | `412` |
| `X-TokenTrimmer-Provider` | on dispatched/cached responses | `anthropic` |
| `X-TokenTrimmer-Model-Used` | on dispatched/cached responses | `claude-3-5-haiku-20241022` |
| `X-TokenTrimmer-Cache` | on dispatched/cached responses | `hit-l1` / `hit-l2` / `miss` / `none` |
| `X-TokenTrimmer-Cost-Usd` | on dispatched/cached responses | `0.0034` |
| `X-TokenTrimmer-Baseline-Cost-Usd` | on dispatched/cached responses | `0.0218` |
| `X-TokenTrimmer-Saved-Usd` | on dispatched/cached responses | `0.0184` |
| `X-TokenTrimmer-Route-Matched` | the applied route's name, on routed responses (forced or condition-matched) | `cheap-for-short` |
| `X-TokenTrimmer-Warnings` | on dispatched responses, when the gateway altered the request | `param_dropped:frequency_penalty,param_dropped:n` |

`X-TokenTrimmer-Trace-Id` and `X-TokenTrimmer-Latency-Ms` are present on every
response (success or error). The cost/provider/model/cache headers are attached on
responses that reach dispatch, cache, or the sandbox path; they are not emitted on
early validation errors (4xx returned before dispatch).

`X-TokenTrimmer-Warnings` is a comma-separated list of tokens, emitted only when
the gateway altered the request before dispatch. Currently the gateway emits one
`param_dropped:<name>` token per request parameter the routed provider rejects and
the gateway drops during translation — e.g. Anthropic drops `n`, `seed`,
`response_format`, `presence_penalty`, and `frequency_penalty`; Gemini drops `n`,
`seed`, `presence_penalty`, `frequency_penalty`, and `user`; reasoning models
(`o3`, `o4-mini`) drop `temperature`. A `response_format_downgrade` token is
emitted when a `json_schema` request is routed to a provider declared
`json_object`-only; the built-in adapters either forward the schema (OpenAI,
Gemini, the OpenAI-compatible providers) or drop `response_format` outright
(Anthropic), so this fires only for providers explicitly marked object-only. A
`temperature_clamped` token is emitted when the request's `temperature` is
clamped to the routed provider's accepted range (e.g. a `1.5` request to
Anthropic, whose max is `1.0`).

### 6.3 Cache control semantics

`X-TokenTrimmer-Cache` values:
- `bypass` — skip the cache lookup, but still write/refresh the result (forces a fresh upstream call, then repopulates the cache)
- `force-write` — write to cache even for normally-ineligible requests (e.g. `temperature` > 0, `n` > 1, `seed` set). Tool-call responses are still never cached. USE WITH CAUTION; can poison the shared cache.
- `read-only` — look up cache but never write
- `disabled` — neither read nor write cache for this request

---

## 7. Error responses

All errors follow OpenAI's error response shape:

```json
{
  "error": {
    "message": "Rate limit exceeded for organization",
    "type": "rate_limit_exceeded",
    "code": "rate_limit_exceeded",
    "param": null
  }
}
```

### 7.1 Status codes

| Status | When |
|---|---|
| 200 | Success |
| 400 | Invalid request (bad params, malformed JSON) |
| 401 | Invalid or missing TokenTrimmer key |
| 402 | Subscription required (free tier exhausted), **or** cost limit exceeded when `X-TokenTrimmer-Cost-Limit-Usd` triggers — distinguish via the error body's `code` |
| 403 | Operation not permitted (e.g., model not enabled) |
| 404 | Model or route not found |
| 408 | Timeout exceeded |
| 413 | Request too large |
| 429 | Rate limited (TokenTrimmer key or upstream provider) |
| 500 | Internal Gateway error |
| 502 | Upstream provider returned 5xx after retries |
| 503 | Gateway temporarily unavailable |
| 504 | Upstream provider timeout |

### 7.2 Rate limit headers on 429

```
HTTP/1.1 429 Too Many Requests
Retry-After: 5
X-TokenTrimmer-RateLimit-Limit: 1000
X-TokenTrimmer-RateLimit-Remaining: 0
X-TokenTrimmer-RateLimit-Reset: 1716598234
```

### 7.3 Upstream provider errors

When the upstream provider returns an error, Gateway preserves the upstream message in the `error.message` field and adds metadata:

```json
{
  "error": {
    "message": "Anthropic API: max_tokens cannot exceed 8192 for this model",
    "type": "upstream_invalid_request",
    "code": "anthropic_max_tokens_exceeded",
    "param": "max_tokens",
    "tokentrimmer": {
      "provider": "anthropic",
      "upstream_status": 400,
      "fallback_attempted": false,
      "trace_id": "5f3a1c..."
    }
  }
}
```

---

## 8. Sandbox mode (test keys)

Test keys (`tt_test_*`) return synthetic responses without calling real providers:

- Chat completions return a deterministic response based on a hash of the request
- Embeddings return deterministic vectors
- All response headers and metadata are populated as if real
- Cost is reported as estimated, no actual charge

Useful for CI, integration tests, and SDK development.

---

## 9. Webhooks (hosted only)

Gateway can deliver event webhooks to a customer URL. Configured per org.

### 9.1 Events

- `request.completed` — every request (high-volume, disabled by default)
- `request.failed` — error responses only
- `budget.threshold_crossed` — usage crossed configured threshold (e.g., 80%)
- `budget.exceeded` — usage exceeded budget
- `anomaly.detected` — anomaly detection fired
- `cost.daily_summary` — daily roll-up
- `plan.applied` — Plan diff applied
- `inspect.scan_completed` — Inspect scan finished
- `inspect.high_severity_finding` — new high-severity finding

### 9.2 Delivery

POST to customer URL with body:

```json
{
  "id": "evt_abc",
  "type": "anomaly.detected",
  "created": 1716598234,
  "org_id": "org_abc",
  "data": {
    "metric": "hourly_cost",
    "current_value": 142.31,
    "baseline": 38.50,
    "deviation_sigma": 4.2,
    "first_seen_at": "2026-05-25T16:00:00Z"
  }
}
```

Headers:

```
X-TokenTrimmer-Event: anomaly.detected
X-TokenTrimmer-Signature: hmac-sha256=...
X-TokenTrimmer-Delivery-Id: del_abc
```

HMAC signature uses webhook secret configured per endpoint. Retries: exponential backoff up to 24 hours.

---

## 10. Configuration API (hosted only)

For programmatic management of routes, cache settings, and other configuration.

### 10.1 List routes

```
GET /v1/admin/routes
Authorization: Bearer tt_live_*
```

### 10.2 Create route

```
POST /v1/admin/routes
Content-Type: application/json
Authorization: Bearer tt_live_*

{
  "name": "cheap-for-short",
  "priority": 100,
  "when": {
    "messages.total_tokens": { "lt": 200 }
  },
  "then": {
    "target_model": "anthropic/claude-3-5-haiku",
    "cache": {
      "enabled": true,
      "ttl": 86400,
      "semantic_threshold": 0.92
    }
  },
  "enabled": true
}
```

### 10.3 Update / delete

```
PATCH /v1/admin/routes/:id
DELETE /v1/admin/routes/:id
```

### 10.4 Plan API

```
POST /v1/admin/plans
{
  "name": "Trial cheap-for-short",
  "diff": [
    { "op": "add", "path": "/routes/-", "value": { ... } },
    { "op": "replace", "path": "/cache/defaults/ttl", "value": 86400 }
  ],
  "window_days": 30,
  "quality_check_budget": 1000
}

→ 202 Accepted, returns plan_id

GET /v1/admin/plans/:id
→ Plan status and (when complete) results

POST /v1/admin/plans/:id/apply
→ Applies the diff
```

### 10.5 Inspect API

```
POST /v1/admin/inspect/runs
{
  "repo_url": "https://github.com/customer/repo",
  "commit_sha": "abc123",
  "auth": { "type": "github_app_install" }
}

→ Triggers a hosted Inspect scan

GET /v1/admin/inspect/runs/:id
→ Status + findings
```

### 10.6 Usage and billing

```
GET /v1/admin/usage?from=2026-05-01&to=2026-05-31
→ Aggregated usage for the period

GET /v1/admin/invoices
→ List of Stripe invoices
```

---

## 11. SDKs

TokenTrimmer ships official SDKs that wrap the official OpenAI and Anthropic SDKs and add convenience:

### 11.1 Python

```python
from tokentrimmer import Client

client = Client(api_key="tt_live_...")

# Standard chat completion
response = client.chat.completions.create(
    model="claude-3-5-sonnet",
    messages=[{"role": "user", "content": "Hello"}],
    tt_tag="feature=onboarding"   # convenience wrapper for header
)

# Access TokenTrimmer metadata
print(response.tt.cost_usd, response.tt.saved_usd, response.tt.cache)
```

### 11.2 TypeScript

```typescript
import { TokenTrimmer } from '@tokentrimmer/client';

const client = new TokenTrimmer({ apiKey: 'tt_live_...' });

const response = await client.chat.completions.create({
  model: 'claude-3-5-sonnet',
  messages: [{ role: 'user', content: 'Hello' }],
  ttTag: 'feature=onboarding',
});

console.log(response.tt.costUsd, response.tt.savedUsd, response.tt.cache);
```

Both SDKs are thin — the underlying request goes through the OpenAI/Anthropic SDK, with `base_url` set to TokenTrimmer. Customers can also use the OpenAI SDK directly without the TokenTrimmer wrapper.

---

## 12. Backwards compatibility

Once GA:
- The Chat Completions and Embeddings request/response schemas are stable.
- TokenTrimmer extension headers (`X-TokenTrimmer-*`) are additive; new headers may be introduced but existing ones won't change semantics.
- Provider-specific fields not in OpenAI's schema may be added under `tokentrimmer` objects but never as top-level fields that conflict with OpenAI.
- Deprecated behaviors get one minor version notice via `X-TokenTrimmer-Deprecation` header before removal.

---

## 13. Rate limits (hosted)

Per-key default limits:

| Tier | Requests/min | Tokens/min | Concurrent streams |
|---|---|---|---|
| Hobby | 60 | 100,000 | 5 |
| Pro | 600 | 1,000,000 | 50 |
| Scale | 6,000 | 10,000,000 | 500 |

Per-org monthly request quota tied to tier (see pricing page). Overage allowed up to 110% of tier, billed at $0.0001/request.

---

## 14. Latency expectations

Targets (p50, on cache miss):

| Region pair | Gateway overhead |
|---|---|
| Same region (Fly anycast) | < 5ms |
| Cross-continent | < 30ms |

On cache hit:

| Layer | Total response time |
|---|---|
| L1 (Redis) | < 10ms (vs hundreds for provider) |
| L2 (pgvector) | < 50ms |

Cache hits "fake-streamed" to maintain client-side streaming UX.

---

## 15. Versioning

Gateway versions follow semver. Customers can pin to a specific version via:

```
POST /v1/chat/completions
X-TokenTrimmer-Version: 1.4.0
```

Without a pin, the latest stable v1.x is served.

Major version bumps (v2) imply breaking changes and run on a separate base URL (`/v2/`).

---

## 16. OpenAI feature support matrix

| OpenAI feature | TokenTrimmer support |
|---|---|
| `messages` (text) | ✓ all providers |
| `messages` (image_url) | ✓ vision-capable providers |
| `messages` (input_audio) | ✗ v2 |
| `tools` / function calling | ✓ all major providers |
| `tool_choice` | ✓ where supported |
| Parallel tool calls | ✓ where supported (most) |
| `response_format: json_object` | ✓ where supported |
| `response_format: json_schema` | ✓ OpenAI; downgrades elsewhere |
| `stream: true` | ✓ all providers |
| `stream_options` | ✓ |
| `n > 1` | ✓ where supported |
| `logprobs` / `top_logprobs` | OpenAI only |
| `seed` | ✓ where supported |
| `temperature`, `top_p` | ✓ all (clamped to provider ranges) |
| `presence_penalty`, `frequency_penalty` | OpenAI only |
| `stop` sequences | ✓ all |
| `user` | ✓ all (forwarded for abuse detection) |
| `max_tokens` | ✓ all (required for Anthropic) |
| `service_tier` | OpenAI only |
| `metadata` | not forwarded; use `X-TokenTrimmer-Tag` instead |

---

## 17. Self-hosted differences

Self-hosted Gateway has all the above with these differences:

- No `/v1/admin/*` API surface (config managed via YAML files only in v1; admin API in v2)
- No webhooks (v2)
- No usage tracking beyond local Postgres
- Test keys (`tt_test_*`) work the same way
- Provider credentials from env vars or config file, not from a stored DB
- No subscription/billing; the binary is free

A `/health` endpoint is exposed for ops:

```
GET /health
→ 200 OK if Gateway is healthy
```

A `/metrics` endpoint is exposed for ops (Prometheus exposition format):

```
GET /metrics
→ 200 text/plain; version=0.0.4; charset=utf-8
```

Exposed metric families:

| Metric | Type | Labels |
|--------|------|--------|
| `http_requests_total` | counter | `method`, `endpoint`, `status` |
| `http_request_duration_seconds` | histogram | `method`, `endpoint` |
| `cache_lookups_total` | counter | `tier` (`l1`/`l2`), `result` (`hit`/`miss`) |
| `provider_failover_total` | counter | `from` |
| `provider_request_duration_seconds` | histogram | `provider`, `operation` |
| `catalog_zero_price_total` | counter | `provider`, `model` |
| `tt_build_info` | gauge | `version` |
| `process_uptime_seconds` | gauge | — |

> **Operator note:** `/metrics` is unauthenticated (like `/health`). Restrict it at the network / reverse-proxy layer so internal ops counters are not publicly scrapeable.

---

## 18. Examples

### 18.1 Curl

```bash
curl https://api.tokentrimmer.com/v1/chat/completions \
  -H "Authorization: Bearer tt_live_..." \
  -H "Content-Type: application/json" \
  -H "X-TokenTrimmer-Tag: feature=demo" \
  -d '{
    "model": "claude-3-5-sonnet",
    "messages": [{"role": "user", "content": "Hello"}]
  }'
```

### 18.2 OpenAI Python SDK

```python
from openai import OpenAI

client = OpenAI(
    base_url="https://api.tokentrimmer.com/v1",
    api_key="tt_live_...",
)

response = client.chat.completions.create(
    model="claude-3-5-sonnet",
    messages=[{"role": "user", "content": "Hello"}],
    extra_headers={"X-TokenTrimmer-Tag": "feature=demo"},
)
```

### 18.3 Vercel AI SDK

```typescript
import { createOpenAI } from '@ai-sdk/openai';

const tt = createOpenAI({
  baseURL: 'https://api.tokentrimmer.com/v1',
  apiKey: process.env.TOKENTRIMMER_KEY!,
  headers: { 'X-TokenTrimmer-Tag': 'feature=demo' },
});

const result = await generateText({
  model: tt('claude-3-5-sonnet'),
  prompt: 'Hello',
});
```

### 18.4 LangChain

```python
from langchain_openai import ChatOpenAI

llm = ChatOpenAI(
    base_url="https://api.tokentrimmer.com/v1",
    api_key="tt_live_...",
    model="claude-3-5-sonnet",
    default_headers={"X-TokenTrimmer-Tag": "feature=demo"},
)
```

### 18.5 Streaming with tool calls (Anthropic via Gateway)

```python
from openai import OpenAI

client = OpenAI(
    base_url="https://api.tokentrimmer.com/v1",
    api_key="tt_live_...",
)

stream = client.chat.completions.create(
    model="claude-3-5-sonnet",
    messages=[{"role": "user", "content": "What's the weather in Tokyo?"}],
    tools=[{
        "type": "function",
        "function": {
            "name": "get_weather",
            "parameters": {
                "type": "object",
                "properties": {"location": {"type": "string"}},
            },
        },
    }],
    stream=True,
)

for chunk in stream:
    if chunk.choices[0].delta.content:
        print(chunk.choices[0].delta.content, end="")
    if chunk.choices[0].delta.tool_calls:
        # handle tool call accumulation
        pass
```

---

## 19. FAQ for integrators

**Q: Do I need to change my code beyond `base_url`?**
Just the base URL and API key. Existing OpenAI SDK code works as-is. TokenTrimmer features (tags, cache control, etc.) are all opt-in via headers.

**Q: What happens if a provider is down?**
If your route has a fallback chain configured, Gateway tries each fallback in order. If no fallback or all fallbacks fail, Gateway returns a 502 with the upstream error preserved.

**Q: How is cost calculated?**
Tokens used × per-model pricing from our daily-refreshed pricing table. Reported in `X-TokenTrimmer-Cost-Usd` and reconciled against actual provider invoices monthly.

**Q: Does Gateway log my prompts?**
No, not by default. Only request metadata (token counts, model, route, latency). Opt-in per API key if you want raw body logging for Plan quality analysis.

**Q: Latency overhead?**
Sub-30ms p50 on cache miss, sub-10ms on cache hit. Streaming responses pass through chunk-by-chunk.

**Q: Can I use my existing OpenAI rate limits?**
Yes — Gateway forwards your provider key (in pass-through mode) so your existing provider rate limits apply on top of TokenTrimmer's. In hosted managed-credentials mode, TokenTrimmer's rate limits per your subscription tier apply.

**Q: How do I migrate from OpenRouter / Helicone / Portkey?**
Change `base_url` and `api_key`. Both OpenRouter and Helicone use OpenAI-compatible APIs, so the migration is a one-line change. Cache and routing config don't carry over; configure them in TokenTrimmer.

---

## 20. Stability commitments

| Surface | Stability |
|---|---|
| Chat completions request/response shape | Stable (OpenAI's contract) |
| Embeddings request/response shape | Stable |
| `X-TokenTrimmer-*` headers (existing) | Stable; additive new ones allowed |
| `tokentrimmer` extension object in models response | Stable |
| Admin API (`/v1/admin/*`) | Stable from v1 GA |
| Webhooks event shapes | Stable |
| SDK public APIs | Semver-stable |
| Self-hosted config file format | Stable from v1 GA |

---

## 21. Audit log integrity

The Gateway records every request as a hash-chained, Ed25519-signed audit entry. Operators can export and verify the chain with:

```bash
tt audit verify [--path .claude/AUDIT-CHAIN.jsonl] [--key-hex <hex>]
```

The verifying key is embedded in the export preamble automatically; pass `--key-hex` or `--key` to override it.

> **Detecting truncation.** `tt audit verify` confirms each entry links and is
> signed, and that `seq` is gap-free — so reordering or a mid-chain deletion is
> caught. It cannot, on its own, detect deletion of the most recent entries or of
> the entire chain (a truncated prefix still verifies). To detect that, pass
> `tt audit verify --expected-tip <seq>:<hash>`, where the tip is captured
> **out-of-band** from the gateway's `tt::audit::tip` log stream (shipped to an
> append-only sink). Do not source the tip from the same export — an export taken
> from a truncated database is self-consistent. The anchor is only as trustworthy
> as that off-box log pipeline; automatic WORM anchoring (S3 Object Lock) is the
> deferred full solution.

---

**End of API reference.**

Companion docs:
- Architecture spec (overall system design)
- Provider adapter guide (adding new providers)
- Plan replay design (how Plan simulations work)
- Inspect rule catalog (full rule list with detection logic)
