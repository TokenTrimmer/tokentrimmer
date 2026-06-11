# TokenTrimmer proxy-overhead benchmark

A **reproducible** harness for measuring the latency a proxy/gateway *adds* on top
of an LLM provider — TokenTrimmer vs LiteLLM, with documented, non-fabricated
notes on Helicone and Bifrost.

> **Read this before quoting any number.** This benchmark measures **proxy
> overhead against a null upstream**, not provider latency, and not realized
> cost savings. The honest framing section below explains why that distinction
> matters and why TokenTrimmer's pitch leads with **savings**, not raw RPS.

---

## TL;DR — one command

```bash
# TokenTrimmer only (no external deps beyond `oha` + python3 + the `tt` binary):
./scripts/benchmarks/run-all.sh

# add the dockerized LiteLLM comparison:
./scripts/benchmarks/run-all.sh --with-litellm

# fast smoke (5s passes):
./scripts/benchmarks/run-all.sh --quick
```

Each run writes `scripts/benchmarks/results/results.csv` (gitignored — it is
fresh per run) with machine/date/version provenance beside it in
`results.csv.prov`. The **committed, self-documenting baseline** is
`scripts/benchmarks/results/baseline.csv` (labeled with the machine + date it
was measured on); `run-all.sh` never overwrites it.

Prerequisites: [`oha`](https://github.com/hatoo/oha) (`brew install oha` or
`cargo install oha`), `python3` (stdlib only — no pip installs), `curl`, and a
built `tt` binary (`cargo build --release -p tt-cli --bin tt`). LiteLLM also
needs Docker.

---

## (a) What this measures — and what it does NOT

This harness measures **proxy dispatch overhead**: the time a proxy spends
parsing the request, applying routing/middleware, serializing, and shuttling
bytes to and from the upstream — *isolated from real provider latency*.

It does **not** measure:

- **Provider latency.** A real `gpt-4o` call spends hundreds of ms to seconds
  inside OpenAI generating tokens. No proxy controls that, and it would swamp
  the microseconds of overhead we care about. So we point every proxy at a
  **null mock upstream** (see below) that replies in ~0 ms.
- **Realized cost savings.** TokenTrimmer's actual value — caching, cheaper-model
  routing, failover, guardrails — shows up as *dollars saved per workload*, which
  is customer- and traffic-specific and is measured by `tt plan` / Inspect, not
  by a latency load test. **This benchmark is an overhead floor, not a savings
  claim.** See honest framing (c).

### Why overhead-vs-null is the only fair head-to-head

If we benchmarked against a real provider, ~99% of every sample would be OpenAI
queue + generation time, identical for all proxies, and the proxy's own cost
would vanish into the noise. Isolating against a fixed ~0 ms upstream is the
only way to see the part each proxy is actually responsible for.

---

## (b) The mock-upstream design

[`mock-upstream.py`](./mock-upstream.py) is a ~200-line, **zero-dependency**
(Python stdlib only) OpenAI-compatible echo server. It implements just enough
wire surface to stand in for a provider:

| Route | Behavior |
|---|---|
| `GET /` and `GET /health` | liveness |
| `GET /v1/models` | fixed one-model list |
| `POST /v1/chat/completions` | fixed `{"content":"OK"}` completion (or SSE if `stream:true`) |
| `POST /v1/embeddings` | fixed 3-dim vector |

Properties that make it a clean isolation fixture:

- **No artificial latency by default** (`MOCK_DELAY_MS=0`) — so observed latency
  is the proxy's. Set `MOCK_DELAY_MS=5` to model a fast provider and watch each
  proxy's overhead become *additive* on top.
- **Accepts any API key and any model name.** The proxy in front owns auth; the
  mock never validates. Token counts in the reply are fixed so any downstream
  cost math is deterministic.
- **Loopback only**, threaded, stdlib `http.server`. It is a test fixture — never
  expose it to a network.

Both proxies under test sit in front of the *same* mock on the *same* machine
under the *same* oha load, so their rows are directly comparable.

### How TokenTrimmer is wired to the mock

The gateway runs with **no `DATABASE_URL`/`REDIS_URL`** (loopback bind, BYO-key
passthrough) and `TT_LOCAL_VLLM_URL=http://127.0.0.1:<mock>/v1`. That registers
the built-in `vllm` local-provider adapter (which sets `allow_local=true`, so a
localhost upstream is permitted), and a request for model `vllm/mock-model`
dispatches straight to the mock. The caller's bearer token is forwarded upstream
and ignored by the mock. This exercises the **full gateway request path** —
auth middleware, routing resolution, provider dispatch, response normalization —
minus persistence and minus a real provider.

---

## (c) Honest framing — read this

**TokenTrimmer's pitch is _savings_, not raw throughput.** This benchmark exists
to prove we are an *honest, low-overhead* path to those savings, not to win an
RPS contest.

- **On a cache miss, a gateway is by definition additive.** Every proxy —
  TokenTrimmer, LiteLLM, anything — adds its dispatch cost on top of the provider
  call. We say so plainly. The right question is "how small is that addition
  relative to the hundreds of ms a real model takes?" (answer: a rounding error),
  **and** "what does that thin layer buy you?" (answer: caching, routing, failover,
  guardrails, cost attribution).
