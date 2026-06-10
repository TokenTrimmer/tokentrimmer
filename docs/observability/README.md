# Observability — gateway traces & the Grafana dashboard

The gateway emits an OpenTelemetry span per request (`http_request`, created at
the request-entry middleware — `crates/core/src/middleware/trace.rs`). When the
cost is known (end of request, alongside the `x-tokentrimmer-*` response
headers) the chat and embeddings handlers record the OpenTelemetry **GenAI
semantic-convention** attributes plus TokenTrimmer **cost** attributes onto that
span (`crates/telemetry/src/gen_ai.rs`, called from
`crates/core/src/routes/chat.rs` and `crates/core/src/routes/embeddings.rs`).
The chat path stamps the attributes on every served outcome — non-streaming
miss, streaming miss, L1/L2 cache hit, and the streaming/fake-stream L1 hit — so
streaming and cache-hit traffic are not undercounted. Spans export over OTLP when
`OTEL_EXPORTER_OTLP_ENDPOINT` is set.

## Attributes emitted on the `http_request` span

GenAI semconv (`gen_ai.*`):

| Attribute | Meaning |
| --- | --- |
| `gen_ai.system` | Provider (`openai`, `anthropic`, `gcp.gemini`, `groq`, `mistral_ai`, …). |
| `gen_ai.provider.name` | Newer semconv spelling of `gen_ai.system`; emitted with the same value for forward-compat. |
| `gen_ai.operation.name` | `chat` on `/v1/chat/completions`, `embeddings` on `/v1/embeddings`. |
| `gen_ai.request.model` | Model the caller asked for (pre-routing). |
| `gen_ai.response.model` | Model that actually served the request (post-routing / failover). |
| `gen_ai.usage.input_tokens` | Prompt tokens. |
| `gen_ai.usage.output_tokens` | Completion tokens. |

TokenTrimmer cost (`tokentrimmer.*`), mirroring the `x-tokentrimmer-*` headers
— the same per-request values `compute_cost` produced (not recomputed):

| Attribute | Mirrors header |
| --- | --- |
| `tokentrimmer.cost_usd` | `x-tokentrimmer-cost-usd` — what the provider bills. |
| `tokentrimmer.baseline_cost_usd` | `x-tokentrimmer-baseline-cost-usd` — cost with no TokenTrimmer. |
| `tokentrimmer.saved_usd` | `x-tokentrimmer-saved-usd` — TokenTrimmer-attributed savings. |
| `tokentrimmer.provider_cache_saved_usd` | `x-tokentrimmer-provider-cache-saved-usd` — provider-side cache discount. |
| `tokentrimmer.cache` | `x-tokentrimmer-cache` — cache outcome (`hit-l1`, `hit-l2`, `miss`, `none`). |
| `tokentrimmer.route` | `x-tokentrimmer-route-matched` — matched route name (when routing applied). |

## Dashboard

`grafana-tokentrimmer-gateway.json` is an importable Grafana dashboard with
panels for request rate, p50/p95 latency, spend over time, savings, cache hits
by layer, cache-hit rate, and tokens by model.

The panels query the `http_request` span attributes above through a **Tempo**
(or any TraceQL-metrics-capable) trace data source, using TraceQL metrics
queries (`rate()`, `quantile_over_time`, `sum_over_time`, `count_over_time`).
On import, Grafana prompts for the Tempo data source (`DS_TEMPO`); the dashboard
also exposes a `${tempo}` data-source template variable so you can switch
sources without editing panels.

Import: Grafana → Dashboards → New → Import → upload the JSON, then pick your
Tempo data source. Requires Grafana 11+ and a Tempo backend with TraceQL
metrics enabled (it ingests the OTLP spans the gateway exports).
