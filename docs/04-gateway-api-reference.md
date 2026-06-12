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
| `/v1/completions` (legacy) | POST | ✗ not supported |
| `/v1/responses` (OpenAI Responses API) | POST | ✗ not yet supported — use `/v1/chat/completions` |
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
| `X-TokenTrimmer-Batch-Forgone-Usd` | on dispatched/cached responses — the **forgone** Batch-API discount for batch-eligible requests (advisory `then.batch` route action), priced from the served model's real catalog batch rate. An advisory projection for the future async Batch Lane: the request was dispatched synchronously and billed in full, so this figure is **never** included in `X-TokenTrimmer-Saved-Usd`. `0.000000` for all unmarked traffic. | `0.0125` |
| `X-TokenTrimmer-Minify-Saved-Est-Usd` | on dispatched/cached responses — the **ESTIMATED** saving from minified-JSON output steering (`then.minify_json` route action): the emitted JSON re-rendered pretty and re-tokenized with the served model's tokenizer, minus the tokens actually emitted, priced at the billed output rate. An estimate of an unmeasurable counterfactual: **never** included in `X-TokenTrimmer-Saved-Usd`. `0.000000` for un-minified traffic, non-JSON responses, and streaming (v1 meters but does not estimate). | `0.000312` |
| `X-TokenTrimmer-Route-Matched` | the applied route's name, on routed responses (forced or condition-matched) | `cheap-for-short` |
| `X-TokenTrimmer-Warnings` | on dispatched responses, when the gateway altered the request | `param_dropped:frequency_penalty,param_dropped:n` |

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
TokenTrimmer figure, so `Saved-Usd` reconciles against the provider invoice.
`X-TokenTrimmer-Cost-Usd` always reflects what the provider actually bills
(cache discounts included).

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
`request_logs` row carries `route_paused = true`. Like every warnings token,
`route_paused` appears on dispatched chat/messages responses only: L1/L2
cache-hit responses return before the warnings header is assembled (the
durable `request_logs.route_paused` marker is still set on hit rows), and the
embeddings endpoint has no warnings header — a paused embeddings passthrough
is visible only in the `route_paused_passthrough_total` metric and the
gateway log line.

The advisory batch-eligibility route action (`then.batch`) emits its own
tokens — honest by design, since the gateway dispatches synchronously today and
the action never changes the bill:

- `batch_deferred_unavailable` — the marker was applied: the row is tagged
  batch-eligible and the forgone discount is reported on
  `X-TokenTrimmer-Batch-Forgone-Usd`, but there is no async Batch Lane yet, so
  the request was served and billed normally. Corner case: eligibility is
  assessed on the *planned* model, while the forgone discount is priced only
  from the *served* model's real catalog batch rate — after a failover to a
  model with no batch tier, this token can therefore accompany a `0.000000`
  forgone figure (intent stays auditable; no rate, no claim).
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
DELETE /v1/routes/:id          → delete a route
POST   /v1/routes/:id/pause    → sticky-pause the route's rewrite
POST   /v1/routes/:id/resume   → the ONLY thing that clears a pause
GET    /v1/routes/:id/savings  → windowed netted savings (see below)
```

#### Pause / resume

A **paused** route still *matches* — requests attribute to it
(`X-TokenTrimmer-Route-Matched`, the `route_paused:<name>` warnings token, and
`request_logs.route_paused = true`) — but its rewrite and every other **cost**
lever (`fallbacks`, `flex`, `compress`, `traffic_pct`, `shadow_model`,
`max_cost_usd`) are suppressed, so requests flow to their originally-requested
model: the **expensive, quality-safe** direction. **Safety** levers (`redact`,
`disable_cache`) stay live — pausing a quality gate never disables a privacy
guardrail. A forced `X-TokenTrimmer-Route` header does **not** bypass a pause.

Pauses are **sticky**: created manually (`POST /v1/routes/:id/pause`) or by the
opt-in quality auto-pause (`then.auto_pause` — see the
[routing rules guide](routing-rules-guide.md)), they persist until an explicit
`POST /v1/routes/:id/resume`. A resume retains the pause record with a
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
  "gross_saved_usd": 12.41,      // Σ per-request Saved-Usd over the route's rows
  "judge_tax_usd": 0.83,         // paired-judge calls + baseline reference dispatches
  "shadow_tax_usd": 0.22,        // discarded shadow-arm spend
  "net_saved_usd": 11.36,        // gross − judge_tax − shadow_tax; MAY BE NEGATIVE
  "unmetered_tax_rows": 3,       // rows whose tax is unmetered (NULL cost) —
                                 // when > 0 the taxes are lower bounds and the
                                 // net is an upper bound
  "verdicts": { "judged": 41, "acceptable": 38, "degraded": 2,
                "unclear": 1, "pass_rate": 0.95 }
}
```

`net_saved_usd` is deliberately **not clamped at zero**: a regressing route
whose verification spend exceeds its swap saving must show a negative net.
Per-request figures (`X-TokenTrimmer-Saved-Usd`, `request_logs`) stay gross — a
single request doesn't carry the amortized measurement tax; netting exists only
at this aggregate surface. The shipped gateway binary wires the Postgres
savings source automatically at boot when `DATABASE_URL` is set; without a
database the endpoint answers `503` (aggregation not configured). An existing
route with no in-window traffic answers an honest all-zero body, not `404`.

Two semantics to read the numbers with:

- `gross_saved_usd` is the route's **full per-request savings headline**
  (`X-TokenTrimmer-Saved-Usd`) summed over its rows — model-swap savings plus
  any L1/L2 cache-hit, Flex, and compression savings on route-attributed
  requests. It is **not** the model-swap delta alone; notably, a paused route
  still serves L1/L2 hits (caching is a safety-neutral lever), so a paused
  route's gross can keep growing from cache hits while its rewrite is
  suppressed. The `verdicts` block and `net_saved_usd` are the
  quality-regression signal; the gross line is invoice-reconcilable savings
  attribution.
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
Tokens used × per-model pricing from our curated pricing table (a snapshot embedded at build time and refreshed on each release, not auto-refreshed). Reported in `X-TokenTrimmer-Cost-Usd` and reconciled against actual provider invoices monthly.

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
- Integration guides (n8n, LangChain, LangGraph, Dify)
