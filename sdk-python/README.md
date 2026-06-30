# tokentrimmer

Thin Python SDK over the official OpenAI client — routes through the TokenTrimmer Gateway, surfaces cost + cache metadata.

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

The class is a `openai.OpenAI` subclass — every other method (`embeddings`, `models`, tools, vision) works unchanged.

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
