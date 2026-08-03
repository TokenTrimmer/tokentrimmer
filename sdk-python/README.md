# tokentrimmer

Thin Python SDK over the official OpenAI client — routes through the TokenTrimmer Gateway, surfaces cost + cache metadata.

For a reproducible development environment, use the checked-in uv lock:

```bash
uv sync --locked --extra test
uv run --locked python -m pytest tests/ -q
```

> **Not yet on PyPI** — published packages land at launch. Until then, install from git:

```bash
pip install "git+https://github.com/TokenTrimmer/tokentrimmer.git#subdirectory=sdk-python"
```

## Try it in 30 seconds — no account, no provider key, $0

A `tt_test_*` **sandbox key** short-circuits inside the Gateway to a deterministic
synthetic response: it never contacts a provider, never verifies against a key
store, and costs nothing — ideal for wiring up an integration before you have an
account. Start a local Gateway (one `docker run`, no provider keys), then call it:

```bash
docker run -p 8080:8080 \
  -e TT_BIND_ADDR=0.0.0.0 -e TT_ALLOW_UNAUTHENTICATED_PUBLIC_BIND=1 \
  ghcr.io/tokentrimmer/tt-cli:latest
```

```python
from tokentrimmer import TokenTrimmer

# Sandbox: any tt_test_ token works — no account, no provider key, $0.
client = TokenTrimmer(api_key="tt_test_demo", base_url="http://localhost:8080/v1")

response = client.chat.completions.create(
    model="claude-sonnet-4-6",
    messages=[{"role": "user", "content": "Hello"}],
)
print(response.choices[0].message.content)
# → [sandbox] TokenTrimmer test response for model=claude-sonnet-4-6
print(f"cost ${response.tt.cost_usd:.4f}  cache {response.tt.cache}")
# → cost $0.0000  cache sandbox   (no provider was called)
```

## Real usage

Point at a live Gateway with a verified `tt_live_*` key for real routing, cost, and
cache metadata.

