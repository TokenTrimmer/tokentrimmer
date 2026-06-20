# tokentrimmer

Thin Python SDK over the official OpenAI client — routes through the TokenTrimmer Gateway, surfaces cost + cache metadata.

> **Not yet on PyPI** — published packages land at launch. Until then, install from git:

```bash
pip install "git+https://github.com/TokenTrimmer/tokentrimmer.git#subdirectory=sdk-python"
```

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
