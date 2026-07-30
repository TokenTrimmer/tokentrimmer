# TokenTrimmer Gateway — API Reference

**Status:** v1 spec
**Base URL (hosted):** `https://api.tokentrimmer.com/v1`
**Base URL (self-hosted, default):** `http://localhost:8080/v1`

> **Hosted gateway launching soon** *(as of 2026-06-10)* — the hosted base URL `https://api.tokentrimmer.com/v1` is not live yet. Self-host with Docker today and integrate against `http://localhost:8080/v1`; every example below works unchanged once you swap the base URL. See `GETTING_STARTED.md` for the one-`docker run` quickstart.

---

## Purpose

This is the public API contract for the TokenTrimmer Gateway. It defines what customers integrate against. The Gateway speaks the OpenAI Chat Completions and Embeddings API surface, with TokenTrimmer-specific extensions exposed via HTTP headers.

The promise to customers: **change one line — your `base_url` — and your existing OpenAI SDK code works.** Everything else is opt-in.

---

## 1. Compatibility statement

Gateway implements the following OpenAI API endpoints, with the OpenAI request/response schema as the source of truth, plus an Anthropic-native `/v1/messages` ingress for Anthropic-wire clients (Claude Code, the Anthropic SDKs):

| Endpoint | Method | Status |
|---|---|---|
| `/v1/chat/completions` | POST | ✓ v1 |
| `/v1/messages` (Anthropic Messages wire) | POST | ✓ v1 |
| `/v1/embeddings` | POST | ✓ v1 |
| `/v1/models` | GET | ✓ v1 |
| `/v1/capabilities` | GET | ✓ gateway-runtime evidence v1 |
| `/v1/completions` (legacy) | POST | ✗ not supported |
| `/v1/responses` (OpenAI Responses API) | POST | ✗ not yet supported — use `/v1/chat/completions` |
| `/v1/images/generations` | POST | ✗ not supported (v2 candidate) |
| `/v1/audio/transcriptions` | POST | ✗ not supported (v2 candidate) |
| `/v1/audio/speech` | POST | ✗ not supported (v2 candidate) |
| `/v1/files` | POST | ✓ Batch Lane input upload |
| `/v1/files/:id/content` | GET | ✓ org-owned Batch result/error stream |
| `/v1/batches` | POST, GET | ✓ create and list Batch jobs |
| `/v1/batches/:id` | GET | ✓ fetch an org-owned Batch job |
| `/v1/batches/:id/cancel` | POST | ✓ cancel an org-owned Batch job |
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

### 1.1 Batch Lane ownership and streaming

Batch Lane is currently OpenAI-only and requires a verified `tt_live_*` key, a
stored OpenAI credential, and a configured durable Batch store. Upload/create,
status, list, and cancellation use OpenAI-compatible request/response shapes.
Every durable row is scoped to the organization derived from the gateway key;
unknown and cross-organization Batch ids return `404`.

`GET /v1/files/:id/content` requires the exact input, output, or error file id
to appear on one of that organization's durable Batch rows before the gateway
makes any provider request. Possession of the same upstream OpenAI credential
does not establish ownership because multiple TokenTrimmer organizations can
share an upstream account. A successful provider response is streamed through
the gateway without being accumulated by the provider adapter, gateway handler,
Rust streaming client, or CLI download command. Provider retention, the
organization's live gateway key, and its matching upstream credential still
govern availability; this API does not promise that a provider-held file is
retained.

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

In hosted mode, provider credentials (OpenAI key, Anthropic key, etc.) are stored encrypted in the customer's TokenTrimmer org settings and selected based on the routed provider. The hosted gateway is **BYO-only**: an org that has not added its own credential for the requested provider gets a `400` with code `missing_provider_credential` telling it to add one — it is never served the operator's keys.

In self-hosted deployments with a Postgres credential store (`DATABASE_URL` + `TT_MASTER_KEY`), the same BYO-only default applies. A **single-tenant** self-host can opt in to serving the operator's process-env keys as a fallback for orgs with no stored credential by setting `TT_ALLOW_ENV_CREDENTIAL_FALLBACK=1` (do **not** set this on a multi-tenant gateway — every org would spend on, and be exposed to, the shared keys). With the opt-in set, these env vars are recognized:

| Provider | Env var |
|---|---|
| OpenAI | `OPENAI_API_KEY` |
| Anthropic | `ANTHROPIC_API_KEY` |
| Google Gemini | `GEMINI_API_KEY` |
| Mistral | `MISTRAL_API_KEY` |
| Groq | `GROQ_API_KEY` |
| Together | `TOGETHER_API_KEY` |
| OpenRouter | `OPENROUTER_API_KEY` |

Without any persistent store at all (no `DATABASE_URL`, dev mode), the gateway never serves env keys either: callers forward their own provider key as the Bearer token (pass-through mode above).

Local providers (Ollama, vLLM, LM Studio) don't require keys.

### 2.4 Security: custom provider URLs & headers

When a caller supplies a custom provider `base_url` and/or `extra_headers` (pass-through / BYOK setups), the gateway validates them before dispatching any request, to prevent SSRF and header injection:

- **URL guard** — the `base_url` must be `https` (or `http` only when local providers are explicitly allowed). The gateway rejects hosts that resolve to loopback, private (`10/8`, `172.16/12`, `192.168/16`), link-local (`169.254/16`), unique-local IPv6 (`fc00::/7`), CGNAT (`100.64/10`), the `0.0.0.0/8` block, and cloud-metadata addresses (`169.254.169.254`, `100.100.100.200`), as well as `localhost`, `*.local`, and `metadata.google.internal`. IPv4-mapped IPv6 is unwrapped and re-checked. A best-effort DNS resolution rejects the URL if **any** resolved address is private.
- **Header filter** — `extra_headers` are stripped of any name that could override gateway-set auth/routing or inject hop-by-hop headers: `authorization`, `x-api-key`, `anthropic-version`, `content-type`, `host`, and the hop-by-hop set (`connection`, `proxy-authorization`, `transfer-encoding`, `upgrade`, `te`, `trailer`, `keep-alive`, `proxy-connection`).

**Limitation (operators):** the DNS check is defense-in-depth and is subject to a TOCTOU/DNS-rebinding race — a malicious resolver can return a safe address at validation time and a private one at connect time. Connect-time enforcement is out of scope for the gateway. Operators handling untrusted `base_url` values should additionally run the gateway behind a network policy that blocks outbound connections to RFC-1918 / metadata ranges.

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
  "max_completion_tokens": 1024,
  "stream": false,
  "stream_options": { "include_usage": true },
  "tools": [],
  "tool_choice": "auto",
  "parallel_tool_calls": true,
  "reasoning_effort": "high",
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

**Compat fidelity (field passthrough):**

Gateway preserves the full OpenAI request shape. In addition to the fields above
it models these newer OpenAI fields as first-class, forwarding them to the
routed provider where supported:

- `max_completion_tokens` — the reasoning-model spend cap (`o3`, `o4-mini`, …).
  Forwarded verbatim to OpenAI; mapped to the native output cap for Anthropic
  (`max_tokens`) and Gemini (`maxOutputTokens`). Takes precedence over
  `max_tokens` when both are set.
- `stream_options` — e.g. `{ "include_usage": true }`. Forwarded to
  OpenAI-shaped providers (the gateway always enables `include_usage` for its
  own accounting; any other keys you set are preserved).
- `parallel_tool_calls` — forwarded to OpenAI-shaped providers.
- `reasoning_effort` — `"low"`/`"medium"`/`"high"`; forwarded to OpenAI-shaped
  providers.

Any **genuinely-unknown or newer** OpenAI field not modeled above (e.g.
`logprobs`, `service_tier`, `prediction`) passes through to OpenAI-shaped
upstreams unchanged rather than being dropped. (TokenTrimmer-internal
`tt_extras` is the one field always stripped before forwarding.)

**Provider-specific parameter handling:**

- Parameters not supported by the routed provider are dropped, with a
  `X-TokenTrimmer-Warnings: param_dropped:<name>` response header noting the
  drop — e.g. `parallel_tool_calls`, `reasoning_effort`, and `stream_options`
  are reported dropped for Anthropic- and Gemini-routed requests.
- Parameters with different ranges across providers (e.g., temperature) are clamped to the provider's valid range, with a `temperature_clamped` warning (e.g. Anthropic caps `temperature` at 1.0).
- For Anthropic-routed requests, `max_tokens` is required by Anthropic but optional here; Gateway defaults to 4096 if omitted (or to `max_completion_tokens` when that is set).

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
- `usage.cache_read_input_tokens` / `usage.cache_creation_input_tokens` — raw
  provider-reported prompt-cache token counts (Anthropic cache fields, OpenAI
  `prompt_tokens_details.cached_tokens`, Gemini `cachedContentTokenCount`).
  Present **only when the provider reported the field** — omitted means
  "unreported", `0` means the provider explicitly reported zero. This is
  distinct from the always-present folded `cached_tokens`. The same additive
  fields appear on streamed usage chunks (final/`include_usage`); the
  `tokentrimmer.usage` cost frame keeps its exact 7-key shape unchanged.
  Two caveats: (1) on streams, if the upstream reported only a folded
  `cached_tokens > 0` without the raw field (an older TokenTrimmer hop or
  pre-fix adapter), the gateway reconstructs `cache_read_input_tokens` from
  that fold — the value still reflects provider-reported cache reads, never a
  fabrication; (2) TokenTrimmer L1/L2 cache hits replay the original miss's
  stored usage verbatim (like every other usage field), so on a hit these
  fields describe the provider call that produced the cached response — no
  provider call happened on the hit itself, and the telemetry ledger logs
  NULL for hit rows. Clients reconciling per-request provider cache reads
  from response bodies should exclude replayed responses
  (`x-tokentrimmer-cache: hit-l1` / `hit-l2`).
- `model` reflects the *actually used* model, which may differ from the requested model if a route rewrote it

### 3.4 Response (streaming)

When `stream: true`, the response is an SSE stream of `ChatCompletionChunk` events:

```
data: {"id":"chatcmpl-abc","object":"chat.completion.chunk","created":1716598234,"model":"claude-3-5-sonnet-20241022","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}

data: {"id":"chatcmpl-abc","object":"chat.completion.chunk","created":1716598234,"model":"claude-3-5-sonnet-20241022","choices":[{"index":0,"delta":{"content":"The "},"finish_reason":null}]}

... (more chunks) ...

data: {"id":"chatcmpl-abc","object":"chat.completion.chunk","created":1716598234,"model":"claude-3-5-sonnet-20241022","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":23,"completion_tokens":8,"total_tokens":31,"cached_tokens":0}}

event: tokentrimmer.usage
data: {"cost_usd":0.000123,"baseline_cost_usd":0.000456,"saved_usd":0.000333,"provider_cache_saved_usd":0.0,"cache_bust_usd":0.0,"input_tokens":23,"output_tokens":8,"cached_tokens":0}

data: [DONE]
```