> **Hosted gateway launching soon** *(as of 2026-06-10)* — `TokenTrimmer(api_key=...)`
> defaults to `https://api.tokentrimmer.com`, which is not live yet. Self-host with
> Docker today and pass `base_url="http://localhost:8080/v1"` (see "Self-hosted
> Gateway" below).

```python
from tokentrimmer import TokenTrimmer

client = TokenTrimmer(api_key="tt_live_...")

response = client.chat.completions.create(
    model="claude-sonnet-4-6",        # any model your Gateway routes
    messages=[{"role": "user", "content": "Hello"}],
    tt_tag="feature=chat-support",     # optional: per-feature cost attribution
)

print(response.choices[0].message.content)

# Cost + cache metadata is on `.tt`:
print(f"cost  ${response.tt.cost_usd:.4f}")
print(f"saved ${response.tt.saved_usd:.4f}")
print(f"cache {response.tt.cache}        # hit-l1 | hit-l2 | miss | none")
print(f"trace {response.tt.trace_id}")
```

The class is a `openai.OpenAI` subclass — inherited methods (`embeddings`,
`models`, tools, vision) work unchanged. For TokenTrimmer's responder-scoped,
runtime-validated metadata extensions, use `client.gateway`:

```python
# Anonymous catalog metadata. No configured bearer is sent.
catalog = client.gateway.models()
print(catalog.data[0].tokentrimmer.max_input_tokens)

# Requires a tt_live_* key; evidence from one responding gateway process.
capabilities = client.gateway.capabilities()
print(capabilities.features.fusion.limits.member_models_max.value)

# Local responder preflight only: no provider request, tokenization, or
# credential-validity/readiness claim.
from tokentrimmer import RequestPreflightRequest
preflight = client.gateway.preflight(RequestPreflightRequest(
    schema_version=1,
    model="gpt-4o-mini",
    provider=None,
    required_capabilities=("text", "tools", "streaming"),
    declared_input_tokens=12_000,
    requested_max_output_tokens=4_096,
))
print(preflight.actions)
print(preflight.catalog_cost)

from tokentrimmer import RequestPreflightBatchRequest
batch = client.gateway.preflight_batch(RequestPreflightBatchRequest(
    schema_version=1,
    requests=(preflight.request,),
))
print(batch.documents)
```

All four operations return frozen typed dataclasses emitted from the
authoritative Rust product schemas, refuse redirects, apply a five-second
transport timeout plus wall-clock checks, stream under body ceilings (256 KiB
models / 64 KiB capabilities and preflight), require no-store/nosniff and exact
v1 semantics, and recompute the model snapshot digest. They do not prove
credentials, provider health, model/modality readiness, live pricing, request
acceptance, fleet convergence, a quote/reservation, enforced budget,
settlement, invoice, or later execution. Successful reason messages are
bounded responder copy; use stable reason codes and the actual request result
for machine decisions. The batch removes cross-process drift, but composite
stores and runtime configuration are still not one transaction.

## Streaming

Streaming works as usual; per-request cost is on the stream's `.tt` once it's drained (the Gateway's terminal usage frame is stripped, so chunk iteration is clean):

```python
stream = client.chat.completions.create(
    model="claude-sonnet-4-6",
    messages=[{"role": "user", "content": "Hello"}],
    stream=True,
)

for chunk in stream:
    print(chunk.choices[0].delta.content or "", end="")

# Cost is known once the stream is fully consumed (`stream.tt` stays None if the
# Gateway emitted no usage frame, e.g. self-hosted without pricing):
if stream.tt is not None:
    print(f"\ncost  ${stream.tt.cost_usd:.4f}")
    print(f"saved ${stream.tt.saved_usd:.4f}")
```

## Agent loop

For multi-step tool-using runs, `client.agent.run(...)` drives the Gateway's
server-side agent loop (`POST /v1/agent/runs`). The Gateway owns the loop
(down-routing, judge-gated summarize, substep cache); the SDK just executes any
**client** tool the run pauses on (via your `executor`) and resumes — until a
final answer. Aggregate cost spans every turn and is read from the run body
(`outcome.usage.cost_usd`), not response headers.

```python
def executor(name: str, arguments: str) -> str:
    # `arguments` is the raw JSON string the model produced; return the tool's
    # result as a string. Raising is fine — the error is fed back to the model.
    if name == "get_weather":
        return '{"temp_c": 21, "sky": "clear"}'
    return "{}"

outcome = client.agent.run(
    model="claude-sonnet-4-6",
    messages=[{"role": "user", "content": "What's the weather in Paris?"}],
    tools=[{
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Current weather for a city",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
        },
    }],
    executor=executor,
    max_turns=8,          # optional: server-side per-run turn cap
    tt_tag="feature=agent",
)

print(outcome.text)                       # final assistant answer
print(f"cost   ${outcome.usage.cost_usd:.4f}")
print(f"rounds {outcome.resume_rounds}")  # client-side tool_outputs resumes made
```

Paused/resumed transcripts remain in Redis for one hour. You can explicitly
export or erase that short-lived resume state without deleting durable
billing/audit metadata:

```python
transcript = client.agent.export_transcript(outcome.run.id)
client.agent.delete_transcript(outcome.run.id)  # idempotent
```

## LangChain cost callback + OpenTelemetry spans

When you point a LangChain `ChatOpenAI` at the Gateway (a plain `base_url` swap),
the `x-tokentrimmer-*` cost/savings headers are invisible to LangSmith / OTel.
`TokenTrimmerCostCallback` recovers them on every LLM finish, records them as
OpenTelemetry span attributes (the **same** `gen_ai.*` / `tokentrimmer.*` keys
the Gateway stamps on its own span — see `tokentrimmer.semconv`), and accumulates
a per-run cost / savings total with an optional budget.

Optional extras — the base package never depends on LangChain or OpenTelemetry:

```bash
pip install "tokentrimmer[langchain,otel]"
```

```python
from tokentrimmer.integrations.langchain import (
    TokenTrimmerCostCallback,
    make_gateway_chat_openai,
)

cb = TokenTrimmerCostCallback(max_cost_usd=0.50)   # optional per-run budget

# make_gateway_chat_openai sets the two easy-to-miss flags for you:
#   include_response_headers=True  → surfaces the x-tokentrimmer-* headers
#   use_responses_api=False        → the Gateway speaks Chat Completions, not Responses
llm = make_gateway_chat_openai(
    model="claude-haiku-4-5",
    api_key="tt_live_...",
    base_url="http://localhost:8080/v1",
    callbacks=[cb],
)

llm.invoke("Hello")

print(f"run cost  ${cb.total_cost_usd:.4f}")
print(f"run saved ${cb.total_saved_usd:.4f}")
```

The cost attributes land on whatever span is active when the LLM call finishes
(the callback calls `opentelemetry.trace.get_current_span()`), so any tracer you
already run picks them up. Without the `otel` extra the callback still tallies
`total_cost_usd` / `total_saved_usd` — span recording is a no-op.

**Budget stop hook.** With `max_cost_usd` set, the finish that tips the
accumulated cost past the cap raises `BudgetExceeded`; the handler sets
`raise_error = True`, so LangChain propagates it out of `invoke` / `stream`
instead of silently overspending.

If you build `ChatOpenAI` yourself, set `include_response_headers=True` (and
`use_responses_api=False`) — that is the hook the callback reads from
`response_metadata["headers"]`. Without it the callback degrades gracefully:
nothing is recorded and the chain runs normally.

### LangGraph

A LangGraph graph runs on LangChain's callback manager, so the **same**
`TokenTrimmerCostCallback` works unchanged — no LangGraph-specific class. Build
the node LLM with `make_gateway_chat_openai(...)` (so it emits the headers) and
pass the callback through `config={"callbacks": [cb]}` on `invoke` / `stream`;
LangGraph propagates it into every LLM call inside the graph's nodes.

```python
from langgraph.graph import StateGraph, START, END
from tokentrimmer.integrations.langchain import (
    TokenTrimmerCostCallback,
    make_gateway_chat_openai,
)

llm = make_gateway_chat_openai(model="claude-haiku-4-5", api_key="tt_live_...")

def call_model(state):
    return {"messages": state["messages"] + [llm.invoke(state["messages"])]}

graph = StateGraph(dict)
graph.add_node("model", call_model)
graph.add_edge(START, "model")
graph.add_edge("model", END)
app = graph.compile()

cb = TokenTrimmerCostCallback(max_cost_usd=0.50)
app.invoke({"messages": [("user", "Hello")]}, config={"callbacks": [cb]})

print(f"run cost  ${cb.total_cost_usd:.4f}")
print(f"run saved ${cb.total_saved_usd:.4f}")
```

Install with `pip install "tokentrimmer[langchain,langgraph,otel]"`.

## LiteLLM cost logger + OpenTelemetry spans

Using [LiteLLM](https://github.com/BerriAI/litellm) instead? It has its own
callback system, so there's a dedicated `TokenTrimmerLiteLLMLogger` (a LiteLLM
`CustomLogger`) that surfaces the `x-tokentrimmer-*` headers into the same
`gen_ai.*` / `tokentrimmer.*` span attributes and per-run totals.

```bash
pip install "tokentrimmer[litellm,otel]"
```

```python
import litellm
from tokentrimmer.integrations.litellm import TokenTrimmerLiteLLMLogger

# install() sets litellm.return_response_headers=True (required for LiteLLM to
# expose the gateway headers) and registers the logger on litellm.callbacks.
logger = TokenTrimmerLiteLLMLogger.install(max_cost_usd=0.50)

resp = litellm.completion(
    model="openai/claude-haiku-4-5",
    api_base="https://api.tokentrimmer.com/v1",
    api_key="tt_live_...",
    messages=[{"role": "user", "content": "Hello"}],
)
logger.raise_if_exceeded()          # enforce the budget at your checkpoint

print(f"run cost  ${logger.total_cost_usd:.4f}")
print(f"run saved ${logger.total_saved_usd:.4f}")
```

LiteLLM only exposes the raw gateway response headers when
`litellm.return_response_headers = True` — `install()` sets it for you (the
counterpart to `include_response_headers=True` on the LangChain side). The logger
accounts for synchronous OpenAI-compatible responses from LiteLLM's post-API
header metadata before `completion()` returns, then de-duplicates the later
background success event. It also reads `response._response_headers` /
`response._hidden_params["additional_headers"]` on ordinary success callbacks,
so a response without TokenTrimmer headers (self-hosted gateway, no pricing)
simply records nothing.

**Budget stop.** LiteLLM's callbacks are post-hoc *logging* events — LiteLLM
swallows exceptions raised inside them — so, unlike the LangChain callback's
inline `raise_error` stop, the budget is enforced at a **checkpoint**: crossing
`max_cost_usd` flips `logger.budget_exceeded` and `logger.raise_if_exceeded()`
raises `BudgetExceeded` (the same class both integrations share). Call it right
after each `completion` to cap a multi-call loop.

The immediate checkpoint is guaranteed for synchronous OpenAI-compatible
`completion()` responses that expose the gateway headers. Async completion uses
LiteLLM's asynchronous callback lifecycle; wait for that lifecycle before
reading totals or enforcing a checkpoint.

## Batch (50% cheaper, async)

The Gateway's `/v1/files` + `/v1/batches` endpoints are OpenAI-compatible, so the
**inherited** OpenAI `files` / `batches` resources route through TokenTrimmer
unchanged — no special methods, just the standard OpenAI batch flow. Provider
batch jobs are ~50% cheaper than synchronous calls, and TokenTrimmer's poll
worker books the realized savings server-side as each batch settles (visible in
your dashboard).

```python
import time

client = TokenTrimmer(api_key="tt_live_...")

# 1. Upload a JSONL of requests (one chat-completion request per line).
with open("requests.jsonl", "rb") as fh:
    f = client.files.create(file=fh, purpose="batch")

# 2. Create the batch.
batch = client.batches.create(
    input_file_id=f.id,
    endpoint="/v1/chat/completions",
    completion_window="24h",
)

# 3. Poll until it settles (the Gateway poll worker drives status + savings).
while batch.status not in ("completed", "failed", "expired", "cancelled"):
    time.sleep(30)
    batch = client.batches.retrieve(batch.id)

# 4. Download the results JSONL.
if batch.status == "completed" and batch.output_file_id:
    results = client.files.content(batch.output_file_id).read()
    print(results.decode())
```

Prefer no code? The [`tt` CLI](https://github.com/TokenTrimmer/tokentrimmer)
wraps the same flow: `tt batch submit requests.jsonl`, `tt batch get <id>`,
`tt batch download <output_file_id>`.

## Self-hosted Gateway

```python
client = TokenTrimmer(
    api_key="sk-...",                              # your provider key, pass-through
    base_url="http://localhost:8080/v1",           # your self-hosted Gateway
)
```

## Releasing

Maintainers: publishing to PyPI is tag-triggered and documented in
[`RELEASING.md`](RELEASING.md).

## License

Apache 2.0.