- **Where TokenTrimmer actually wins on latency is the cache _hit_:** an L1/L2
  cache hit returns in single-digit ms without touching the provider at all —
  but that is a *savings* feature (you didn't pay for, or wait for, the model),
  not a dispatch-speed feature. The cache-hit path needs Redis; see the caveat
  below.
- **If our raw overhead is ever higher than a thinner proxy, that is an acceptable
  trade** for the safety/caching/routing the overhead pays for — and we will
  report the real number either way rather than hide it. The CSV is generated,
  never hand-edited.

So: lead with **cost reduction via caching + smart routing**, cite this benchmark
only to show the **overhead is negligible** next to a real provider call. Do not
market TokenTrimmer as "the fastest proxy."

### Cache-hit caveat

The default TokenTrimmer harness runs with **no Redis**, so the L1 cache is
disabled and the `chat-hit` pass is effectively a second cache-*miss* pass — it
shows the steady-state miss path, not a cache win. To measure the real cache-hit
path, run the gateway with a live `REDIS_URL` and re-issue an identical body; the
hit should return in single-digit ms without an upstream call. The harness labels
the row honestly (`tokentrimmer:chat-hit`) and this caveat documents it.

---

## (d) Competitors — how each is configured & pinned

### LiteLLM — runnable (`--with-litellm` / `litellm/run.sh`)

- **Image (pinned):** `ghcr.io/berriai/litellm:main-v1.61.20-stable` (override via
  `LITELLM_IMAGE`). Pin deliberately; never float to `:latest` or results stop
  being reproducible.
- **Config:** [`litellm/config.yaml`](./litellm/config.yaml) — a single
  `openai/`-style model whose `api_base` is the host mock (`host.docker.internal`),
  caching disabled (`cache: false`) and spend logs off, so we measure LiteLLM's
  dispatch path the same way we measure TokenTrimmer's miss path.
- **Why it's opt-in, not default:** it's a several-hundred-MB Docker pull and
  needs a running daemon — too heavy and network-dependent for the default run,
  and we never commit competitor numbers measured on someone else's hardware.
  You run it; the row is then yours and comparable.

### Helicone — documented limitation, NOT benchmarked

Helicone's proxy is **SaaS-only** for the hosted product. Benchmarking it would
mean either (1) hitting `oai.helicone.ai` over the public internet — which
measures *Helicone's network + cloud region*, not proxy overhead, and is not
apples-to-apples with a localhost proxy — or (2) standing up their self-host
stack, whose footprint and config drift make a fair, pinned local comparison
impractical to bundle. Rather than fabricate a number, `run-all.sh` writes a
**limitation row** (`# helicone,SaaS-only,…`) and we leave it at that. If you
want a Helicone data point, run their hosted proxy yourself against a real
provider and compare end-to-end latency — but know you're measuring a different
thing.

### Bifrost — documented limitation, NOT benchmarked

Bifrost is a separate gateway runtime with its own server + configuration model.
We do not bundle a pinned harness for it here; doing it justice means a dedicated,
version-locked setup we have not validated, and we will not ship a fabricated
row. `run-all.sh` writes a **limitation row** (`# bifrost,separate-runtime,…`).
Contributions adding a pinned, self-contained `bifrost/run.sh` that mirrors the
mock-upstream methodology are welcome.

> **Rule we hold ourselves to:** every competitor row in `results.csv` is either
> (a) produced locally by a script in this directory against the shared mock, or
> (b) a `#`-prefixed documented-limitation line. **No invented competitor
> numbers, ever.**

---

## (e) One-command reproduction

```bash
# from the repo root, with a release `tt` already built:
cargo build --release -p tt-cli --bin tt        # once
./scripts/benchmarks/run-all.sh --with-litellm  # TokenTrimmer + LiteLLM
cat scripts/benchmarks/results/results.csv      # this run's output (gitignored)
cat scripts/benchmarks/results/baseline.csv     # committed reference baseline
```

Tune the load with env vars (honored by every harness):

| Env | Default | Meaning |
|---|---|---|
| `BENCH_DURATION` | `20s` | oha `-z` per pass |
| `BENCH_CONCURRENCY` | `20` | oha `-c` |
| `MOCK_DELAY_MS` | `0` | artificial upstream think-time (model a fast provider) |
| `LITELLM_IMAGE` | pinned stable | LiteLLM image to pull |

### CSV schema

```
timestamp,label,requests,rps,p50_ms,p95_ms,p99_ms,success_rate
```

`label` is `<proxy>:<pass>` (e.g. `tokentrimmer:chat-miss`, `litellm:chat`).
Latencies are milliseconds (oha reports seconds; the harness converts).

---

## Relationship to the other perf assets

This harness complements, and does not replace:

- `scripts/latency-smoke.sh` / `scripts/load-test-gateway.sh` — release gates that
  probe a **live deployed** gateway (real provider in the path). This benchmark is
  for **local, comparative, null-upstream** overhead.
- `crates/core/benches/streaming.rs` — a criterion micro-bench for **per-SSE-chunk**
  overhead in-process. This benchmark is end-to-end over a real socket.

## CI posture — manual / nightly, never PR-blocking

This is **not** wired into the PR-gating CI path. Micro-overhead regressions
should not block merges, and the suite needs `oha` + (for LiteLLM) Docker. An
optional **non-blocking** scheduled workflow lives at
`.github/workflows/benchmark-nightly.yml` — it runs the TokenTrimmer harness on a
schedule and uploads the CSV as an artifact for trend-watching. It has no
`pull_request` trigger and gates nothing.