Usage is included on the final content chunk (this differs from OpenAI's default; can be toggled with `stream_options: {"include_usage": false}` to suppress the folded usage block).

**`stream_options.include_usage: true` (OpenAI-native usage chunk).** When the
client explicitly sets `include_usage: true`, Gateway additionally emits an
OpenAI-native final usage chunk — a chunk with an **empty `choices` array** and a
populated `usage` block — immediately before the trailing frames, matching how
OpenAI streams usage. This is guaranteed end-to-end regardless of which provider
served the request (for OpenAI-shaped upstreams the provider's own usage chunk is
forwarded; for Anthropic/Gemini one is synthesized from the accumulated counts):

```
data: {"id":"chatcmpl-abc","object":"chat.completion.chunk","created":1716598234,"model":"claude-3-5-sonnet-20241022","choices":[],"usage":{"prompt_tokens":23,"completion_tokens":8,"total_tokens":31,"cached_tokens":0}}
```

**`event: tokentrimmer.usage` (cost frame).** On clean completion Gateway always
emits a non-OpenAI `tokentrimmer.usage` SSE frame carrying per-request cost,
baseline, and savings — so streaming clients can surface savings that response
headers cannot. Its shape is **stable** (TokenTrimmer SDKs parse it): exactly the
keys `cost_usd`, `baseline_cost_usd`, `saved_usd`, `provider_cache_saved_usd`,
`cache_bust_usd`, `input_tokens`, `output_tokens`, `cached_tokens`.
(`cache_bust_usd` is the explicit negative-savings entry for a deliberate
stable-prefix mutation, already subtracted from `saved_usd` pre-clamp —
`0.0` on every request whose cache-stable prefix was untouched.) The `include_usage` chunk (when
requested) is emitted *before* this frame; this frame does not replace it.

**Unknown / newer provider chunk fields** (e.g. `system_fingerprint`,
per-choice `logprobs`, per-delta `refusal`) are preserved on streaming chunks and
round-tripped to the client unchanged rather than being dropped.

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

### 3.8 Anthropic Messages ingress (`POST /v1/messages`)

For Anthropic-wire clients (Claude Code, the Anthropic SDKs), Gateway also accepts the native Anthropic Messages API request shape at `POST /v1/messages` and returns the Anthropic Messages response shape — `{ "type": "message", "role": "assistant", "content": [...], "stop_reason": ..., "usage": {...} }`. With `"stream": true` it returns Anthropic typed SSE event frames (`message_start`, `content_block_start`, `content_block_delta`, `content_block_stop`, `message_delta`, `message_stop`).

Tool use is supported in both directions. Non-streaming responses carry `tool_use` content blocks; streaming responses emit a `tool_use` content block per tool call (`content_block_start` → `input_json_delta` deltas → `content_block_stop`) and a `stop_reason: "tool_use"` in the closing `message_delta`, so Claude Code's agentic loop receives runnable tools.

```
POST /v1/messages
```

```json
{
  "model": "claude-sonnet-4-6",
  "max_tokens": 1024,
  "system": "You are a helpful assistant.",
  "messages": [{ "role": "user", "content": "Hello" }]
}
```

The request is translated to the canonical chat shape and runs through the **same** cost, routing, cache, and credential pipeline as `/v1/chat/completions` — including the same `X-TokenTrimmer-*` response headers (§6.2) and the same BYO credential requirement (a verified org without a stored `anthropic` credential gets `400 missing_provider_credential`). Authenticate exactly as for chat (§2): a TokenTrimmer key in `Authorization: Bearer …`.

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

### 5.2 Gateway runtime capabilities

```
GET /v1/capabilities
Authorization: Bearer tt_live_...
```

Returns a versioned, `Cache-Control: private, no-store` snapshot of facts known
by the **responding gateway process** for a verified live key. Anonymous,
sandbox (`tt_test_*`), dogfood, and other non-live identities receive the
normal `401` response. The document never includes an organization ID, key ID,
provider credential, provider configuration, or secret.

The first version describes only Fusion facts that the gateway itself enforces:
its kill switch, the effective caller tier, its configured minimum tier, and
the maximum number of member models. This selected-fields excerpt omits the
repeated `reason.message`, applicable `source`, and `schema_versions` metadata;
the cap value is only an example, since the response carries the process's
configured value:

```json
{
  "schema_version": 1,
  "scope": "gateway_runtime",
  "snapshot_scope": "responding_process",
  "generated_at": "2026-07-16T12:00:00.000Z",
  "features": {
    "fusion": {
      "enabled": {
        "state": "enabled",
        "source": "gateway_runtime",
        "reason": { "code": "fusion_kill_switch_enabled" }
      },
      "access": {
        "state": "available",
        "reason": { "code": "fusion_gateway_gate_passed" }
      },
      "current_tier": { "state": "known", "value": "pro" },
      "minimum_tier": { "state": "known", "value": "pro" },
      "limits": {
        "member_models_max": {
          "value": 8,
          "enforcement": "gateway_runtime",
          "reason": { "code": "fusion_member_cap" }
        }
      }
    }
  },
  "provider_credentials": { "state": "unknown", "source": "not_negotiated" },
  "provider_health": { "state": "unknown", "source": "not_negotiated" },
  "model_support": { "state": "unknown", "source": "not_negotiated" },
  "modality_support": { "state": "unknown", "source": "not_negotiated" }
}
```

`features.fusion.access: "available"` means only that the responding process
has passed its Fusion kill-switch and tier gate for this key. It is neither a
reservation nor an execution guarantee: request-time credential, model,
budget, policy, and upstream-provider checks remain authoritative. Conversely,
`"unavailable"` includes a machine-readable reason such as
`fusion_disabled` or `fusion_tier_below_minimum`.

The provider, model, and modality fields are intentionally `unknown`; this
endpoint does not decrypt or count credentials, probe providers, or negotiate a
request. In particular, catalog data from `/v1/models` is metadata, not proof
that a credentialed request will be accepted. `snapshot_scope` also means a
load-balanced fleet may return different snapshots while configuration or a
binary rollout is in progress. Clients must execute the real request and handle
its result rather than using this endpoint as a fleet-wide readiness signal.

For these intentionally unknown facts, version 1 emits the following stable
`reason.code` values:

| Field | `reason.code` | Meaning |
| --- | --- | --- |
| `provider_credentials` | `provider_credentials_not_inspected` | The endpoint did not decrypt, count, or probe credentials. It says nothing about whether a credential is absent, present, valid, or usable. |
| `provider_health` | `provider_health_not_probed` | The endpoint did not make a provider health probe or spend-producing test request. It says nothing about provider health. |
| `model_support` | `model_support_not_negotiated` | The endpoint did not negotiate this request's model. Catalog metadata is not request acceptance evidence. |
| `modality_support` | `modality_support_not_negotiated` | The endpoint did not negotiate this request's modality. It is not modality-support or request-acceptance evidence. |

For every row, `state: "unknown"` and `source: "not_negotiated"` are
normative: the code describes an omitted observation, never a positive or
negative readiness result. Clients MUST use the actual request result for any
credential, provider, model, modality, limit, or execution decision.

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
| `X-TokenTrimmer-Interactive` | Declares a human is waiting on this request (send `1` or `true`). Hard-clears the advisory batch-eligibility route action (`then.batch`) — the gateway never marks interactive traffic batch-eligible (`batch_ineligible:interactive` warning). Parsing fails interactive-safe: **any** non-empty value other than an explicit `0`/`false` opt-out is treated as interactive, so an unrecognized spelling (`yes`, `on`, …) can never be silently batch-marked. Set automatically by `tt chat` and the `/tools` loop. | Honored | `1` |
| `traceparent` | Standard [W3C TraceContext](https://www.w3.org/TR/trace-context/) header. When present and valid, the gateway **continues your trace**: its request span becomes a child of your inbound span (same `trace_id`), so gateway cost/latency appears on your existing distributed trace. An accompanying `tracestate` is preserved. Absent/malformed → a fresh root trace. | Honored | `00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01` |

### 6.2 Response headers

| Header | Present | Example |
|---|---|---|
| `X-TokenTrimmer-Trace-Id` | every response | `5f3a1c...` |
| `X-TokenTrimmer-Latency-Ms` | every response | `412` |
| `X-TokenTrimmer-Provider` | on dispatched/cached responses | `anthropic` |
| `X-TokenTrimmer-Model-Used` | on dispatched/cached responses | `claude-3-5-haiku-20241022` |
| `X-TokenTrimmer-Cache` | on dispatched/cached responses | `hit-l1` / `hit-l2` / `neg-hit` / `miss` / `none` / `sandbox` |
| `X-TokenTrimmer-Cost-Usd` | on dispatched/cached responses | `0.0034` |
| `X-TokenTrimmer-Baseline-Cost-Usd` | on dispatched/cached responses | `0.0218` |
| `X-TokenTrimmer-Saved-Usd` | on dispatched/cached responses | `0.0184` |
| `X-TokenTrimmer-Provider-Cache-Saved-Usd` | on dispatched/cached responses | `0.0009` |
| `X-TokenTrimmer-Batch-Forgone-Usd` | on dispatched/cached responses — the **forgone** Batch-API discount for batch-eligible requests (advisory `then.batch` route action), priced from the served model's real catalog batch rate. The manual async Batch Lane does not change this synchronous request: it was dispatched and billed in full, so this figure is **never** included in `X-TokenTrimmer-Saved-Usd`. `0.000000` for all unmarked traffic. | `0.0125` |
| `X-TokenTrimmer-Minify-Saved-Est-Usd` | on dispatched/cached responses — the **ESTIMATED** saving from minified-JSON output steering (`then.minify_json` route action): the emitted JSON re-rendered pretty and re-tokenized with the served model's tokenizer, minus the tokens actually emitted, priced at the billed output rate. An estimate of an unmeasurable counterfactual: **never** included in `X-TokenTrimmer-Saved-Usd`. `0.000000` for un-minified traffic, non-JSON responses, and streaming (v1 meters but does not estimate). | `0.000312` |
| `X-TokenTrimmer-Diff-Saved-Usd` | on dispatched/cached responses — the **measured** saving of an applied `then.diff` patch: tokens of the reconstructed artifact minus the billed patch tokens, priced at the served model's output rate. Both sides are real tokenizer counts on real strings, so this figure **is** included in `X-TokenTrimmer-Saved-Usd` (via the baseline fold) and is itemized here for the methodology breakdown. `0.000000` for all undiffed traffic. | `0.0297` |
| `X-TokenTrimmer-Format-Switch-Saved-Est-Usd` | on dispatched/cached responses — the **estimated** saving of a validated `then.format_switch` emission: a JSON-equivalent reconstruction minus the emitted body, output-rate-priced. The `Est` in the name is the label: an estimate, **never** included in `X-TokenTrimmer-Saved-Usd` / `Baseline-Cost-Usd` (which are catalog-priced gateway figures, not provider-invoice reconciliation). `0.000000` for unswitched traffic, whenever the reconstruction is not computable (booked $0), and on cache **hits** of a switched response (the hit still advertises `format_switch:<label>`, but its saving is attributed to the cache — the estimate belongs to the original miss's row, mirroring the diff/compression figures). | `0.0041` |
| `X-TokenTrimmer-Diff-Failed-Cost-Usd` | on dispatched/cached responses — the realized cost of a FAILED `then.diff` patch attempt on a fail-closed double dispatch. Already **folded into** `X-TokenTrimmer-Cost-Usd` (the retry is real served usage for this trace; the header remains a catalog-priced gateway figure rather than provider-invoice reconciliation); duplicated here so the retry tax can be unpicked. `0.000000` unless a patch failed on this trace. | `0.0106` |
| `X-TokenTrimmer-Route-Matched` | the applied route's name, on routed responses (forced or condition-matched) | `cheap-for-short` |
| `X-TokenTrimmer-Warnings` | on dispatched responses when the gateway altered the request, and on cache-hit responses carrying pre-dispatch tokens | `param_dropped:frequency_penalty,param_dropped:n` |
| `X-TokenTrimmer-Captured` | present **only** when this trace's body was persisted to the per-org **opt-in encrypted body-capture** sink — i.e. the hosted feature is armed AND your org enabled capture in its settings (the gateway checks the per-org opt-in before stamping the header, so it never appears for orgs that have not opted in). Always `true` when present; **absent** on every default-path response (capture off by default) and on cache-hit replays (no live body to capture). Non-streaming traces persist request **and** response; streaming traces persist the **request** body only. See [§19 — does Gateway log my prompts?](#19-faq) and the [hosted privacy spec](tokentrimmer-architecture-spec-v1.md#161-customer-data). | `true` |

`X-TokenTrimmer-Trace-Id` and `X-TokenTrimmer-Latency-Ms` are present on every
response (success or error). The cost/provider/model/cache headers are attached on
responses that reach dispatch, cache, or the sandbox path; they are not emitted on
early validation errors (4xx returned before dispatch).

Savings attribution: `X-TokenTrimmer-Saved-Usd` contains only savings *caused by
TokenTrimmer* (routing to a cheaper model, TokenTrimmer L1/L2 cache hits,
failover choices). Discounts the provider applies automatically to its own bill
— prompt-cache read discounts (e.g. OpenAI cached input tokens, Anthropic
`cache_read_input_tokens`), net of any cache-write premium — are reported
separately on `X-TokenTrimmer-Provider-Cache-Saved-Usd` and never inflate the
TokenTrimmer figure. `Saved-Usd` and `Cost-Usd` are observed gateway usage
priced from the active catalog; they are not provider-invoice reconciliation,
which requires a separate period-level import and comparison.

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
Anthropic, whose max is `1.0`). A `route_paused:<route-name>` token is emitted
when the matched route is **paused** (manually or by the quality auto-pause —
see §10.7): the request was served on its originally-requested model with the
route's rewrite and every other cost lever suppressed; the matching
`request_logs` row carries `route_paused = true`. Pre-dispatch tokens
(`route_paused:*`, `redacted:*`, the output-shaping tokens below) are attached
on L1/L2 cache-hit responses as well as dispatched ones; `param_dropped:*`
tokens appear on dispatched responses only (no dispatch happened on a hit).
The embeddings endpoint has no warnings header — a paused embeddings
passthrough is visible only in the `route_paused_passthrough_total` metric
and the gateway log line.

The contract-changing output-shaping route actions (`then.format_switch` /
`then.diff` — see the routing rules guide) emit their own token families.
Format switch: `format_switch:<csv|bare>` advertises every response served in
the switched format (dispatched AND cache hits — only validated switched
bodies are ever cached under a switched key); `format_switch_failed:<label>`
marks a fail-open passthrough (the model did not comply, or the emission was
cut short by the provider — a `finish_reason` other than `"stop"` is never
accepted as a switched body, since a token-limit cut on a record boundary
would otherwise pass shape validation while silently dropping records; the
untouched body was served and nothing was booked);
`format_switch_skipped:<reason>` names the
eligibility gate that no-op'd the switch (`unknown_format`, `streaming`,
`tools`, `n`, `no_schema`, `strict_schema`, `nested_schema`,
`not_single_value`, `conflict`). Diff: `diff_applied` marks a reconstructed
full artifact served from a billed patch; `diff_failed:<reason>` marks a
fail-closed full re-emit (`truncated` — the patch emission carried a
non-`"stop"` finish_reason, e.g. a token-limit cut that can parse as a
valid-but-incomplete patch — `no_blocks`, `malformed_patch`,
`anchor_missing`, `anchor_ambiguous`, `invalid_json`); `diff_degraded` marks
the last-resort raw-patch passthrough when the re-emit dispatch itself
errored; `diff_skipped:<reason>` names the no-op gate (`no_prior`,
`prior_too_small`, `bad_prior_type` — a present `tt_extras.diff_prior` that
is not a JSON string skips the lever rather than silently falling back to
the assistant history — `streaming`, `tools`, `n`, `strict_schema`). On a
canary route (`then.traffic_pct`) the CONTROL arm is served fully unchanged:
neither shaping lever runs there (no `format_switch_*` / `diff_*` tokens at
all), so the canary comparison never mixes shaped and unshaped responses.

The advisory batch-eligibility route action (`then.batch`) emits its own
tokens — honest by design, since the gateway dispatches synchronously today and
the action never changes the bill:

- `batch_deferred_unavailable` — the marker was applied: the row is tagged
  batch-eligible and the forgone discount is reported on
  `X-TokenTrimmer-Batch-Forgone-Usd`, but automatic conversion of a synchronous
  request into a Batch job is not implemented. The manual Batch Lane is a
  separate API, so this request was served and billed normally. Corner case:
  eligibility is assessed on the *planned* model, while the forgone discount
  is priced only from the *served* model's real catalog batch rate — after a
  failover to a model with no batch tier, this token can therefore accompany a
  `0.000000` forgone figure (intent stays auditable; no rate, no claim).
- `batch_ineligible:streaming` — the matched route requested the marker but the
  request streams; the gateway cleared it (a ≤24h batch window cannot serve a
  live stream).
- `batch_ineligible:interactive` — the marker was cleared because the client
  declared `X-TokenTrimmer-Interactive` (a human is waiting).
- `batch_not_available:<model>` — the served model carries no catalog batch
  tier (possible after failover); nothing is marked and no discount is claimed.

The output-shaping route actions (`then.minify_json`,
`then.reasoning_max_effort` / `then.reasoning_budget_tokens`) emit a token on
every act **and** every refusal:

- `output_minified` — the deterministic minify instruction was appended to the
  system prompt; the per-response estimate (when the response parses as JSON)
  rides `X-TokenTrimmer-Minify-Saved-Est-Usd`.
- `minify_skipped:structured_output` — the request's `response_format:
  json_schema` is honored natively by the served provider (grammar-locked
  structured output already controls whitespace): no instruction, no claim.
- `reasoning_capped:reasoning_effort:<cap>` / `reasoning_capped:thinking_budget:<cap>`
  — the cap was applied (lower-only). Books `$0`: the event is metered and the
  route-level netted savings report carries the truth over the window.
- `reasoning_cap_skipped:class:<math|code|legal|medical>` — the HARD class
  gate refused: this request's content is reasoning-is-the-work and is never
  capped.
- `reasoning_cap_skipped:unknown_effort:<value>` — the request carried a
  `reasoning_effort` the gateway cannot rank; nothing was rewritten.
- `reasoning_cap_skipped:not_reasoning:<model>` — no `reasoning_effort` on the
  request and the served model is not catalog-Reasoning-capable; the gateway
  never injects `reasoning_effort` into a model that may reject it.
- `reasoning_cap_skipped:unsupported:<provider>` — a cap is configured but the
  served surface carries no lever (the provider drops `reasoning_effort` and
  the request has no enabled `thinking` config).

Both actions are judge-gateable: an output-shaped request is eligible for the
sampled paired judge even when the route's `target_model` equals the requested
model (no price downgrade), because the pre-routing capture re-dispatches the
**un-shaped** request as the baseline reference. With `auto_pause: true`, a
shaped route whose paired pass-rate regresses below its floor sticky-pauses
itself, and a paused route suppresses both actions (§10.7).

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

When the upstream provider returns an error, Gateway preserves the upstream message in `error.message`. The error envelope is the flat OpenAI-compatible shape — `message`, `type`, `code`, and optional `param`:

```json
{
  "error": {
    "message": "Anthropic API: max_tokens cannot exceed 8192 for this model",
    "type": "upstream_invalid_request",
    "code": "anthropic_max_tokens_exceeded",
    "param": "max_tokens"
  }
}
```

The request's trace id is returned on the **`X-TokenTrimmer-Trace-Id`** response header (not in the body).

> **Planned (not yet honored):** an enriched `error.tokentrimmer` object carrying `provider`, `upstream_status`, `fallback_attempted`, and `trace_id` is on the roadmap but is **not emitted today** — do not depend on it. The body currently contains only the four fields above.

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

> **Planned (not yet honored):** customer event-webhook delivery is **not yet implemented** in the gateway. The event types, payload shape, and signing scheme below describe the intended design — do not build against them yet. (The only webhooks the gateway processes today are internal Stripe tier-change events, which are unrelated to this customer-facing delivery system.)

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

## 10. Configuration API

There are **two distinct surfaces**, depending on deployment:

| Surface | Base path | Methods | Where |
|---------|-----------|---------|-------|
| **Hosted (cloud) admin API** | `/v1/admin/*` | GET/POST/PATCH/DELETE + plans/inspect/usage/invoices | TokenTrimmer Cloud only |
| **Self-hosted gateway routes API** | `/v1/routes` | GET, POST, DELETE (no PATCH) | the open-source gateway binary |

Sections 10.1–10.6 document the **hosted admin API**. Self-hosted operators use the routes API in §10.7.

### 10.1 List routes (hosted)

```
GET /v1/admin/routes
Authorization: Bearer tt_live_*
```

### 10.2 Create route (hosted)

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

### 10.6 Usage and billing (hosted)

```
GET /v1/admin/usage?from=2026-05-01&to=2026-05-31
→ Aggregated usage for the period

GET /v1/admin/invoices
→ List of Stripe invoices
```

### 10.7 Self-hosted gateway routes API

The open-source gateway binary serves a routes API at `/v1/routes` (note: **no `/admin/` prefix**). It supports list, create, get, delete, pause/resume, and a per-route savings report — there is **no PATCH/update** handler; to change a route, delete and re-create it.

```
GET    /v1/routes              → list all routes
POST   /v1/routes              → create a route (body identical to §10.2)
GET    /v1/routes/:id          → fetch one route
DELETE /v1/routes/:id?expected_revision=N
                               → delete only the management-read revision N
POST   /v1/routes/:id/pause?expected_revision=N
                               → sticky-pause only management-read revision N
POST   /v1/routes/:id/resume?expected_revision=N
                               → clear a pause only on management-read revision N
GET    /v1/routes/:id/savings  → windowed netted savings (see below)
```

#### Route-triggered workflow releases

A route can opt into automatic execution of a current workflow release by
setting the closed `then.workflow.environment` selector. Omit `environment` to
preserve the legacy behavior of executing the latest saved definition.

```json
{
  "name": "production-support-flow",
  "when": { "models": ["gpt-4o"] },
  "then": {
    "target_model": null,
    "workflow": {
      "workflow_id": "550e8400-e29b-41d4-a716-446655440000",
      "environment": "production",
      "mode": "detour",
      "max_cost_usd": 0.05
    }
  }
}
```

`environment` accepts only `development`, `staging`, or `production`. For each
matched request, the gateway resolves the selected environment before provider
work, executes its exact immutable workflow version with its exact current
non-secret variable snapshot, and records workflow version, release revision,
and variables revision on the durable workflow run. A missing release, missing
required variable, inconsistent retained release, or unavailable run-provenance
store fails the detour before spend. Shadow mode skips the workflow and surfaces
a warning while the direct request continues. Advancing the environment affects
only later matched requests; an already accepted run keeps its recorded tuple.

#### Activation evidence and legacy-row recovery

`GET /v1/routes` and `GET /v1/routes/:id` are **management reads**. They
include disabled routes and expose a persisted legacy/manual row even when the
runtime refuses to execute it. This makes a bad row repairable or deletable
instead of silently disappearing from the route list.

Each returned route keeps the familiar flattened route fields (`id`, `name`,
`priority`, `enabled`, `when`, `then`, and optional `paused`) and adds:

```json
{
  "revision": 42,
  "schema_version": 1,
  "canonical_hash": "sha256:…",
  "activation": { "state": "active" }
}
```

`revision` is an opaque positive generation token. Every stateful single-route
operation (`DELETE`, `POST …/pause`, and `POST …/resume`) requires the exact
value from a recent management read as `expected_revision`; a missing,
malformed, or stale value makes no change (`400` for an invalid/missing token,
`409` for a newer current row). The CLI re-reads this value before `tt route rm`;
API clients must do the same rather than retrying a revision-less delete, pause,
or resume.

`activation.state` is `active` for a canonical enabled definition,
`disabled` for a canonical disabled definition, or `invalid` for a stored
definition the gateway will not execute. An invalid row has
`"canonical_hash": null` and field-addressed `activation.issues`; its raw
`when`/`then` JSON is returned unchanged for diagnosis. `active` describes
canonical enabled eligibility, not a bypass of a sticky pause: when
`"paused": true`, the route still matches for attribution but its cost action
is suppressed as described below.

#### Pause / resume

A **paused** route still *matches* — requests attribute to it
(`X-TokenTrimmer-Route-Matched`, the `route_paused:<name>` warnings token, and
`request_logs.route_paused = true`) — but its rewrite and every other **cost**
lever (`fallbacks`, `flex`, `compress`, `traffic_pct`, `shadow_model`,
`max_cost_usd`) are suppressed, so requests flow to their originally-requested
model: the **expensive, quality-safe** direction. **Safety** levers (`redact`,
`disable_cache`) stay live — pausing a quality gate never disables a privacy
guardrail. A forced `X-TokenTrimmer-Route` header does **not** bypass a pause.

Pauses are **sticky**: created manually (`POST /v1/routes/:id/pause?expected_revision=N`) or by the
opt-in quality auto-pause (`then.auto_pause` — see the
[routing rules guide](routing-rules-guide.md)), they persist until an explicit
`POST /v1/routes/:id/resume?expected_revision=N`. A resume retains the pause record with a
`resumed_at` watermark: the auto-pause evaluator only counts verdicts recorded
**after** the most recent resume, so a just-resumed route is re-evaluated on
fresh evidence, never instantly re-paused by its frozen pre-pause window.
Pause/resume takes effect immediately on the replica that served the call and
within the 60-second route-cache TTL on other replicas. `GET /v1/routes` /
`GET /v1/routes/:id` surface `"paused": true` on paused routes (the key is
omitted when false). Both endpoints are idempotent and answer `200`; `resume`
reports `"was_paused"` so callers can tell whether an active pause was
actually cleared.

Two caveats worth knowing: deleting a route deletes its pause record with it,
so the documented delete-and-re-create edit flow starts the new route (a fresh
id) **unpaused** — re-pause it explicitly if the quality concern still stands.
And `paused` on the API is a bare flag: the recorded evidence (`paused_by`,
`reason`, `pass_rate`, `paused_at`) lives in the `route_pauses` table and is
not yet surfaced on a read endpoint (dashboard surfacing is tracked in the
cloud repo).

#### Per-route netted savings

`GET /v1/routes/:id/savings?hours=N` (default `720` = 30 days, clamped to
`1..=2160`) reports the route's savings over the window with the
**measurement tax netted and itemized** — every tax line is its own field,
never silently subtracted:

```json
{
  "route_id": "0190…",
  "window_start": "2026-05-12T00:00:00Z",
  "window_end": "2026-06-11T00:00:00Z",
  "paused": false,
  "requests": 1842,
  "gross_saved_usd": 12.41,      // legacy positive-only compatibility figure
  "judge_tax_usd": 0.83,         // paired-judge calls + baseline reference dispatches
  "shadow_tax_usd": 0.22,        // discarded shadow-arm spend
  "cache_bust_usd": 0.07,        // itemized; already included in legacy/signed rows
  "net_saved_usd": 11.36,        // legacy gross − judge_tax − shadow_tax; MAY BE NEGATIVE
  "positive_estimated_savings_usd": 12.06,
  "estimated_regressions_usd": 0.16,
  "summarizer_tax_usd": 0.04,    // itemized; already included in signed row values
  "net_estimated_savings_usd": 10.85, // positive − regressions − judge_tax − shadow_tax
  "unmetered_tax_rows": 3,       // rows whose tax is unmetered (NULL cost) —
                                 // when > 0 the taxes are lower bounds and the
                                 // net is an upper bound
  "verdicts": { "judged": 41, "acceptable": 38, "degraded": 2,
                "unclear": 1, "pass_rate": 0.95 }
}
```

Both net fields are deliberately **not clamped at zero**. The additive
`net_estimated_savings_usd` is the recommended route-level estimate: it keeps a
request that cost more than its counterfactual visible as a regression instead
of losing it to a per-row positive floor. Per-request figures
(`X-TokenTrimmer-Saved-Usd`, `request_logs`) do not carry the amortized
judge/shadow measurement taxes; that netting exists only at this aggregate
surface. The shipped gateway binary wires the Postgres savings source
automatically at boot when `DATABASE_URL` is set; without a database the
endpoint answers `503` (aggregation not configured). An existing route with no
in-window traffic answers an honest all-zero body, not `404`.

Two semantics to read the numbers with:

- `gross_saved_usd` and `net_saved_usd` are retained legacy compatibility
  fields. Their row contribution is `max(baseline_cost_usd - cost_usd -
  provider_cache_saved_usd - cache_bust_penalty_usd, 0)`, so a costly request
  is hidden before the route aggregate is formed. They also predate the
  durable `summarizer_tax_usd` field. They remain useful for existing clients,
  but are not the complete row-level estimate. A paused route can still accrue
  cache-hit savings while its rewrite is suppressed; provider-invoice
  reconciliation remains a separate, period-level process.
- The signed fields use each route-attributed request's complete delta:
  `baseline_cost_usd - cost_usd - provider_cache_saved_usd -
  cache_bust_penalty_usd - summarizer_tax_usd`. The API reports the sum of
  its positive half as `positive_estimated_savings_usd`, the absolute
  magnitude of its negative half as `estimated_regressions_usd`, and
  `net_estimated_savings_usd = positive_estimated_savings_usd -
  estimated_regressions_usd - judge_tax_usd - shadow_tax_usd`. Cache-bust and
  summarizer taxes are already inside the signed row values and are itemized
  only for auditability—do **not** subtract either again. This is a
  catalog-priced gateway estimate, not provider-invoice reconciliation.
- `unmetered_tax_rows` identifies only NULL judge/shadow measurement costs.
  `summarizer_tax_usd` is the sum of durably recorded row values; the gateway
  does not yet persist a separate marker for an auxiliary summarizer cost that
  failed to meter, so that unknown amount cannot be inferred from this response.
- The window buckets `request_logs` rows and `quality_verdicts` rows
  independently by their own timestamps, and a verdict is written by the
  detached judge task seconds-to-minutes after its request — so a request near
  `window_end` can land in this window while its judge tax lands in the next
  one. Self-correcting across consecutive windows; negligible at the default
  720 h window, visible at very short windows (`hours=1`).

> **Planned (not yet honored):** in-place `PATCH /v1/routes/:id` update, and the hosted-only `/v1/admin/plans|inspect|usage|invoices` surfaces, are not served by the self-hosted binary.

---

## 11. SDKs

TokenTrimmer ships official SDKs. The Python and TypeScript SDKs are thin
subclasses of the official OpenAI SDK (default `base_url` → TokenTrimmer, plus
`tt_*` convenience params and a parsed `.tt` cost accessor); the Rust
`tokentrimmer-client` crate is a standalone typed client. All three resolve the
API key in the same order: an explicit constructor argument wins, then the
`TOKENTRIMMER_API_KEY` environment variable (the Python/TypeScript SDKs then
fall back to the OpenAI SDK's own `OPENAI_API_KEY`).

### 11.1 Python

```python
from tokentrimmer import TokenTrimmer

client = TokenTrimmer(api_key="tt_live_...")  # or set TOKENTRIMMER_API_KEY

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

const client = new TokenTrimmer({ apiKey: 'tt_live_...' }); // or set TOKENTRIMMER_API_KEY

const response = await client.chat.completions.create({
  model: 'claude-3-5-sonnet',
  messages: [{ role: 'user', content: 'Hello' }],
  ttTag: 'feature=onboarding',
});

console.log(response.tt.costUsd, response.tt.savedUsd, response.tt.cache);
```

The Python and TypeScript SDKs are thin — the underlying request goes through the OpenAI SDK, with `base_url` set to TokenTrimmer. Customers can also use the OpenAI SDK directly (point its `base_url` at the gateway) without the TokenTrimmer wrapper.

### 11.3 Rust

The `tokentrimmer-client` crate (`tt-client`) is a standalone typed client (it does not wrap another SDK). `Client::new` reads `TOKENTRIMMER_API_KEY` when you pass an empty key.

```rust
use tt_client::{user, Client};

// Pass an empty key to read TOKENTRIMMER_API_KEY from the environment.
let client = Client::new("https://api.tokentrimmer.com", "");

let outcome = client
    .chat()
    .model("claude-3-5-sonnet")
    .message(user("Hello"))
    .tag("feature=onboarding") // X-TokenTrimmer-Tag
    .send()
    .await?;

println!("{:?}", outcome.text());
// Cost/savings parsed from the x-tokentrimmer-* response headers.
println!("{:?} {:?}", outcome.cost.cost_usd, outcome.cost.saved_usd);
```

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

Gateway versions follow semver.

> **Planned (not yet honored):** per-request version pinning via an `X-TokenTrimmer-Version` header is **not yet implemented** — the gateway does not read this header today. The form below describes the intended design.

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

- No `/v1/admin/*` admin API (that surface is hosted/cloud only). The self-hosted binary **does** serve a routes API at `/v1/routes` (GET/POST/DELETE — see §10.7); routes can also be seeded from YAML config.
- No customer event webhooks (Planned — see §9)
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
| `cache_lookups_total` | counter | `tier` (`l1`/`l2`), `result` (`hit`/`miss`; `verify_reject` on `l2` when the verify gate rejects an ambiguous-band hit) |
| `cache_l2_verify_total` | counter | `result` (`confident`/`verified`/`unverifiable`/`rejected`) — emitted per L2 hit only when the verify gate (`TT_L2_VERIFY`) is enabled |
| `cache_l2_threshold_raised_total` | counter | `class` — adaptive FP-gate ratchet raised a per-class L2 threshold |
| `cache_l2_judge_capped_total` | counter | — (an L2-hit judge sample was skipped by the hourly spend cap, `TT_JUDGE_L2_HIT_MAX_PER_HOUR`) |
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
Tokens used × per-model pricing from our curated pricing table (a snapshot embedded at build time and refreshed on each release, not auto-refreshed). `X-TokenTrimmer-Cost-Usd` and `X-TokenTrimmer-Saved-Usd` are catalog-priced gateway estimates based on observed usage, not provider-invoice reconciliation.

**Q: Does Gateway log my prompts?**
No, not by default. By default the gateway records only request **metadata** (token counts, model, route, latency) — never prompt or response bodies. There is one opt-in exception: hosted orgs can enable **encrypted body capture** (per org, off unless you turn it on) so the `/logs` replay tooling can show real request/response bodies. When enabled, the gateway:

- applies your route's **redaction** pass (`then.redact`) *before* anything is stored, so masked secrets never reach the capture table;
- **encrypts** the request and response bodies with XChaCha20-Poly1305 under a per-org key derived from the operator's `TT_MASTER_KEY` — the plaintext never lands in Postgres;
- caps each stored body at a configurable size (`TT_BODY_CAPTURE_MAX_BYTES`, default 256 KiB): over-cap bodies are truncated and an explicit `[tt-body-capture:truncated original_bytes=…]` marker is appended inside the encrypted payload, while the original byte length is still recorded honestly;
- enforces **retention** (1–30 days, per-org setting; default 7) after which rows expire;
- stamps `X-TokenTrimmer-Captured: true` only on responses whose body was actually persisted — the gateway checks your org's per-org opt-in before stamping, so the header reflects a stored body, never merely an armed deployment. It is **absent** on every default-path response and for orgs that have not opted in. (Streaming traces persist the request body only; non-streaming traces persist request and response.)

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

## 21. Fusion (multi-model panel)

Fusion is TokenTrimmer's opt-in multi-model panel: it fans one request out to N member models concurrently, then passes their responses to an arbiter model that synthesizes or selects an answer. It is not a web-search, source-capture, or citation-backed Deep Research workflow. The panel is **off by default** — a request without the trigger header is handled identically to any other request, with zero overhead.

### 21.1 Enabling and trigger

The panel is **kill-switched off** at the process level. `TT_PANEL_ENABLED` must be set to `1` (or `true`) for any panel request to be accepted. Until then, a request with the panel header receives `403 panel disabled`.

A second gate is **entitlement**: `TT_PANEL_MIN_TIER` (default `Free` = allow-all) is the minimum caller tier (`Free` < `Pro` < `Team` < `Scale`) allowed to use the panel. A panel request from a caller below the configured minimum is rejected — **before any dispatch or billing** — with `403`:

```json
{ "error": { "type": "permission_error", "code": "operation_not_permitted", "message": "panel: requires Pro tier or higher" } }
```

Both gates are evaluated in order **kill-switch → entitlement → budget**, all before the panel runs.

Once enabled, trigger the panel on **any of the three ingresses** by adding:

```http
X-TokenTrimmer-Panel: synthesize
```

Accepted values (case-insensitive):

| Value | Strategy |
|---|---|
| `synthesize` | Arbiter model synthesizes a new answer from all legs |
| `best-of-n` | Arbiter model judges and picks the single best leg verbatim |
| `majority` | Embedding clustering picks the medoid of the largest cluster (no arbiter LLM call) |

An unknown or absent value is silently treated as "no panel" — it never errors.

### 21.2 Per-request configuration (`tt_extras.panel`)

On `/v1/chat/completions` and `/v1/responses`, you can fine-tune the panel by including a `tt_extras.panel` object in the request body. All fields are optional:

```json
{
  "model": "gpt-4o",
  "messages": [{ "role": "user", "content": "..." }],
  "tt_extras": {
    "panel": {
      "members": ["gpt-4o", "claude-3-5-sonnet", "google/gemini-1.5-pro"],
      "arbiter_model": "gpt-4o",
      "quorum": 2,
      "max_cost_usd": 0.10
    }
  }
}
```

| Field | Type | Default | Description |
|---|---|---|---|
| `members` | string[] | gateway default (`TT_PANEL_DEFAULT_MEMBERS`) | Model ids to fan out to. Overrides the gateway default entirely when non-empty. |
| `arbiter_model` | string | gateway default (`TT_PANEL_DEFAULT_ARBITER`) | Model used for arbitration (Synthesize/BestOfN). |
| `quorum` | integer | see §21.3 | Minimum legs that must succeed before arbitration. |
| `max_cost_usd` | float | none | Per-request preflight budget estimate (USD); see §21.5. |

`/v1/messages` (Anthropic wire) accepts the `X-TokenTrimmer-Panel` header but does **not** accept `tt_extras.panel` — header-only trigger on that ingress.

### 21.3 Behavior

**Fan-out:** all member legs are dispatched concurrently (non-streaming, regardless of the caller's `stream` flag on the legs). Each leg targets the assigned member model and provider.

**Quorum:** the minimum number of legs that must succeed before arbitration. Default (when not set in `tt_extras.panel.quorum`):
- `majority` strategy: `floor(members / 2) + 1`
- all other strategies: `1`

If fewer legs succeed than the quorum threshold, the panel fails with `502` and no billing row is written.

**Arbitration:**
- `synthesize` — the arbiter model is called with all surviving leg answers and asked to synthesize one best answer. When `stream: true`, the arbiter call streams back to the caller.
- `best-of-n` — the arbiter model is called as a judge to pick the single best leg by number; that leg's original response is returned verbatim (no paraphrase). When `stream: true`, the chosen leg is fake-streamed.
- `majority` — surviving answers are embedded and clustered by cosine similarity (threshold `TT_PANEL_MAJORITY_THRESHOLD`, default `0.83`). The medoid of the largest cluster is returned verbatim; no arbiter LLM call is made. When embedding is unavailable or fails, falls back to the first surviving leg and sets `arbiter.degraded: true`.

**Streaming behavior:**
- `/v1/chat/completions` with `stream: true`: member legs always run non-streaming; the arbiter answer streams back as standard chat completion chunks. A trailing `event: tokentrimmer.panel` SSE frame carries the per-leg attribution (same shape as the non-streaming body key; see §21.4).
- `/v1/messages`: forwards the arbiter's Anthropic-wire SSE events verbatim, including the `tokentrimmer.panel` and `tokentrimmer.usage` frames at the end.
- `/v1/responses`: always non-streaming (the responses bridge sets `stream: false`).

### 21.4 Response

On a successful non-streaming panel request the response body is the standard chat completion (or Anthropic Messages / Responses) shape, with one additional top-level key:

```json
{
  "id": "chatcmpl-abc",
  "object": "chat.completion",
  "choices": [...],
  "usage": { "prompt_tokens": 14, "completion_tokens": 203, "total_tokens": 217, "cached_tokens": 0 },
  "tokentrimmer": {
    "panel": {
      "strategy": "synthesize",
      "total_cost_usd": 0.00341,
      "quorum": { "required": 1, "met": 3 },
      "cost_incomplete": false,
      "legs": [
        {
          "leg_index": 0,
          "role": "leg",
          "model": "gpt-4o",
          "provider": "openai",
          "cost_usd": 0.00080,
          "status": "ok",
          "tokens": { "input_tokens": 14, "output_tokens": 87, "cached_tokens": 0 }
        },
        {
          "leg_index": 1,
          "role": "leg",
          "model": "claude-3-5-sonnet-20241022",
          "provider": "anthropic",
          "cost_usd": 0.00109,
          "status": "ok",
          "tokens": { "input_tokens": 14, "output_tokens": 91, "cached_tokens": 0 }
        },
        {
          "leg_index": 2,
          "role": "leg",
          "model": "gemini-1.5-pro",
          "provider": "google",
          "cost_usd": 0.00021,
          "status": "ok",
          "tokens": { "input_tokens": 14, "output_tokens": 78, "cached_tokens": 0 }
        },
        {
          "leg_index": 3,
          "role": "arbiter",
          "model": "gpt-4o",
          "provider": "openai",
          "cost_usd": 0.00131,
          "status": "ok",
          "tokens": { "input_tokens": 294, "output_tokens": 203, "cached_tokens": 0 }
        }
      ],
      "arbiter": {
        "strategy": "synthesize",
        "cost_usd": 0.00131
      }
    }
  }
}
```

On a **streaming** panel request the same object is emitted as a trailing SSE event before `[DONE]`:

```
event: tokentrimmer.panel
data: {"strategy":"synthesize","total_cost_usd":0.00341,"quorum":{"required":1,"met":3},"cost_incomplete":false,"legs":[...],"arbiter":{"strategy":"synthesize","cost_usd":0.00131}}

event: tokentrimmer.usage
data: {"cost_usd":0.00341,"baseline_cost_usd":0.00341,"saved_usd":0.0,...}

data: [DONE]
```

**`tokentrimmer.panel` fields:**

| Field | Type | Description |
|---|---|---|
| `strategy` | string | `"synthesize"`, `"best-of-n"`, or `"majority"` |
| `total_cost_usd` | float\|null | Aggregate cost (Σ member legs + arbiter). `null` only when every leg was unpriced. |
| `cost_incomplete` | boolean | `true` when any surviving leg reported no catalog price (aggregate is a lower bound). |
| `quorum.required` | integer | Minimum legs required to succeed. |
| `quorum.met` | integer | Legs that actually succeeded. |
| `legs[]` | array | One entry per dispatched leg (members + arbiter), in dispatch order. |
| `legs[].leg_index` | integer | Zero-based index into the member list; arbiter entry uses a sentinel value. |
| `legs[].role` | string | `"leg"` for a panel member, `"arbiter"` for the arbitration call. |
| `legs[].model` | string | Model id dispatched for this leg. |
| `legs[].provider` | string | Provider id used for dispatch. |
| `legs[].cost_usd` | float\|null | Cost for this leg; `null` when the model is unpriced. |
| `legs[].status` | string | `"ok"`, `"error"`, `"timeout"`, or `"skipped_no_cred"`. |
| `legs[].tokens` | object\|null | `{ input_tokens, output_tokens, cached_tokens }`; `null` on non-ok legs. |
| `arbiter.strategy` | string | Strategy name. |
| `arbiter.cost_usd` | float\|null | Cost of the arbiter call alone; `null` for `majority` (no LLM call). |
| `arbiter.chosen_leg` | integer | (`best-of-n` only) leg_index of the chosen answer. |
| `arbiter.reason` | string | (`best-of-n` only) Judge's one-sentence rationale. |
| `arbiter.fell_back` | boolean | (`best-of-n` only) `true` when the judge response was unparseable; fell back to first leg. |
| `arbiter.winning_cluster_size` | integer | (`majority` only) Members in the winning cluster. |
| `arbiter.total_clusters` | integer | (`majority` only) Total distinct clusters found. |
| `arbiter.no_majority` | boolean | (`majority` only) `true` when all answers were distinct (global medoid returned). |
| `arbiter.degraded` | boolean | (`majority` only) `true` when embedding failed; fell back to first leg. |

The `tokentrimmer.panel` shape is **identical across all three ingresses** — `/v1/chat/completions`, `/v1/messages`, and `/v1/responses` all carry or emit the same object.

### 21.5 Preflight budget gate (required)

A Fusion panel request **requires an explicit preflight budget**. Provide one via:

- `X-TokenTrimmer-Cost-Limit-Usd` request header (e.g. `0.10`), OR
- `tt_extras.panel.max_cost_usd` in the body.

The request must also provide an explicit member output cap: `max_completion_tokens` takes precedence when supplied, otherwise use `max_tokens`. The gateway estimates the known total cost (all member calls, requested `n` choices, arbiter candidate fan-in, and the fixed Synthesize/Best-of-N arbiter allowance, including fee multipliers) **before any dispatch**. If the budget or output cap is absent, any required model is unpriced, the strategy has unpriced work, or the estimate exceeds the budget, the request is rejected with `402` before any leg is dispatched. In particular, `majority` is currently rejected by the budget gate because its embedding pass has no pricing contract. This is an admission check, not a runtime spend ceiling; treat the configured value as a maximum planned estimate until runtime reservations, settlement, and cancellation are implemented. A budget-less panel is always rejected.

### 21.6 Billing

A panel request generates **one** `request_logs` row:

| Field | Value |
|---|---|
| `provider` | `"panel"` (sentinel; not a real provider id) |
| `model` | the arbiter model |
| `cost_usd` | Σ all member leg costs + arbiter cost |
| `cached` | `false` (always) |

Per-leg detail is written to the `panel_legs` table (one row per leg, including the arbiter), referenced by the parent `request_logs` row id. Panels bypass the L1/L2 cache entirely and are never cache-hits.

From the billing and overage meter perspective a panel counts as **one request** — `cached=false` ensures the cloud overage meter and the monthly request accumulator both count it once.

`X-TokenTrimmer-Saved-Usd` is `0.0` for panel requests: the panel is the service the caller opted into, so no routing or cache savings are claimed. `X-TokenTrimmer-Cost-Usd` reflects the full aggregate (Σ legs + arbiter).

### 21.7 Controls (env vars)

| Env var | Default | Description |
|---|---|---|
| `TT_PANEL_ENABLED` | `""` (off) | Kill-switch. Set to `1` or `true` to enable. Absent or any other value → disabled. Panel requests receive `403` when disabled. |
| `TT_PANEL_MIN_TIER` | `"Free"` (allow-all) | Minimum caller tier required to use the panel. Accepted values: `Free`, `Pro`, `Team`, `Scale` (case-insensitive). Unknown values log a warning and fall back to `Free`. Below-tier requests receive `403`. |
| `TT_PANEL_MAX_MEMBERS` | `8` | Hard cap on the number of panel members (arbiter not counted). A request specifying more members than the cap receives `400`. Must be ≥ 1; invalid or zero values are ignored. |
| `TT_PANEL_MAJORITY_THRESHOLD` | `0.83` | Cosine-similarity threshold for the `majority` strategy's embedding clustering. Float in `(0.0, 1.0]`; invalid/out-of-range values are ignored and the default is used. |
| `TT_PANEL_DEFAULT_MEMBERS` | `""` (empty) | Comma-separated default member model ids used when `tt_extras.panel.members` is absent. Absent → panel requires per-request members. |
| `TT_PANEL_DEFAULT_ARBITER` | `""` (empty) | Default arbiter model id when `tt_extras.panel.arbiter_model` is absent. Absent → panel requires per-request arbiter. |

**OTel span attributes** (additive; present only on panel requests):

| Attribute | Description |
|---|---|
| `tokentrimmer.panel_strategy` | The strategy name (`synthesize`, `best-of-n`, `majority`) |
| `tokentrimmer.panel_leg_count` | Total dispatched legs (members + arbiter) |
| `tokentrimmer.panel_quorum_required` | Quorum threshold |
| `tokentrimmer.panel_quorum_met` | Legs that succeeded |

**Prometheus metrics** (additional counters on panel traffic):

| Metric | Labels | Description |
|---|---|---|
| `panel_requests_total` | `strategy`, `outcome` (`success`/`quorum_unmet`/`credential_preflight`/`strategy_unsupported`/`error`) | One increment per panel request attempt |
| `panel_legs_total` | `role` (`leg`/`arbiter`), `status` (`ok`/`error`/`timeout`/`skipped_no_cred`) | One increment per dispatched leg |

---

## 22. Audit log integrity

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

## 23. Agent Runs

The agent-run endpoint drives a server-side model→tool→model loop without client round-trips. The loop repeats until the model returns a final (non-tool-calling) answer, `max_turns` is reached, or — when `max_cost_usd` is set — the accumulated served cost would breach the budget. All gateway tools (e.g. `tt_search`, `tt_read`) execute server-side; client-defined tools that are not gateway tools surface as a `requires_action` event/status.

### 23.1 Endpoint

```
POST /v1/agent/runs
```

Authenticate the same way as chat completions (§2): a TokenTrimmer key in `Authorization: Bearer …`.

### 23.2 Request body

| Field | Type | Required | Description |
|---|---|---|---|
| `model` | string | yes | Model id for every turn. Routing may rewrite it per turn. |
| `messages` | array | yes | Initial transcript — system/user messages in OpenAI format. |
| `tools` | array | no | Tool definitions advertised to the model. Defaults to `[]`. |
| `max_turns` | integer | no | Turn cap; clamped to `[1, 32]`. Default: 10. |
| `max_cost_usd` | number | no | Admission guard on accumulated served cost (USD). Before a new turn, the run stops as `incomplete` with `stop_reason: "budget_exhausted"` when accrued cost—plus a best-effort estimate when the model is priced—reaches the cap. A turn already started can settle beyond the cap; this is not a reservation/settlement or provider-invoice guarantee. |
| `stream` | boolean | no | When `true`, the response is a stream of SSE run events (see §23.4). Default `false`. |

### 23.3 Response (non-streaming)

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "status": "completed",
  "messages": ["..."],
  "turns": 3,
  "usage": {
    "prompt_tokens": 520,
    "completion_tokens": 210,
    "cost_usd": 0.00214,
    "per_turn_cost_usd": [0.00063, 0.00091, 0.00060]
  }
}
```

An `incomplete` run that hit the budget cap looks like:

```json
{
  "id": "...",
  "status": "incomplete",
  "turns": 2,
  "usage": {
    "prompt_tokens": 340,
    "completion_tokens": 130,
    "cost_usd": 0.00091,
    "per_turn_cost_usd": [0.00063, 0.00028]
  },
  "stop_reason": "budget_exhausted"
}
```

**Response fields:**

| Field | Type | Description |
|---|---|---|
| `id` | string (UUID) | Unique run identifier. |
| `status` | string | `"completed"` — model returned a final answer; `"incomplete"` — loop stopped without a final answer (turn cap or budget); `"failed"` — a turn errored. |
| `messages` | array | Full message transcript (request messages plus model/tool exchanges). |
| `turns` | integer | Number of model turns completed. |
| `usage.prompt_tokens` | integer | Accumulated prompt tokens across all turns. |
| `usage.completion_tokens` | integer | Accumulated completion tokens across all turns. |
| `usage.cost_usd` | number | Total accumulated served cost (USD) across all turns. |
| `usage.per_turn_cost_usd` | number[] | Served cost (USD) of each completed turn, in turn order. Lets callers see where spend accumulated without per-turn headers. |
| `stop_reason` | string \| null | Machine-readable termination cause — `"max_turns"` (turn cap reached) or `"budget_exhausted"` (`max_cost_usd` would have been exceeded). Omitted on `completed` and `failed` runs. |
| `note` | string | (optional) Human-readable note on why the run ended early. |

### 23.4 Response (streaming, `stream: true`)

When `stream: true`, the response is an SSE stream of named run events. Events are emitted as they occur; the stream closes after the terminal event (`run.completed`, `run.incomplete`, or `run.failed`).

**Event types:**

| Event name | When emitted | Data fields |
|---|---|---|
| `run.turn` | Start of each model turn | `{ type, turn }` |
| `run.turn_cost` | After each completed turn | `{ type, turn, turn_cost_usd, run_cost_usd, budget_remaining_usd? }` |
| `run.message` | After each assistant message | `{ type, message }` |
| `run.tool_result` | After each gateway tool call completes | `{ type, tool_call_id, content }` |
| `run.completed` | Terminal — model returned a final answer | `{ type, run }` |
| `run.incomplete` | Terminal — run stopped without a final answer | `{ type, run }` |
| `run.failed` | Terminal — a turn errored | `{ type, run }` |

**`run.turn_cost` event — fields:**

| Field | Type | Description |
|---|---|---|
| `turn` | integer | 1-based turn index. |
| `turn_cost_usd` | number | Served cost (USD) for this turn alone. |
| `run_cost_usd` | number | Accumulated served cost (USD) across all turns so far, including this turn. |
| `budget_remaining_usd` | number | (optional) Remaining budget (`max_cost_usd − run_cost_usd`). Present only when `max_cost_usd` was set on the request. |

Example stream excerpt (budget-capped run, `max_cost_usd: 0.10`):

```
event: run.turn
data: {"type":"run.turn","turn":1}

event: run.message
data: {"type":"run.message","message":{"role":"assistant","content":null,"tool_calls":[...]}}

event: run.tool_result
data: {"type":"run.tool_result","tool_call_id":"call_abc","content":"..."}

event: run.turn_cost
data: {"type":"run.turn_cost","turn":1,"turn_cost_usd":0.00063,"run_cost_usd":0.00063,"budget_remaining_usd":0.09937}

event: run.turn
data: {"type":"run.turn","turn":2}

...

event: run.completed
data: {"type":"run.completed","run":{"id":"...","status":"completed","turns":3,"usage":{"prompt_tokens":520,"completion_tokens":210,"cost_usd":0.00214,"per_turn_cost_usd":[0.00063,0.00091,0.00060]}}}
```

The terminal event's embedded `run` object has the same shape as the non-streaming response (§23.3).

### 23.5 Rust client (`tt-client`)

The `tokentrimmer-client` crate exposes agent runs via `AgentBuilder`:

```rust
use tt_client::{async_trait, user, system, Client, ToolExecutor};

// A no-op executor for runs that use only server-side (gateway) tools.
struct NoTools;

#[async_trait]
impl ToolExecutor for NoTools {
    async fn call(
        &self,
        _name: &str,
        _arguments: &str,
    ) -> std::result::Result<String, Box<dyn std::error::Error + Send + Sync>> {
        Ok(String::new())
    }
}

let client = Client::new("https://api.tokentrimmer.com", "");

let outcome = client
    .agent()
    .model("gpt-4o-mini")
    .message(system("You are a helpful assistant."))
    .message(user("Summarize the TokenTrimmer README"))
    .max_turns(5)
    .max_cost_usd(0.05)   // preflight cost-admission cap, not a runtime spend ceiling
    .run(&NoTools)
    .await?;

let run = &outcome.run;
println!("status: {:?}", run.status);
println!("turns: {}", run.turns);
println!("total cost: ${:.5}", run.usage.cost_usd);
for (i, cost) in run.usage.per_turn_cost_usd.iter().enumerate() {
    println!("  turn {}: ${:.5}", i + 1, cost);
}
```

From the CLI: `tt agent run --model gpt-4o-mini --max-cost 0.05 "Summarize the README"`.

### 23.6 Durable run list and get

Agent runs are persisted durably to Postgres (`agent_runs` table). Identity and terminal state survive beyond the Redis TTL; the message transcript does not.

#### `GET /v1/agent/runs` — list runs

Returns up to 50 of the authenticated org's agent runs, newest first. Requires Postgres (503 if absent) and a real authenticated org (401 for anonymous callers).

```
GET /v1/agent/runs
Authorization: Bearer tt_live_*
```

Response:

```json
{
  "object": "list",
  "data": [
    {
      "id": "550e8400-e29b-41d4-a716-446655440000",
      "status": "completed",
      "model": "gpt-4o-mini",
      "turns": 3,
      "max_turns": 8,
      "cost_usd": 0.00214,
      "stop_reason": null,
      "tag": "feature=research"
    }
  ]
}
```

**Data item fields:**

| Field | Type | Description |
|---|---|---|
| `id` | string (UUID) | Unique run identifier. |
| `status` | string | `"running"`, `"completed"`, `"incomplete"`, `"failed"`, or `"requires_action"`. |
| `model` | string | Model id used for this run. |
| `turns` | integer | Number of model turns completed so far. |
| `max_turns` | integer \| null | Turn cap supplied at run creation, if any. |
| `cost_usd` | number | Accumulated served cost (USD) across all completed turns. |
| `stop_reason` | string \| null | `"max_turns"` or `"budget_exhausted"` on incomplete runs; `null` otherwise. |
| `tag` | string \| null | Cost-attribution tag from `X-TokenTrimmer-Tag` at run creation, if supplied. |

From the CLI: `tt agent runs`.

#### `GET /v1/agent/runs/:id` — get a run

Fetches the current state of a run. Look-up order:

1. **Redis (L1)** — if configured and the run is still within its TTL. Returns the full run including the live message transcript.
2. **Postgres fallback** — on a Redis miss or when no Redis is configured. Returns durable identity and terminal state; `messages: []` and a note explaining the absence of the transcript.
3. **404** — if neither store holds the run for the authenticated org.

**Durable view note:** once the Redis TTL expires (1 hour after the last write), only the durable Postgres record is available. The Postgres record carries identity and terminal state (`id`, `status`, `turns`, `cost_usd`, `stop_reason`) but not the message transcript. The response in that case has `messages: []` and a `note` field explaining the absence.

```
GET /v1/agent/runs/:id
Authorization: Bearer tt_live_*
```

### 23.7 Request-log attribution (`run_id`)

Every `request_logs` row produced during an agent run carries the run's `id` in the `run_id` column, enabling complete per-run cost attribution in the request log. This includes turns that dispatch the quality judge or a shadow model — all sub-dispatches within the run are attributed to the same `run_id`.

The run's total **served cost** (sum of `x-tokentrimmer-cost-usd` across all turns) is also stored as `agent_runs.cost_usd` and surfaced in the list and get responses as `cost_usd`. Use `agent_runs.cost_usd` when you want the run-level total without summing `request_logs` rows; use `request_logs` with `WHERE run_id = $1` when you want per-turn or per-sub-dispatch detail.

---

## 24. Workflow Engine

The workflow engine lets you define and execute multi-step, multi-node AI pipelines as a DAG of typed nodes. Each node is an LLM call, an agentic loop, a conditional branch, a deterministic transform, or an output collector. Workflows are versioned: every `POST /v1/workflows` stores a new immutable version. The ordinary definition read returns latest, while the bounded version-history endpoints expose exact retained versions read-only.

**Authentication:** a real `tt_live_*` key in `Authorization: Bearer …`. Dogfood keys and anonymous callers receive `401`. Requires Postgres; returns `503` when the pool is absent.

**Execution model:** synchronous, bounded by the gateway's 60-second timeout. Async/queued execution is a future phase.

### 24.1 Workflow definition shape

A workflow definition is a JSON object with the following top-level fields:

| Field | Type | Required | Description |
|---|---|---|---|
| `id` | string (UUID) | no | Client-supplied workflow id. A new UUIDv4 is generated when omitted. |
| `version` | integer | no | Ignored on create; the store computes the next version atomically. Returned on reads. |
| `name` | string | yes | Human-readable workflow name. |
| `nodes` | array | yes | Ordered list of DAG nodes (see node kinds below). |
| `edges` | array | yes | Directed edges connecting nodes. |
| `inputs` | object | no | Freeform input schema / defaults passed to the trigger node. |
| `budget` | object | no | Run-level cost cap (see §24.6). |
| `allowed_hosts` | array of strings | no | Per-workflow egress allowlist for Http nodes (default-deny: `[]`). Each entry is a bare hostname (e.g. `"api.example.com"`). Http nodes whose URL host is not an exact member of this list are rejected at definition-save time and again at run time. |
| `metadata` | JSON value | no | Editor metadata. The engine does not execute it, but clients must preserve it when updating a definition. |
| `triggers` | array | no | Out-of-band invokers. A `schedule` runs on a bounded interval; a `webhook` enables a signed invocation URL. Omit or send `[]` for human-run-only workflows. |

`POST /v1/workflows` is a strict top-level contract: unknown top-level fields
are rejected rather than silently discarded. Read-modify-write clients must
preserve every returned top-level field, including `metadata` and `triggers`.

#### Out-of-band triggers

```json
"triggers": [
  { "type": "schedule", "interval": "6h", "environment": "staging" },
  { "type": "webhook", "token_id": "ops_sync_1", "environment": "production" }
]
```

`schedule.interval` accepts bounded duration components such as `"1h"`,
`"6h"`, `"1d"`, or `"1d6h"`; new or updated definitions must be between one
hour and 30 days. At most one schedule trigger is allowed per workflow. The
hosted dispatcher normally picks due work up on an approximate hourly sweep,
not at an exact wall-clock time; startup, leader acquisition, and the configured
sweep profile can add pickup jitter. A webhook `token_id` must be a non-empty
URL-safe identifier (`A-Z`, `a-z`, `0-9`, `_`, `-`). The signing key and final
URL are managed server-side; do not place a webhook secret in the definition.

Each trigger may optionally select `development`, `staging`, or `production`.
The hosted control plane resolves that environment once when it durably queues
an occurrence, persists the exact workflow version, release revision, and
non-secret variable revision, and replays that tuple unchanged after pointer
movement. Omit `environment` to preserve the legacy latest-saved-definition
behavior. A missing release or positive variable snapshot fails before gateway
provider work; it never falls back to latest or to the current variable set.

Definitions persisted before the one-hour validation floor are not rewritten or
disabled by this change. Operators must inventory and explicitly migrate any
stored sub-hour schedule to one hour or longer; those legacy definitions remain
compatibility-only and do not gain a sub-hour delivery guarantee.

Each edge:

```json
{ "from": "<node_id>", "to": "<node_id>" }
```

`map` is an optional field parsed by the schema but **not yet applied by the engine in W1a** (reserved for a future release). Omit it.

### 24.2 Node kinds

Each node carries an `id` (string, unique within the workflow) and a `type` discriminant plus kind-specific fields:

#### `trigger`

Entry-point; receives the workflow's external `inputs`. No additional fields.

```json
{ "id": "start", "type": "trigger" }
```

#### `model`

Single LLM call.

```json
{
  "id": "summarise",
  "type": "model",
  "selection": { "type": "route", "route_ref": "my-route" },
  "prompt": "Summarise: {{input}}"
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `selection` | object | yes | Which model to use (see §24.3). |
| `prompt` | string | yes | Prompt template; `{{input}}` is replaced with the node's input value. |
| `max_output_tokens` | integer | no | Positive completion-token limit sent to the provider. Omit to retain legacy provider-default output behavior. Required on every model/agent node only when this workflow is admitted under a run-level `max_cost_usd`. |
| `max_cost_usd` | number | no | Per-node preflight USD admission cap; a started node can settle above it. |

#### `agent`

Multi-turn agentic loop with tool access.

```json
{
  "id": "researcher",
  "type": "agent",
  "selection": { "type": "model", "model": "claude-3-5-haiku-20241022" },
  "prompt": "Research: {{input}}",
  "max_turns": 5,
  "tools": []
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `selection` | object | yes | Which model to use (see §24.3). |
| `prompt` | string | yes | Prompt template. |
| `max_turns` | integer | no | Turn cap for the agent loop. Defaults to 8. |
| `max_output_tokens` | integer | no | Positive per-turn completion-token limit. Omit to retain legacy provider-default output behavior. Required on every model/agent node only when this workflow is admitted under a run-level `max_cost_usd`. |
| `max_cost_usd` | number | no | Per-node preflight USD admission cap; a started node can settle above it. |
| `tools` | array | no | Tool definitions advertised to the model. Defaults to `[]`. |

#### `transform`

Deterministic template substitution; no LLM call. The `expr` field is a
`{{ref}}`-template string (same syntax as `prompt` on Model/Agent nodes).

```json
{ "id": "relay", "type": "transform", "expr": "{{summarise}}" }
```

`{{input}}` expands to the trigger payload; `{{node_id}}` expands to that node's
full output; `{{node_id.field}}` extracts a top-level JSON field. Unclosed `{{`
is passed through as-is. jq-style expressions (e.g. `.output | ascii_upcase`) are
**not** supported — use template refs only.

#### `branch`

Conditional; evaluates `cond` and routes to exactly one of `when_true` or
`when_false`.

```json
{
  "id": "gate",
  "type": "branch",
  "cond": "{{score}} == \"pass\"",
  "when_true": "pass_node",
  "when_false": "fail_node"
}
```

`cond` supports three forms:

| Form | Example | Meaning |
|---|---|---|
| `{{ref}} == "literal"` | `{{score}} == "pass"` | String equality after template resolution. |
| `{{ref}} != "literal"` | `{{label}} != "skip"` | String inequality. |
| `{{ref}}` | `{{flag}}` | Truthiness: true unless the resolved value is empty, `"false"`, `"null"`, or `"0"`. |

Literals may be single- or double-quoted. Numeric comparisons (e.g. `.score > 0.5`)
are **not** supported in W1a — coerce to a string label from the model output instead.

#### `output`

Terminal node; collects the workflow's final output. No additional fields.

```json
{ "id": "done", "type": "output" }
```

#### `http`

Outbound HTTP request. The URL host must appear in the workflow definition's
`allowed_hosts` list (default-deny); the definition is rejected at save time
and again at run time if the host is not allowlisted.

```json
{
  "id": "fetch",
  "type": "http",
  "method": "POST",
  "url": "https://api.example.com/v1/classify",
  "headers": [
    ["Authorization", "Bearer {{secrets.EXAMPLE_API_KEY}}"],
    ["Content-Type", "application/json"]
  ],
  "body": "{\"text\": \"{{summarise}}\"}",
  "max_response_bytes": 65536
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `method` | string | yes | HTTP method (`GET`, `POST`, `PUT`, `PATCH`, `DELETE`). |
| `url` | string | yes | Full URL. The host must be a static literal in `allowed_hosts`; path, query, headers, and body may use `{{template}}` tokens. |
| `headers` | array of `[name, value]` pairs | no | Request headers. Values may contain `{{secrets.NAME}}` tokens. |
| `body` | string | no | Request body (raw string). May contain `{{template}}` and `{{secrets.NAME}}` tokens. |
| `max_response_bytes` | integer | no | Maximum response body size in bytes. Default: 1 MiB (1 048 576). Responses exceeding this cap are truncated and the node returns an error. |

**Secret references** — embed a per-org secret as `{{secrets.NAME}}` in any
`headers` value or `body` string. At run time the gateway substitutes the
decrypted value before the request is made. The secret value never appears in
logs, the node journal, or any API response. Store secrets with
`POST /v1/workflows/secrets` (see §24.7). Before executing any node in the
current workflow definition, the engine verifies that every referenced secret
is available and decryptable; otherwise the run fails with zero cost and no
HTTP dispatch.

**Allowlist enforcement** is applied at definition-save time (`POST /v1/workflows`)
and again at run time. A workflow with an `http` node whose host is absent from
`allowed_hosts` will fail validation at save time with a 400 error.

#### `sub_workflow`

Invoke another stored workflow as a child step.  The child runs synchronously
inside the parent run; its cost and baseline cost roll up into the parent's
totals.  The child must belong to the same org.

```json
{
  "id": "enrich",
  "type": "sub_workflow",
  "workflow_id": "c3d4e5f6-0000-0000-0000-000000000001"
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `workflow_id` | string (UUID) | yes | Id of the child workflow definition to invoke. Must be owned by the same org. |
| `version` | integer | no | Accepted but currently ignored — the latest version is always loaded. |

**Same-org scoping.** The child definition is loaded from the authenticated org's
workflow store.  A `workflow_id` that does not exist or belongs to another org
produces a `Failed` run with a `404`-class error.

**Nesting depth cap — `MAX_SUBWORKFLOW_DEPTH = 5`.** The engine tracks the current
recursion depth.  When a `sub_workflow` node would start a child at depth 6 or
greater the run is immediately failed with:

```
sub-workflow nesting exceeds max depth 5
```

**Cycle rejection.** Both self-cycles (a workflow whose `sub_workflow` node points
at its own `workflow_id`) and indirect cycles (A → B → A) are detected before
any recursive call is made.  The guard maintains an ancestors list; if the child's
`workflow_id` matches the current workflow id or any ancestor the run fails with:

```
sub-workflow cycle detected
```

**Budget rollup.** For an uncapped legacy execution, a child's `cost_usd` and
`baseline_cost_usd` are folded into the parent's running totals. The child's
`saved_usd` is **not** added directly
— savings are always re-derived as `(accrued_baseline − accrued_cost)` at the
end of the parent run to prevent double-counting.

**Child failure propagates.** If the child run ends with `Failed` or
`BudgetExhausted`, the parent run also ends with the same status.  Any cost
already incurred by the child is included in the parent's reported `cost_usd`.
A run requesting `max_cost_usd` is instead rejected at static admission before
dispatch when it contains any `sub_workflow`; this rollup is not a reservation
or a cost-ceiling guarantee.

**Output mapping.** The SubWorkflow node's output `content` is the child
workflow's `node_outputs` array serialized as JSON.  Downstream nodes can
reference it with `{{enrich}}` (the node id).

---

#### `loop`

Execute a stored body workflow repeatedly while a condition holds (while-semantics: the condition is checked _before_ each iteration).  Iteration count is bounded by a hard cap to guarantee termination.

```json
{
  "id": "retry",
  "type": "loop",
  "body_workflow_id": "c3d4e5f6-0000-0000-0000-000000000002",
  "cond": "{{retry}} == \"\"",
  "max_iters": 5
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `body_workflow_id` | string (UUID) | yes | Id of the body workflow to run each iteration. Must be owned by the same org. |
| `cond` | string | yes | Branch-style condition evaluated before each iteration (while-semantics). Same syntax as `branch` nodes: `{{ref}} == "literal"`, `{{ref}} != "literal"`, or a bare `{{ref}}` (truthy check). Falsy values: empty string, `"false"`, `"null"`, `"0"`. |
| `max_iters` | integer | yes | Hard termination cap. Must be in `1..=100`. The loop stops after at most this many body executions, regardless of whether the condition is still true. Guarantees the loop always terminates. |

**While-semantics (cond before body).** The condition is evaluated at the start of each iteration, _before_ the body runs.  If the condition is false on the first check the body runs zero times and the loop node is a no-op (status `Succeeded`, zero cost).

**`max_iters` hard cap (1–100).** Passing `max_iters: 0` or `max_iters > 100` is a validation error.  The cap is the definitive termination guarantee: even an always-true condition cannot cause an infinite loop.

**Capped admission.** A public workflow run requesting `max_cost_usd` is
rejected before dispatch when it contains a `loop`, because the future number
of body executions cannot be projected as one bounded direct graph. Existing
uncapped workflows retain loop behavior; this is not a spend reservation or a
runtime cost-ceiling claim.

**Per-iteration output threading.** After each iteration the body's `node_outputs` array is serialized as JSON and stored under the loop node's id in the output map.  The next iteration's condition evaluation and the body's trigger input both receive this value, so downstream cond expressions (e.g. `{{retry}} != "STOP"`) can react to what the body last produced.

**Cost and savings rollup.** The body's `cost_usd` and `baseline_cost_usd` accumulate across iterations and are folded into the parent run's totals on loop exit.  The body's `saved_usd` is **not** added directly — savings are always re-derived as `(accrued_baseline − accrued_cost)` to prevent double-counting.

**Child failure stops the loop.** If any body iteration ends with `Failed` or `BudgetExhausted`, the loop stops immediately and the parent run ends with the same status.  Any cost already incurred is included in the parent's reported `cost_usd`.

**Depth and cycle guards.** The same `MAX_SUBWORKFLOW_DEPTH = 5` nesting cap and cycle-detection logic that apply to `sub_workflow` nodes also apply to `loop` body workflows.

### 24.3 Model selection (`selection`)

The `selection` field on `model` and `agent` nodes determines how the model is resolved:

| `type` | Additional field | Description |
|---|---|---|
| `"model"` | `"model": "<model_id>"` | Pin a specific model (e.g. `"claude-3-5-haiku-20241022"`). |
| `"route"` | `"route_ref": "<route_name>"` | Use a named TokenTrimmer route (resolved at run time). |
| `"auto"` | — | Automatic model selection. **Rejected at definition create/validate time in W1a** — pin a model or route_ref instead. |

### 24.4 Endpoints

#### `POST /v1/workflows` — create / update a workflow definition

Validates and stores a workflow definition. If the `id` is new, version 1 is created. If the `id` already exists (for the same org), the next version is computed atomically.

`expected_latest_version` is an optional write-only optimistic precondition. `0`
means the org/workflow must have no retained version; a positive value must
match its current latest version. A mismatch or a concurrent writer returns
`409` and appends no row. Omitting the field retains the legacy unconditional
append behavior. The ordinary `version` definition field remains ignored for
write concurrency and is not a substitute for this precondition.

**Request body:** a workflow definition object (§24.1).

**Response (`201 Created`):**

```json
{
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "version": 1,
  "content_hash": "a1b2c3d4..."
}
```

**Error codes:** `400` with a list of validation errors (including an unrepresentable precondition); `409` when `expected_latest_version` is stale or a concurrent insert wins (reload before retrying); `500` on a DB write error; `503` when Postgres is unavailable.

---

#### `GET /v1/workflows` — list workflow definitions

Returns the latest version of each workflow definition owned by the authenticated org (up to 100).

**Response:**

```json
{
  "object": "list",
  "data": [
    {
      "id": "550e8400-e29b-41d4-a716-446655440000",
      "name": "My Pipeline",
      "version": 3,
      "created_at": "2026-06-28T12:00:00Z"
    }
  ]
}
```

---

#### `GET /v1/workflows/:id` — get latest workflow definition

Returns the full workflow definition for the latest version of the given id, scoped to the authenticated org. Returns `404` when the id does not exist or belongs to another org. The returned `version` field reflects the authoritative DB version.

---

#### `GET /v1/workflows/:id/versions` — list immutable definition versions

Returns newest-first metadata for at most 100 retained versions of exactly one
org-owned workflow. The handler fetches one extra row to set `truncated` and
never returns another org's existence or metadata. The response is
`Cache-Control: private, no-store`.

```json
{
  "object": "list",
  "workflow_id": "550e8400-e29b-41d4-a716-446655440000",
  "data": [
    {
      "version": 3,
      "content_hash": "a1b2c3d4...",
      "created_at": "2026-06-28T12:00:00Z"
    }
  ],
  "truncated": false
}
```

Returns `404` for an absent or differently owned workflow, `500` on a storage
read/decode failure, and `503` when Postgres is unavailable.

#### `GET /v1/workflows/:id/versions/:version` — get one immutable version

Returns the exact retained definition and its authoritative version,
`content_hash`, and `created_at` metadata. `version` must be a positive integer.
The definition's embedded `version` is patched from the database row, and the
response is `Cache-Control: private, no-store`. An absent, unowned, or
non-retained version returns `404`.

#### `GET /v1/workflows/:id/versions/:from/compare/:to` — compare two immutable versions

Returns a deterministic, value-free structural comparison of two positive
org-owned versions. `from` and `to` metadata include each authoritative
version, content hash, and creation timestamp. `data` contains only RFC 6901
JSON Pointer paths and one of `added`, `removed`, or `modified`; neither
definition's values are echoed. The server-owned embedded `version` field is
excluded from authored changes.

At most 256 entries are returned. The walker stops at 64 nested levels, reports
that subtree as one modified path, and fetches one further detected change only
to set `truncated`. The response is `Cache-Control: private, no-store`.

```json
{
  "object": "workflow_definition_version_diff",
  "workflow_id": "550e8400-e29b-41d4-a716-446655440000",
  "from": {
    "version": 2,
    "content_hash": "0a1b2c3d...",
    "created_at": "2026-06-27T12:00:00Z"
  },
  "to": {
    "version": 3,
    "content_hash": "a1b2c3d4...",
    "created_at": "2026-06-28T12:00:00Z"
  },
  "changed": true,
  "data": [
    { "path": "/name", "kind": "modified" },
    { "path": "/metadata/canvas_positions/new-node", "kind": "added" }
  ],
  "truncated": false
}
```

The immutable history/read/compare endpoints do not mutate release state.

#### Workflow release state — explicit publish, promotion, and rollback pointers

Every immutable version starts as a draft. Release state is separate metadata:
one compare-and-swap current pointer per `development`, `staging`, and
`production` environment, backed by an append-only release ledger. Release
responses include versions, hashes, revisions, transition provenance, and
timestamps only—never definitions or secret values—and are
`Cache-Control: private, no-store`.

`GET /v1/workflows/:id/release-state` returns the newest saved version plus the
current pointer for each environment that has a release. `data` is always in
development → staging → production order and contains at most three entries.

```json
{
  "object": "workflow_release_state",
  "workflow_id": "550e8400-e29b-41d4-a716-446655440000",
  "latest": {
    "version": 4,
    "content_hash": "draft-hash...",
    "created_at": "2026-07-20T18:00:00Z"
  },
  "data": [
    {
      "environment": "development",
      "revision": 2,
      "workflow_version": 3,
      "content_hash": "released-hash...",
      "action": "publish",
      "source_environment": null,
      "source_revision": null,
      "created_at": "2026-07-20T17:00:00Z"
    }
  ]
}
```

`GET /v1/workflows/:id/environments/:environment/releases` returns at most 100
newest-first immutable release records for one environment and an explicit
`truncated` flag. This history supplies the exact `release_revision` accepted
by rollback without exposing a retained definition.

`POST /v1/workflows/:id/environments/development/publish` points development
at one exact retained version. `expected_release_revision` is `0` only when no
development release exists; otherwise it must match the current revision and
the target workflow version must be newer than development's current version.
Restoring an older version uses the rollback endpoint and same-environment
release history.

```json
{ "workflow_version": 4, "expected_release_revision": 2 }
```

`POST /v1/workflows/:id/environments/:environment/promote` accepts only
`staging` or `production`. Staging copies the exact current development
version; production copies the exact current staging version. Both source and
target revisions are required optimistic preconditions, and the caller cannot
substitute a different version.

```json
{ "expected_source_revision": 2, "expected_release_revision": 0 }
```

`POST /v1/workflows/:id/environments/:environment/rollback` appends a new
current release that restores the workflow version recorded by an earlier
release revision in that same environment. It cannot target an arbitrary saved
draft or another environment's history.

```json
{ "release_revision": 1, "expected_release_revision": 2 }
```

All three mutations return `409` for a stale revision, concurrent winner,
missing source transition, same-version no-op, or otherwise invalid current
state, with no partial pointer/ledger update. A missing publish target returns
`404`; malformed/nonpositive/out-of-range inputs return `400`. Because a
successful transition increments the target revision, its expected revision
must be at most 2,147,483,646.

This contract does not constitute human approval, and it does not silently
change legacy latest-version Estimate/Run or durable-trigger behavior. Direct
Estimate/Run callers may opt into the explicit environment selector below;
automatic trigger activation and attributable approval remain separate gates.

---

#### Workflow environment variables — versioned non-secret configuration

`GET` and `PUT /v1/workflows/:id/environments/:environment/variables` manage
one independently versioned configuration map for `development`, `staging`,
or `production`. This is explicitly a **non-secret** surface: management reads
return the values. Credentials belong in the encrypted workflow-secrets API.
Responses use `Cache-Control: private, no-store`.

An environment without a stored snapshot returns the implicit empty revision:

```json
{
  "object": "workflow_environment_variables",
  "workflow_id": "550e8400-e29b-41d4-a716-446655440000",
  "environment": "production",
  "revision": 0,
  "variables": {},
  "updated_at": null
}
```

`PUT` replaces the complete map under an optimistic `expected_revision`:

```json
{
  "expected_revision": 0,
  "variables": {
    "API_BASE": "https://api.example.com/v2",
    "REGION": "us-east"
  }
}
```

Names must match `^[A-Z0-9_]{1,64}$`. A set contains at most 100 entries,
each value at most 4,096 UTF-8 bytes, and at most 65,536 encoded bytes in
total. Unknown request fields and unknown environments are rejected. A stale
revision returns `409`; an exact retry of the current value is idempotent.
Each successful change appends an immutable snapshot and advances only the
selected environment's current pointer.

Executable template fields may bind `{{variables.NAME}}`. An environment-bound
Estimate/Run accepts the current immutable variable snapshot independently of
the release pointer and reports its exact revision. Before an execution starts,
the gateway validates every variable reference in the bounded, frozen nested
workflow tree and fails at zero cost if a required name is absent. A latest- or
exact-version run has no environment snapshot, so definitions containing these
references fail closed on those execution paths. The accepted map applies to
the root and every nested workflow in that run.

---

#### `POST /v1/workflows/:id/estimate` — pre-run cost projection

Returns a static cost projection without making model calls. By default it
uses the workflow's latest definition.

Set optional positive `workflow_version` (legacy alias: `version`) to estimate
that exact retained org-owned definition instead of latest. Alternatively set
`workflow_environment` to `development`, `staging`, or `production` to resolve
that environment's exact current immutable release. The two selectors are
mutually exclusive. A missing requested version or environment release returns
`404`; zero/negative versions and unknown environments return `400` before
execution.

**Request body:**

```json
{ "inputs": {}, "workflow_version": 3 }
```

```json
{ "inputs": {}, "workflow_environment": "production" }
```

**Response:**

```json
{
  "workflow_version": 3,
  "workflow_environment": "production",
  "release_revision": 2,
  "variables_revision": 4,
  "projected_cost_usd": 0.0012,
  "per_node": [
    { "node_id": "summarise", "model": "claude-3-5-haiku-20241022", "cost_usd": 0.0008 },
    { "node_id": "researcher", "model": null, "cost_usd": null }
  ],
  "warnings": ["node 'researcher': selection type 'auto' is not yet resolvable at estimate time"]
}
```

`cost_usd: null` on a node means the estimate is unavailable (e.g. `auto` selection or unknown route). `warnings` lists any nodes that could not be estimated and why.

---

#### `POST /v1/workflows/:id/runs` — run a workflow

Executes the workflow synchronously and returns the result. Bounded by the 60-second gateway timeout.

For durable callers, send a stable Idempotency-Key header. The gateway hashes
that value before storage and creates or reuses one run per (org, workflow,
key). The first accepted request binds the key to its immutable workflow
version, canonical JSON inputs, max_cost_usd request option, and explicit
environment or frozen release selector when supplied. A retry with changed
inputs, cost, version, environment, release revision, or variable revision
receives 409. Repeating the same current-environment selector
reuses the release and variables revisions accepted first even if either current
pointer has since advanced.
Legacy retries without `workflow_environment` preserve their pre-existing
fingerprint and behavior. Clients without Idempotency-Key retain the historical
behavior: every request creates a fresh run.

**Request body:**

```json
{
  "inputs": { "text": "Summarise this document…" },
  "workflow_environment": "production",
  "max_cost_usd": 0.05
}
```

| Field | Type | Required | Description |
|---|---|---|---|
| `inputs` | object | no | Input values passed to the trigger node. |
| `workflow_version` | integer | no | Exact immutable definition version to execute. Omit for the latest version on a new invocation. `version` is accepted as a compatibility alias. |
| `workflow_environment` | string | no | Current `development`, `staging`, or `production` release to resolve before execution. Normally mutually exclusive with `workflow_version`; durable automatic delivery uses the complete four-field frozen tuple described below. Route detours have a separate optional `then.workflow.environment` selector (§10.7). |
| `release_revision` | integer | no | Positive immutable release-ledger revision. Valid only with all of `workflow_version`, `workflow_environment`, and `variables_revision`. |
| `variables_revision` | integer | no | Nonnegative immutable non-secret variable revision; `0` is the implicit empty set. Valid only as part of the complete frozen tuple. |
| `max_cost_usd` | number | no | Run-level USD cap. Superseded by `def.budget.max_cost_usd` when that is set. |

Idempotency-Key is optional, must be 1–256 visible bytes, and is never
returned or stored in raw form. stream is deliberately not part of the logical
invocation binding: a retry may request status JSON after a lost SSE connection
without starting a second run.

Schedule/webhook bridges use a complete frozen selector:

```json
{
  "inputs": {},
  "workflow_version": 3,
  "workflow_environment": "production",
  "release_revision": 2,
  "variables_revision": 4,
  "stream": false
}
```

That four-field shape requires `Idempotency-Key`. Any partial tuple, unknown
environment, nonpositive workflow/release revision, negative variable revision,
missing immutable ledger row, or release/version mismatch fails closed. The
gateway reads the exact release and variable ledgers and does not consult either
current pointer for this request shape.

**Response:**

```json
{
  "run_id": "550e8400-e29b-41d4-a716-446655440000",
  "workflow_version": 3,
  "workflow_environment": "production",
  "release_revision": 2,
  "variables_revision": 4,
  "status": "completed",
  "cost_usd": 0.00312,
  "baseline_cost_usd": 0.00480,
  "saved_usd": 0.00168,
  "node_outputs": [
    {
      "node_id": "summarise",
      "content": "The document discusses…",
      "cost_usd": 0.00312
    }
  ]
}
```

| Field | Type | Description |
|---|---|---|
| `run_id` | string (UUID) | The run's unique identifier. |
| `workflow_version` | integer | Exact immutable definition version that executed. |
| `workflow_environment` | string | Present only when the caller selected an environment; the closed development/staging/production value resolved before execution. |
| `release_revision` | integer | Present with `workflow_environment`; exact immutable release-ledger revision bound to the run. |
| `variables_revision` | integer | Present with `workflow_environment`; exact immutable non-secret configuration snapshot accepted by the run. `0` is the implicit empty set. |
| `status` | string | Terminal status of the run (see values below). |
| `cost_usd` | number | Total served cost (USD) across all nodes in this run. |
| `baseline_cost_usd` | number | What the run would have cost without routing — sum of per-node baseline costs (the default/first-ranked model's price). Zero when no model nodes executed. |
| `saved_usd` | number | `baseline_cost_usd − cost_usd`, floored at 0. Positive values indicate routing saved money; 0 when cost ≥ baseline (e.g. the route was unchanged). This is a catalog-priced run-level estimate, not provider-invoice reconciliation. An eligible retained terminal run may be minted on demand as a signed estimate; it is not automatic per-run proof. |
| `node_outputs` | array | Per-node output and cost (see below). |

With `"stream": true`, the response is a named SSE stream. Before any node
event or provider work, `run.start` carries the run id and exact accepted
immutable definition version. An environment-bound run additionally carries a
single nested `workflow_release` object, so environment plus release and
variables revisions cannot be separated on the wire:

```text
event: run.start
data: {"type":"run.start","run_id":"550e8400-e29b-41d4-a716-446655440000","workflow_version":3,"workflow_release":{"environment":"production","revision":2,"variables_revision":4}}
```

Selector-free and exact-version streams omit `workflow_release`. Clients should
validate this first event against their request before presenting release
provenance; a malformed or mismatched event does not mean the already accepted
run was cancelled, so reconcile it through the retained run endpoint.

An exact Idempotency-Key replay returns the existing run_id and current totals
with 200 for a terminal run or 202 while it is running, plus the
Idempotent-Replay: true header and a replayed: true response field. It is a
status/reconciliation response, so node_outputs is empty; use
GET /v1/workflows/runs/:run_id for the durable run status. It never executes
the graph again. Recent-run and single-run reads likewise expose
`workflow_environment`, `release_revision`, and `variables_revision` only for
environment-bound runs;
legacy latest/exact-version rows retain null provenance.

#### `GET /v1/workflows/runs/:run_id/nodes` — inspect the retained node journal

Returns at most 500 persisted node-journal rows for one org-owned run. The
gateway first resolves the run under the authenticated org, then joins the
node rows through that owned run; a foreign run id returns 404. Labels,
`node_type`, and `definition_position` are derived from the exact immutable
workflow version recorded on the run, not from the latest editor definition.
Successful responses include `Cache-Control: private, no-store`.

The response also includes `workflow_version`, `workflow_name`, and the
definition's freeform `workflow_inputs_schema` so an inspector can compare the
accepted run inputs with the contract/defaults that executed.

```json
{
  "object": "list",
  "run_id": "550e8400-e29b-41d4-a716-446655440000",
  "workflow_id": "8aa43e0d-f318-4d3c-bb52-7ed725a4fb51",
  "workflow_version": 3,
  "workflow_name": "Summarise and score",
  "workflow_inputs_schema": { "text": { "type": "string" } },
  "truncated": false,
  "data": [
    {
      "id": "ab21f7aa-c0e7-44aa-8601-03a44fedf860",
      "journal_index": 1,
      "node_id": "summarise",
      "definition_position": 2,
      "node_type": "model",
      "label": "02. model · summarise",
      "attempt": 1,
      "status": "completed",
      "output": { "content": "The document discusses…" },
      "cost_usd": 0.00312,
      "model_used": "gpt-4o-mini",
      "error": null,
      "timing_source": "gateway_node_envelope",
      "started_at": "2026-07-20T10:00:00.000Z",
      "finished_at": "2026-07-20T10:00:00.125Z",
      "duration_ms": 125,
      "legacy_recorded_at": null
    }
  ]
}
```

Node journaling is best-effort, so an empty list does not prove that no node
executed. For new rows, `started_at`, `finished_at`, and `duration_ms` describe
the gateway workflow-node envelope; for Model/Agent nodes that spans the
executor call, not individual provider attempts. Internal retries, fallback
attempts, and provider timing are not reconstructed. Legacy rows instead use
`timing_source: "legacy_post_run_persistence"`, set only
`legacy_recorded_at`, and leave the three execution fields null.
`journal_index` is stable for repeated reads, but is not a duration or a
provider-attempt sequence. Trigger/Output nodes and some early guard failures
do not produce rows, and subworkflow child identity is not reconstructed. This
endpoint is read-only inspection, not a breakpoint or replay API.

**`status` values:**

| Value | Meaning |
|---|---|
| `"completed"` | All nodes executed successfully. |
| `"failed"` | A node returned an error; the run was aborted. |
| `"budget_exhausted"` | The run-level `max_cost_usd` budget was reached before a node could execute; the run was stopped. |

### 24.5 Full example

```json
{
  "name": "Summarise and score",
  "nodes": [
    { "id": "start", "type": "trigger" },
    {
      "id": "summarise",
      "type": "model",
      "selection": { "type": "model", "model": "claude-3-5-haiku-20241022" },
      "prompt": "Summarise this in one sentence: {{input}}",
      "max_output_tokens": 128
    },
    {
      "id": "score",
      "type": "model",
      "selection": { "type": "model", "model": "claude-3-5-haiku-20241022" },
      "prompt": "Rate the quality of this summary 0–1: {{input}}",
      "max_output_tokens": 128
    },
    { "id": "done", "type": "output" }
  ],
  "edges": [
    { "from": "start", "to": "summarise" },
    { "from": "summarise", "to": "score" },
    { "from": "score", "to": "done" }
  ],
  "budget": { "max_cost_usd": 0.10, "on_exceed": "stop" }
}
```

### 24.6 Budget policy

The `budget` field asks the gateway to perform bounded static budget admission
before it creates a run record or dispatches a provider request. An admitted
capped wave then runs priceable single-turn nodes sequentially: each directional
preview is reserved against the remaining value and settled to actual node cost
before the next sibling can launch. Immediately before dispatch, the final
routed/shaped request is priced again against that node's effective remaining
value. A capped workflow node allows one provider attempt: route fallbacks,
retries, shadow/panel/workflow fan-out, quality-judge work, and diff re-emission
are suppressed because those extra legs are not included in the reservation.
This in-memory reservation is not a hard runtime cost ceiling or
provider-invoice guarantee: a started provider turn can still settle
differently from its directional estimate.

```json
{ "max_cost_usd": 0.10, "on_exceed": "stop" }
```

| Field | Type | Default | Description |
|---|---|---|---|
| `max_cost_usd` | number | none | Requested USD value for static admission and runtime reservation checks. Capped admission requires each model/agent node to set a positive `max_output_tokens`, only `{{input}}` prompt references, pinned priceable selections, single-turn agents without tools, and no loops/sub-workflows. |
| `on_exceed` | string | `"stop"` | Action for the runtime guard when it detects a budget condition. Only `"stop"` is supported; it does not make a started call or provider invoice hard-capped. |

A `max_cost_usd` can also be supplied per-run in the `POST /v1/workflows/:id/runs` request body. The definition-level budget (`def.budget.max_cost_usd`) takes precedence when both are set.

### 24.7 HTTP node secrets

#### `POST /v1/workflows/secrets` — store or rotate a per-org secret

Encrypts the supplied value with XChaCha20-Poly1305 (per-row derived key,
AAD-bound to `(org_id, name)`) and upserts it into the `workflow_secrets`
table. On conflict the ciphertext is rotated in-place. **The value is never
returned in any response or log.**

**Request body:**

```json
{ "name": "EXAMPLE_API_KEY", "value": "sk-live-abc123..." }
```

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | string | yes | Secret name. Must match `^[A-Z0-9_]{1,64}$` (uppercase letters, digits, and underscore). |
| `value` | string | yes | Plaintext secret value, 1–65,536 UTF-8 bytes. Encrypted before storage; never echoed. |

**Status codes:**

| Code | Meaning |
|---|---|
| `204 No Content` | Secret stored (or rotated) successfully. |
| `400 Bad Request` | `name` is invalid, or `value` is empty or larger than 65,536 UTF-8 bytes. |
| `401 Unauthorized` | Missing or invalid bearer token, or dogfood key. |
| `503 Service Unavailable` | `TT_MASTER_KEY` not configured on this gateway instance. |

**Example:**

```bash
curl -X POST https://gateway.tokentrimmer.com/v1/workflows/secrets \
  -H "Authorization: Bearer tt_live_..." \
  -H "Content-Type: application/json" \
  -d '{"name": "EXAMPLE_API_KEY", "value": "sk-live-abc123"}'
# → 204 No Content
```

Secrets are scoped to the authenticated org. Each org's secrets are isolated:
a secret named `MY_KEY` for org A cannot be read or decrypted by org B.

#### `GET /v1/workflows/secrets` — list safe secret metadata

Returns at most 500 names in ascending order for the authenticated org. The
response contains no secret value, ciphertext, hash, length, or key material,
and carries `Cache-Control: private, no-store`.

```json
{
  "object": "list",
  "data": [
    {
      "name": "EXAMPLE_API_KEY",
      "state": "ready",
      "created_at": "2026-07-19T12:00:00Z",
      "rotated_at": null
    }
  ],
  "truncated": false
}
```

`ready` means only that the ciphertext decrypts under this gateway's current
master key; it does not validate the credential with a downstream provider.
`unusable` means a master key is configured but the row does not decrypt.
`unavailable` means this gateway has no configured master key. Names are still
security-sensitive metadata and should not be cached or copied into prompts.

#### `DELETE /v1/workflows/secrets/:name` — delete a per-org secret

Idempotently removes the named ciphertext for the authenticated org and
returns `204 No Content`, including when the name is already absent. `name`
must match `^[A-Z0-9_]{1,64}$`; an invalid name returns `400`. Deletion does
not require `TT_MASTER_KEY`, does not rewrite stored workflow versions, and
does not disclose whether another org has the same name.

Deleting a referenced name takes effect immediately. Every retained workflow
version and durable trigger that starts a run loads and checks the complete
executable nested-definition tree before the root Trigger or any parent node.
The run fails at zero cost if any checked definition has a malformed, missing,
or unusable secret or environment-variable reference. The preparation is bounded to 256 nested definitions
through the five supported depth levels, uses org-scoped depth-batch reads, and
reuses those exact loaded definitions during execution so a newer saved child
cannot bypass the check. Recreate the same name with `POST` to restore future
runs; the previous value is not recoverable through this API.

#### Using secrets in Http nodes

Reference a stored secret in any `headers` value or `body` string using
`{{secrets.NAME}}`:

```json
{
  "headers": [["Authorization", "Bearer {{secrets.EXAMPLE_API_KEY}}"]],
  "body": "{\"api_key\": \"{{secrets.EXAMPLE_API_KEY}}\"}"
}
```

At run time the gateway resolves `{{secrets.NAME}}` to the decrypted value
before making the HTTP request. Missing, undecryptable, or unavailable
references anywhere in the bounded nested tree fail closed before the root
workflow executes its Trigger or any provider/HTTP node. The exact definitions
checked during preparation are the definitions reused by recursive execution.

#### Security model

- **Guarded egress.** Http nodes use an internal HTTP client configured with
  `redirect=none` and a per-request timeout. Redirects are not followed.
- **Allowlist default-deny.** An Http node can only reach hosts explicitly
  listed in the workflow's `allowed_hosts`. The check is enforced at
  definition-save time (400 on violation) and again at run time (node fails
  without making the request). Template tokens in the URL host position are
  rejected at save time — only static literals may appear in the host.
- **Response-byte cap.** Responses are truncated at `max_response_bytes`
  (default 1 MiB). The `Content-Length` header is not trusted; the cap is
  enforced while streaming the body.
- **Secrets never logged.** Secret values are not written to the node journal,
  the run record, the request log, or any error message. The `{{secrets.NAME}}`
  tokens are substituted in-memory immediately before the HTTP call.
- **AAD-bound ciphertext.** Each secret's ciphertext is bound to `(org_id, name)`
  via AEAD additional-data; copying a ciphertext row to a different org or
  renaming the secret causes a decryption failure rather than a silent decrypt.
- **Deletion is fail-closed, not a definition rewrite.** Removing a name leaves
  historical definitions intact and makes their next execution fail secret
  preflight. The endpoint is idempotent so transport retries are safe.

---

**End of API reference.**

Companion docs:
- Architecture spec (overall system design)
- Provider adapter guide (adding new providers)
- Plan replay design (how Plan simulations work)
- Inspect rule catalog (full rule list with detection logic)
- Integration guides (n8n, LangChain, LangGraph, Dify)
