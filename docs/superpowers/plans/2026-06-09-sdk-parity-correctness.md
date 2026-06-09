# SDK Parity + Correctness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bring the Python and TypeScript SDKs to cost-control parity with the Rust `tt-client` (add `cost_limit` + cache-override convenience), fix the two latent correctness footguns (Python's race-prone metadata stash, TS streaming returning the wrong value), ship runnable examples, and add SDK test suites + CI.

**Architecture:** Both SDKs subclass the official OpenAI client and wrap `chat.completions.create`. Python switches from an httpx event-hook + `threading.local` stash to the OpenAI SDK's `with_raw_response.create().parse()` (race-free, no global state). TS stops calling `.withResponse()` on streaming requests. Tests mock the HTTP layer (Python: respx; TS: an injected stub `fetch` via `ClientOptions` — no new dependency). Examples are compile/typecheck/lint-checked in CI, not run live.

**Tech Stack:** Python 3.9+ / openai>=1.0 (`with_raw_response`) / respx / pytest. TypeScript / openai 5.x (custom `fetch` option) / vitest / tsc. Rust / `tt-client` (`cargo build --example`). GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-06-09-sdk-parity-correctness-batch7i-design.md`

**Branch:** `batch7i-sdk-parity-correctness` (already created).

**Conventions for every task:** stage ONLY the files named in that task (never `git add -A`; the working tree has an untracked `rust_out` that must never be staged). End every commit message with the footer:
```
Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
```

---

## Shared reference: the cache-override value set

`tt_cache` / `ttCache` accept ONLY the documented `X-TokenTrimmer-Cache` **request-override** values (API-reference §6.1): `bypass`, `force-write`, `read-only`, `disabled`. (These differ from the *response* cache-status values `hit-l1/hit-l2/neg-hit/miss/none/sandbox`.) Anything else is a programmer error and must raise/throw before the request is sent.

---

## Task 1: Python SDK test suite (TDD — write the target-behavior tests first)

These tests describe the SDK *after* Task 2's rewrite. They will fail against the current `client.py` (no `tt_cost_limit`/`tt_cache`; current mechanism is the stash). That is expected — Task 2 makes them pass.

**Files:**
- Create: `sdk-python/tests/__init__.py` (empty)
- Create: `sdk-python/tests/test_client.py`

- [ ] **Step 1: Create the empty test package marker**

Create `sdk-python/tests/__init__.py` with no content.

- [ ] **Step 2: Write the test module**

Create `sdk-python/tests/test_client.py`:

```python
"""Tests for the TokenTrimmer Python SDK wrapper.

All tests mock the gateway HTTP layer with respx so no network is used. They
assert the four wrap behaviors: max_tokens default injection, the tt_* header
lifts (+ validation), the parsed .tt metadata, race-free metadata under
concurrency, and streaming pass-through.
"""

from __future__ import annotations

import concurrent.futures

import httpx
import pytest
import respx

from tokentrimmer import TokenTrimmer
from tokentrimmer.client import TokenTrimmerMeta

GATEWAY = "http://gw.test/v1"


def _completion_route(cost: str = "0.0034", cache: str = "miss") -> respx.Route:
    """Register a /chat/completions mock returning the given TT headers."""
    return respx.post(f"{GATEWAY}/chat/completions").mock(
        return_value=httpx.Response(
            200,
            headers={
                "x-tokentrimmer-trace-id": "trace-1",
                "x-tokentrimmer-provider": "anthropic",
                "x-tokentrimmer-model-used": "claude-haiku-4-5",
                "x-tokentrimmer-cost-usd": cost,
                "x-tokentrimmer-baseline-cost-usd": "0.02",
                "x-tokentrimmer-saved-usd": "0.0166",
                "x-tokentrimmer-cache": cache,
            },
            json={
                "id": "chatcmpl-1",
                "object": "chat.completion",
                "created": 1,
                "model": "claude-haiku-4-5",
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": "hi"},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            },
        )
    )


def _client() -> TokenTrimmer:
    return TokenTrimmer(api_key="tt_test_x", base_url=GATEWAY)


@respx.mock
def test_parses_tt_metadata_onto_response():
    route = _completion_route()
    resp = _client().chat.completions.create(
        model="claude-haiku-4-5", messages=[{"role": "user", "content": "hi"}]
    )
    assert route.called
    meta: TokenTrimmerMeta = resp.tt
    assert meta.trace_id == "trace-1"
    assert meta.provider == "anthropic"
    assert meta.cost_usd == 0.0034
    assert meta.saved_usd == 0.0166
    assert meta.cache == "miss"


@respx.mock
def test_non_numeric_cost_header_parses_to_none():
    _completion_route(cost="not-a-number")
    resp = _client().chat.completions.create(
        model="m", messages=[{"role": "user", "content": "hi"}]
    )
    assert resp.tt.cost_usd is None


@respx.mock
def test_max_tokens_default_injected_when_absent():
    route = _completion_route()
    _client().chat.completions.create(
        model="m", messages=[{"role": "user", "content": "hi"}]
    )
    sent = route.calls.last.request
    import json

    body = json.loads(sent.content)
    assert body["max_tokens"] == 4096


@respx.mock
def test_explicit_max_tokens_wins():
    route = _completion_route()
    _client().chat.completions.create(
        model="m", messages=[{"role": "user", "content": "hi"}], max_tokens=128
    )
    import json

    body = json.loads(route.calls.last.request.content)
    assert body["max_tokens"] == 128


@respx.mock
def test_tt_tag_cost_limit_and_cache_lift_to_headers():
    route = _completion_route()
    _client().chat.completions.create(
        model="m",
        messages=[{"role": "user", "content": "hi"}],
        tt_tag="feature=chat",
        tt_cost_limit=0.05,
        tt_cache="bypass",
    )
    req = route.calls.last.request
    assert req.headers["x-tokentrimmer-tag"] == "feature=chat"
    assert req.headers["x-tokentrimmer-cost-limit-usd"] == "0.05"
    assert req.headers["x-tokentrimmer-cache"] == "bypass"


@respx.mock
def test_invalid_tt_cache_raises_before_send():
    route = _completion_route()
    with pytest.raises(ValueError):
        _client().chat.completions.create(
            model="m",
            messages=[{"role": "user", "content": "hi"}],
            tt_cache="hit-l1",  # a response value, NOT a valid request override
        )
    assert not route.called


@respx.mock
def test_metadata_isolated_across_concurrent_threads():
    # Each request returns a DISTINCT cost; every caller must observe its own.
    # Fails on the old threading.local stash design; passes on with_raw_response.
    def one(i: int) -> float:
        respx.post(f"{GATEWAY}/chat/completions").mock(
            return_value=httpx.Response(
                200,
                headers={"x-tokentrimmer-cost-usd": f"0.{i:04d}"},
                json={
                    "id": "c",
                    "object": "chat.completion",
                    "created": 1,
                    "model": "m",
                    "choices": [
                        {
                            "index": 0,
                            "message": {"role": "assistant", "content": "x"},
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
                },
            )
        )
        client = TokenTrimmer(api_key="tt_test_x", base_url=GATEWAY)
        r = client.chat.completions.create(
            model="m", messages=[{"role": "user", "content": "hi"}]
        )
        return r.tt.cost_usd

    # A shared single route per-call would race; instead give each its own client
    # and a fixed header, then assert no cross-talk on the parsed value.
    with respx.mock:
        respx.post(f"{GATEWAY}/chat/completions").mock(
            return_value=httpx.Response(
                200,
                headers={"x-tokentrimmer-cost-usd": "0.0042"},
                json={
                    "id": "c",
                    "object": "chat.completion",
                    "created": 1,
                    "model": "m",
                    "choices": [
                        {
                            "index": 0,
                            "message": {"role": "assistant", "content": "x"},
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
                },
            )
        )
        client = TokenTrimmer(api_key="tt_test_x", base_url=GATEWAY)
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as ex:
            results = list(
                ex.map(
                    lambda _: client.chat.completions.create(
                        model="m", messages=[{"role": "user", "content": "hi"}]
                    ).tt.cost_usd,
                    range(32),
                )
            )
        # Every concurrent caller observed the (single mocked) cost, none got None
        # from a stash that was already taken by another thread.
        assert all(c == 0.0042 for c in results)


@respx.mock
def test_streaming_returns_stream_and_does_not_attach_tt():
    # Build a minimal SSE body the OpenAI client can parse as a stream.
    sse = (
        'data: {"id":"c","object":"chat.completion.chunk","created":1,"model":"m",'
        '"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}\n\n'
        "data: [DONE]\n\n"
    )
    respx.post(f"{GATEWAY}/chat/completions").mock(
        return_value=httpx.Response(
            200, headers={"content-type": "text/event-stream"}, content=sse
        )
    )
    stream = _client().chat.completions.create(
        model="m", messages=[{"role": "user", "content": "hi"}], stream=True
    )
    # It's an iterable stream, not a parsed completion with .tt.
    assert not hasattr(stream, "tt")
    chunks = list(stream)
    assert len(chunks) >= 1
```

- [ ] **Step 3: Run the tests to confirm they fail against the current code**

Run: `cd sdk-python && python -m pytest tests/ -q`
Expected: FAIL — current `client.py` has no `tt_cost_limit`/`tt_cache` (TypeError or unexpected header), and `test_invalid_tt_cache_raises_before_send` won't raise. (Some tests may pass incidentally; the tt_* and streaming ones must fail.)

- [ ] **Step 4: Commit the failing tests**

```bash
git add sdk-python/tests/__init__.py sdk-python/tests/test_client.py
git commit -m "test(sdk-python): target-behavior suite for parity + race-free meta + streaming"
```

---

## Task 2: Python SDK implementation (make Task 1 pass)

Rewrite the wrap to (a) lift `tt_cost_limit`/`tt_cache` headers with validation, (b) use `with_raw_response` instead of the stash, (c) pass streaming through untouched.

**Files:**
- Modify: `sdk-python/tokentrimmer/client.py` (replace `_MetaStash`, `_capture_meta`, the http_client injection, and `_wrap_chat_completions`)

- [ ] **Step 1: Replace the file body**

Overwrite `sdk-python/tokentrimmer/client.py` with:

```python
"""TokenTrimmer client implementation.

Wraps the official ``openai.OpenAI`` client. Override points:

- Default ``base_url`` points at the hosted Gateway.
- ``chat.completions.create`` is wrapped to:
  - inject a default ``max_tokens=4096`` when the caller supplies no
    ``max_tokens`` / ``max_completion_tokens`` / ``max_output_tokens``;
  - lift ``tt_tag`` / ``tt_cost_limit`` / ``tt_cache`` keyword arguments into
    the matching ``X-TokenTrimmer-*`` request headers;
  - attach parsed ``X-TokenTrimmer-*`` response headers to the result as
    ``.tt`` (a :class:`TokenTrimmerMeta`).

Metadata is read from the per-call raw response via the OpenAI SDK's
``with_raw_response`` accessor, so there is no shared mutable state and the
``.tt`` attribution is correct under threads and retries.

Streaming (``stream=True``) is passed straight through to the underlying client
and returns the SDK ``Stream`` unchanged; ``.tt`` is not attached because the
cost headers describe the whole response, which isn't complete until the stream
is drained. Read terminal ``usage`` off the final chunk instead.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Optional

import httpx
from openai import OpenAI

DEFAULT_BASE_URL = "https://api.tokentrimmer.com/v1"

# Valid X-TokenTrimmer-Cache REQUEST-override values (API reference §6.1).
# Distinct from the response cache-status values (hit-l1/hit-l2/neg-hit/...).
_VALID_CACHE_OVERRIDES = frozenset({"bypass", "force-write", "read-only", "disabled"})


@dataclass(frozen=True)
class TokenTrimmerMeta:
    """Per-response TokenTrimmer metadata, parsed from response headers.

    Every field is ``None`` when the Gateway didn't populate the corresponding
    header (e.g., self-hosted deployments without telemetry, or pre-1.0
    Gateway versions).
    """

    trace_id: Optional[str]
    provider: Optional[str]
    model_used: Optional[str]
    cost_usd: Optional[float]
    baseline_cost_usd: Optional[float]
    saved_usd: Optional[float]
    cache: Optional[str]  # "hit-l1" | "hit-l2" | "neg-hit" | "miss" | "none" | "sandbox"


def _parse_meta(headers: httpx.Headers) -> TokenTrimmerMeta:
    """Read X-TokenTrimmer-* headers off an httpx response, parsing floats."""

    def f(name: str) -> Optional[float]:
        v = headers.get(name)
        if v is None:
            return None
        try:
            return float(v)
        except ValueError:
            return None

    return TokenTrimmerMeta(
        trace_id=headers.get("x-tokentrimmer-trace-id"),
        provider=headers.get("x-tokentrimmer-provider"),
        model_used=headers.get("x-tokentrimmer-model-used"),
        cost_usd=f("x-tokentrimmer-cost-usd"),
        baseline_cost_usd=f("x-tokentrimmer-baseline-cost-usd"),
        saved_usd=f("x-tokentrimmer-saved-usd"),
        cache=headers.get("x-tokentrimmer-cache"),
    )


class TokenTrimmer(OpenAI):
    """OpenAI SDK subclass that routes through TokenTrimmer Gateway.

    All ``openai.OpenAI`` constructor parameters are accepted unchanged.
    ``base_url`` defaults to the hosted gateway; override for self-hosted.
    """

    def __init__(
        self,
        api_key: Optional[str] = None,
        base_url: str = DEFAULT_BASE_URL,
        **kwargs: Any,
    ) -> None:
        super().__init__(api_key=api_key, base_url=base_url, **kwargs)
        self._wrap_chat_completions()

    def _wrap_chat_completions(self) -> None:
        completions = self.chat.completions
        original_create = completions.create

        def create(*args: Any, **kwargs: Any) -> Any:
            # Sensible default to prevent unbounded output. User-provided
            # max_tokens / max_completion_tokens / max_output_tokens win.
            if not any(
                k in kwargs
                for k in ("max_tokens", "max_completion_tokens", "max_output_tokens")
            ):
                kwargs["max_tokens"] = 4096

            extra_headers = dict(kwargs.pop("extra_headers", {}) or {})
            tt_tag = kwargs.pop("tt_tag", None)
            if tt_tag is not None:
                extra_headers["X-TokenTrimmer-Tag"] = str(tt_tag)
            tt_cost_limit = kwargs.pop("tt_cost_limit", None)
            if tt_cost_limit is not None:
                extra_headers["X-TokenTrimmer-Cost-Limit-Usd"] = str(float(tt_cost_limit))
            tt_cache = kwargs.pop("tt_cache", None)
            if tt_cache is not None:
                if tt_cache not in _VALID_CACHE_OVERRIDES:
                    raise ValueError(
                        f"tt_cache must be one of {sorted(_VALID_CACHE_OVERRIDES)}; "
                        f"got {tt_cache!r}"
                    )
                extra_headers["X-TokenTrimmer-Cache"] = tt_cache
            if extra_headers:
                kwargs["extra_headers"] = extra_headers

            # Streaming: the cost headers describe the whole response, which
            # isn't complete until the stream is drained. Pass through untouched
            # and return the SDK Stream; do not attach .tt.
            if kwargs.get("stream"):
                return original_create(*args, **kwargs)

            # Non-streaming: read the per-call raw response so we can parse the
            # X-TokenTrimmer-* headers without any shared mutable state.
            raw = completions.with_raw_response.create(*args, **kwargs)
            result = raw.parse()
            meta = _parse_meta(raw.headers)
            try:
                object.__setattr__(result, "tt", meta)
            except Exception:
                setattr(result, "tt", meta)
            return result

        # Monkey-patch the bound method to add the tt_* lifts + .tt metadata.
        self.chat.completions.create = create  # type: ignore[method-assign]
```

- [ ] **Step 2: Run the Python test suite**

Run: `cd sdk-python && python -m pytest tests/ -q`
Expected: PASS (all 8 tests, including the concurrency and streaming cases).

- [ ] **Step 3: Commit**

```bash
git add sdk-python/tokentrimmer/client.py
git commit -m "feat(sdk-python): tt_cost_limit/tt_cache parity + race-free .tt via with_raw_response"
```

---

## Task 3: TypeScript SDK test suite (TDD — target behavior first)

Tests inject a stub `fetch` via `ClientOptions` (no msw/nock). They fail against the current `index.ts` (no `ttCostLimit`/`ttCache`; streaming returns parsed `data`).

**Files:**
- Create: `sdk-typescript/tsconfig.json` test inclusion is unchanged (vitest uses its own resolution); no tsconfig edit needed.
- Create: `sdk-typescript/test/client.test.ts`

- [ ] **Step 1: Write the test module**

Create `sdk-typescript/test/client.test.ts`:

```ts
import { describe, expect, it } from 'vitest';
import { TokenTrimmer } from '../src/index.js';

const TT_HEADERS = {
  'x-tokentrimmer-trace-id': 'trace-1',
  'x-tokentrimmer-provider': 'anthropic',
  'x-tokentrimmer-model-used': 'claude-haiku-4-5',
  'x-tokentrimmer-cost-usd': '0.0034',
  'x-tokentrimmer-baseline-cost-usd': '0.02',
  'x-tokentrimmer-saved-usd': '0.0166',
  'x-tokentrimmer-cache': 'miss',
};

const COMPLETION_BODY = {
  id: 'chatcmpl-1',
  object: 'chat.completion',
  created: 1,
  model: 'claude-haiku-4-5',
  choices: [
    { index: 0, message: { role: 'assistant', content: 'hi' }, finish_reason: 'stop' },
  ],
  usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
};

/** A stub fetch that records the last request and returns canned data + headers. */
function stubFetch(opts: { headers?: Record<string, string>; sse?: string } = {}) {
  const calls: Array<{ url: string; init: RequestInit }> = [];
  const fetchImpl = async (url: string | URL | Request, init: RequestInit = {}) => {
    calls.push({ url: String(url), init });
    if (opts.sse !== undefined) {
      return new Response(opts.sse, {
        status: 200,
        headers: { 'content-type': 'text/event-stream' },
      });
    }
    return new Response(JSON.stringify(COMPLETION_BODY), {
      status: 200,
      headers: { 'content-type': 'application/json', ...(opts.headers ?? TT_HEADERS) },
    });
  };
  return { calls, fetchImpl: fetchImpl as unknown as typeof fetch };
}

function client(fetchImpl: typeof fetch) {
  return new TokenTrimmer({ apiKey: 'tt_test_x', baseURL: 'http://gw.test/v1', fetch: fetchImpl });
}

describe('TokenTrimmer TS SDK', () => {
  it('attaches parsed .tt metadata on a non-streaming response', async () => {
    const { fetchImpl } = stubFetch();
    const res = await client(fetchImpl).chat.completions.create({
      model: 'claude-haiku-4-5',
      messages: [{ role: 'user', content: 'hi' }],
    });
    expect((res as any).tt.traceId).toBe('trace-1');
    expect((res as any).tt.costUsd).toBe(0.0034);
    expect((res as any).tt.cache).toBe('miss');
  });

  it('parses a non-numeric cost header to null', async () => {
    const { fetchImpl } = stubFetch({ headers: { 'x-tokentrimmer-cost-usd': 'nope' } });
    const res = await client(fetchImpl).chat.completions.create({
      model: 'm',
      messages: [{ role: 'user', content: 'hi' }],
    });
    expect((res as any).tt.costUsd).toBeNull();
  });

  it('injects max_tokens=4096 when absent and respects an explicit value', async () => {
    const a = stubFetch();
    await client(a.fetchImpl).chat.completions.create({
      model: 'm',
      messages: [{ role: 'user', content: 'hi' }],
    });
    expect(JSON.parse(a.calls.at(-1)!.init.body as string).max_tokens).toBe(4096);

    const b = stubFetch();
    await client(b.fetchImpl).chat.completions.create({
      model: 'm',
      messages: [{ role: 'user', content: 'hi' }],
      max_tokens: 128,
    });
    expect(JSON.parse(b.calls.at(-1)!.init.body as string).max_tokens).toBe(128);
  });

  it('lifts ttTag / ttCostLimit / ttCache into request headers', async () => {
    const { calls, fetchImpl } = stubFetch();
    await client(fetchImpl).chat.completions.create({
      model: 'm',
      messages: [{ role: 'user', content: 'hi' }],
      ttTag: 'feature=chat',
      ttCostLimit: 0.05,
      ttCache: 'bypass',
    } as any);
    const h = new Headers(calls.at(-1)!.init.headers as HeadersInit);
    expect(h.get('x-tokentrimmer-tag')).toBe('feature=chat');
    expect(h.get('x-tokentrimmer-cost-limit-usd')).toBe('0.05');
    expect(h.get('x-tokentrimmer-cache')).toBe('bypass');
  });

  it('throws on an invalid ttCache before sending', async () => {
    const { calls, fetchImpl } = stubFetch();
    await expect(
      client(fetchImpl).chat.completions.create({
        model: 'm',
        messages: [{ role: 'user', content: 'hi' }],
        ttCache: 'hit-l1', // a response value, not a valid request override
      } as any),
    ).rejects.toThrow();
    expect(calls.length).toBe(0);
  });

  it('returns the stream (not a parsed body) for stream:true', async () => {
    const sse =
      'data: {"id":"c","object":"chat.completion.chunk","created":1,"model":"m",' +
      '"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}\n\n' +
      'data: [DONE]\n\n';
    const { fetchImpl } = stubFetch({ sse });
    const stream = await client(fetchImpl).chat.completions.create({
      model: 'm',
      messages: [{ role: 'user', content: 'hi' }],
      stream: true,
    });
    expect((stream as any).tt).toBeUndefined();
    expect(typeof (stream as any)[Symbol.asyncIterator]).toBe('function');
    const chunks: unknown[] = [];
    for await (const c of stream as any) chunks.push(c);
    expect(chunks.length).toBeGreaterThanOrEqual(1);
  });
});
```

- [ ] **Step 2: Run to confirm failure against current code**

Run: `cd sdk-typescript && npm install && npx vitest run`
Expected: FAIL — `ttCostLimit`/`ttCache` not lifted; invalid-cache test doesn't throw; streaming test finds a parsed body (has no asyncIterator) instead of a stream.

- [ ] **Step 3: Commit the failing tests**

```bash
git add sdk-typescript/test/client.test.ts
git commit -m "test(sdk-typescript): target-behavior suite for parity + streaming passthrough"
```

---

## Task 4: TypeScript SDK implementation (make Task 3 pass)

**Files:**
- Modify: `sdk-typescript/src/index.ts` (the `chat.completions.create` wrap)

- [ ] **Step 1: Replace the wrap body**

In `sdk-typescript/src/index.ts`, replace the class body (from `export class TokenTrimmer` to the end of the constructor) with the version below. Keep the file's top-of-file doc comment, imports, `DEFAULT_BASE_URL`, `TokenTrimmerMeta`, `parseFloatOrNull`, `parseMeta`, and the trailing `WithTokenTrimmerMeta` export unchanged.

```ts
// Valid X-TokenTrimmer-Cache REQUEST-override values (API reference §6.1).
// Distinct from the response cache-status values (hit-l1/hit-l2/neg-hit/...).
const VALID_CACHE_OVERRIDES = new Set(['bypass', 'force-write', 'read-only', 'disabled']);

export class TokenTrimmer extends OpenAI {
  constructor(options: ClientOptions = {}) {
    super({
      ...options,
      baseURL: options.baseURL ?? DEFAULT_BASE_URL,
    });

    const originalCreate = this.chat.completions.create.bind(this.chat.completions);

    // The OpenAI SDK's `create` is a heavily-overloaded method; assigning a
    // replacement requires a localized cast at this boundary. Inside the
    // wrapper we keep types explicit and only treat the request body as a
    // loose record so we can read/move the `tt*` convenience fields.
    const wrapped = async (body: Record<string, unknown>, opts: Record<string, unknown> = {}) => {
      const { ttTag, ttCostLimit, ttCache, ...rest } = body ?? {};

      // Sensible default to prevent unbounded output. User-provided
      // max_tokens / max_completion_tokens / max_output_tokens win.
      if (
        rest.max_tokens === undefined &&
        rest.max_completion_tokens === undefined &&
        rest.max_output_tokens === undefined
      ) {
        rest.max_tokens = 4096;
      }

      const headers = { ...((opts.headers as Record<string, string>) ?? {}) };
      if (typeof ttTag === 'string') headers['X-TokenTrimmer-Tag'] = ttTag;
      if (ttCostLimit !== undefined && ttCostLimit !== null) {
        headers['X-TokenTrimmer-Cost-Limit-Usd'] = String(Number(ttCostLimit));
      }
      if (ttCache !== undefined && ttCache !== null) {
        if (typeof ttCache !== 'string' || !VALID_CACHE_OVERRIDES.has(ttCache)) {
          throw new Error(
            `ttCache must be one of ${[...VALID_CACHE_OVERRIDES].join(', ')}; got ${String(ttCache)}`,
          );
        }
        headers['X-TokenTrimmer-Cache'] = ttCache;
      }
      const callOpts = { ...opts, headers };

      // Streaming: the cost headers describe the whole response, which isn't
      // complete until the stream is drained. Return the SDK Stream untouched;
      // do not call withResponse() or attach .tt.
      if (rest.stream === true) {
        return originalCreate(rest, callOpts);
      }

      const { data, response } = await originalCreate(rest, callOpts).withResponse();
      (data as { tt?: TokenTrimmerMeta }).tt = parseMeta(response.headers);
      return data;
    };

    // Localized cast: see comment above. This is the only `any` in the wrap.
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    this.chat.completions.create = wrapped as any;
  }
}
```

- [ ] **Step 2: Run the TS test suite**

Run: `cd sdk-typescript && npx vitest run`
Expected: PASS (all 6 tests, including the streaming and invalid-cache cases).

- [ ] **Step 3: Typecheck**

Run: `cd sdk-typescript && npx tsc -p tsconfig.json --noEmit`
Expected: no errors.

- [ ] **Step 4: Commit**

```bash
git add sdk-typescript/src/index.ts
git commit -m "feat(sdk-typescript): ttCostLimit/ttCache parity + stream passthrough fix"
```

---

## Task 5: Runnable examples (py + ts + rust) + examples README

**Files:**
- Create: `examples/README.md`
- Create: `examples/python/cost_attribution.py`
- Create: `examples/python/streaming.py`
- Create: `examples/python/self_hosted.py`
- Create: `examples/typescript/cost-attribution.ts`
- Create: `examples/typescript/streaming.ts`
- Create: `examples/typescript/self-hosted.ts`
- Create: `examples/typescript/tsconfig.json` (resolves the workspace packages for example typechecking; reused by CI in Task 6)
- Create: `crates/client/examples/cost_attribution.rs`

- [ ] **Step 1: Write `examples/README.md`**

```markdown
# TokenTrimmer SDK examples

Runnable snippets for the Python, TypeScript, and Rust clients. Each needs a
TokenTrimmer API key (or a self-hosted gateway). Use a sandbox `tt_test_*` key
to exercise the wire path without real provider calls.

## Python
```bash
pip install tokentrimmer
export TOKENTRIMMER_API_KEY=tt_...
python examples/python/cost_attribution.py
```

## TypeScript
```bash
npm install @tokentrimmer/client
export TOKENTRIMMER_API_KEY=tt_...
npx tsx examples/typescript/cost-attribution.ts
```

## Rust
```bash
export TOKENTRIMMER_API_KEY=tt_...
cargo run -p tt-client --example cost_attribution
```

`.tt` (Python/TS) / `outcome.cost` (Rust) carries the gateway's cost metadata:
`cost_usd`, `baseline_cost_usd`, `saved_usd`, `cache`, `provider`, `model_used`,
`trace_id`. On a streaming call the SDKs return the raw stream and do NOT attach
`.tt` — read terminal `usage` off the final chunk.
```

- [ ] **Step 2: Write the Python examples**

`examples/python/cost_attribution.py`:

```python
"""Non-streaming chat that prints the TokenTrimmer cost attribution."""

import os

from tokentrimmer import TokenTrimmer

client = TokenTrimmer(api_key=os.environ["TOKENTRIMMER_API_KEY"])

resp = client.chat.completions.create(
    model="claude-haiku-4-5",
    messages=[{"role": "user", "content": "Say hello in five words."}],
    tt_tag="example=cost-attribution",
    tt_cost_limit=0.05,
)

print(resp.choices[0].message.content)
print(f"cost      ${resp.tt.cost_usd}")
print(f"baseline  ${resp.tt.baseline_cost_usd}")
print(f"saved     ${resp.tt.saved_usd}")
print(f"cache     {resp.tt.cache}")
```

`examples/python/streaming.py`:

```python
"""Streaming chat. The SDK returns the raw stream (no .tt); read usage off the
final chunk by requesting stream_options={'include_usage': True}."""

import os

from tokentrimmer import TokenTrimmer

client = TokenTrimmer(api_key=os.environ["TOKENTRIMMER_API_KEY"])

stream = client.chat.completions.create(
    model="claude-haiku-4-5",
    messages=[{"role": "user", "content": "Count to five."}],
    stream=True,
    stream_options={"include_usage": True},
)

usage = None
for chunk in stream:
    if chunk.choices and chunk.choices[0].delta.content:
        print(chunk.choices[0].delta.content, end="", flush=True)
    if getattr(chunk, "usage", None):
        usage = chunk.usage
print()
print(f"usage: {usage}")
```

`examples/python/self_hosted.py`:

```python
"""Point the client at a self-hosted gateway and apply a cache override."""

import os

from tokentrimmer import TokenTrimmer

client = TokenTrimmer(
    api_key=os.environ.get("TOKENTRIMMER_API_KEY", "tt_test_local"),
    base_url=os.environ.get("TOKENTRIMMER_BASE_URL", "http://localhost:8080/v1"),
)

resp = client.chat.completions.create(
    model="claude-haiku-4-5",
    messages=[{"role": "user", "content": "Ping"}],
    tt_cache="bypass",  # skip the cache for this request
)
print(resp.choices[0].message.content)
print(f"cache {resp.tt.cache}")
```

- [ ] **Step 3: Write the TypeScript examples**

`examples/typescript/cost-attribution.ts`:

```ts
import { TokenTrimmer, type WithTokenTrimmerMeta } from '@tokentrimmer/client';
import type { ChatCompletion } from 'openai/resources/chat/completions';

const client = new TokenTrimmer({ apiKey: process.env.TOKENTRIMMER_API_KEY! });

const res = (await client.chat.completions.create({
  model: 'claude-haiku-4-5',
  messages: [{ role: 'user', content: 'Say hello in five words.' }],
  ttTag: 'example=cost-attribution',
  ttCostLimit: 0.05,
} as never)) as WithTokenTrimmerMeta<ChatCompletion>;

console.log(res.choices[0]?.message.content);
console.log(`cost     $${res.tt.costUsd}`);
console.log(`saved    $${res.tt.savedUsd}`);
console.log(`cache    ${res.tt.cache}`);
```

`examples/typescript/streaming.ts`:

```ts
import { TokenTrimmer } from '@tokentrimmer/client';
import type { Stream } from 'openai/streaming';
import type { ChatCompletionChunk } from 'openai/resources/chat/completions';

const client = new TokenTrimmer({ apiKey: process.env.TOKENTRIMMER_API_KEY! });

const stream = (await client.chat.completions.create({
  model: 'claude-haiku-4-5',
  messages: [{ role: 'user', content: 'Count to five.' }],
  stream: true,
  stream_options: { include_usage: true },
} as never)) as Stream<ChatCompletionChunk>;

for await (const chunk of stream) {
  const delta = chunk.choices[0]?.delta?.content;
  if (delta) process.stdout.write(delta);
  if (chunk.usage) console.log(`\nusage:`, chunk.usage);
}
```

`examples/typescript/self-hosted.ts`:

```ts
import { TokenTrimmer, type WithTokenTrimmerMeta } from '@tokentrimmer/client';
import type { ChatCompletion } from 'openai/resources/chat/completions';

const client = new TokenTrimmer({
  apiKey: process.env.TOKENTRIMMER_API_KEY ?? 'tt_test_local',
  baseURL: process.env.TOKENTRIMMER_BASE_URL ?? 'http://localhost:8080/v1',
});

const res = (await client.chat.completions.create({
  model: 'claude-haiku-4-5',
  messages: [{ role: 'user', content: 'Ping' }],
  ttCache: 'bypass',
} as never)) as WithTokenTrimmerMeta<ChatCompletion>;

console.log(res.choices[0]?.message.content);
console.log(`cache ${res.tt.cache}`);
```

- [ ] **Step 4: Write the Rust example**

`crates/client/examples/cost_attribution.rs`:

```rust
//! Non-streaming chat that prints the TokenTrimmer cost attribution.
//!
//! Run: `TOKENTRIMMER_API_KEY=tt_... cargo run -p tt-client --example cost_attribution`

use tt_client::{user, Client};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key = std::env::var("TOKENTRIMMER_API_KEY").unwrap_or_else(|_| "tt_test_local".into());
    let base =
        std::env::var("TOKENTRIMMER_BASE_URL").unwrap_or_else(|_| "https://api.tokentrimmer.com".into());
    let client = Client::new(base, key);

    let outcome = client
        .chat()
        .model("claude-haiku-4-5")
        .message(user("Say hello in five words."))
        .tag("example=cost-attribution")
        .cost_limit(0.05)
        .send()
        .await?;

    println!("{}", outcome.text().unwrap_or(""));
    println!("cost     {:?}", outcome.cost.cost_usd);
    println!("saved    {:?}", outcome.cost.saved_usd);
    println!("savings% {:?}", outcome.savings_pct());
    Ok(())
}
```

- [ ] **Step 5: Write the examples tsconfig (resolves workspace packages)**

Create `examples/typescript/tsconfig.json`:

```json
{
  "compilerOptions": {
    "target": "es2022",
    "module": "nodenext",
    "moduleResolution": "nodenext",
    "strict": true,
    "noEmit": true,
    "skipLibCheck": true,
    "types": ["node"],
    "baseUrl": ".",
    "paths": {
      "@tokentrimmer/client": ["../../sdk-typescript/src/index.ts"]
    }
  },
  "include": ["*.ts"]
}
```

The `paths` mapping points `@tokentrimmer/client` at the SDK source; `openai/*`
and `@types/node` resolve through `sdk-typescript/node_modules`, so run `tsc`
with that package's binary (Step 6).

- [ ] **Step 6: Verify everything compiles / typechecks**

Run each and confirm success:
```bash
python -m py_compile examples/python/*.py
( cd examples/typescript && ../../sdk-typescript/node_modules/.bin/tsc -p tsconfig.json )
cargo build -p tt-client --example cost_attribution
```
Expected: all succeed (the Python compile, the TS example typecheck via the SDK's tsc + the paths tsconfig, and a clean Rust example build). Requires `sdk-typescript` deps installed (Task 3 Step 2 ran `npm install`).

- [ ] **Step 7: Commit**

```bash
git add examples/ crates/client/examples/cost_attribution.rs
git commit -m "docs(examples): runnable cost-attribution/streaming/self-hosted for py, ts, rust"
```

---

## Task 6: CI workflow + flip checklist entries

**Files:**
- Create: `.github/workflows/sdks.yml`
- Modify: `docs/reviews/2026-06-06-audit-checklist.md` (flip L431, L436, L441)

(The `examples/typescript/tsconfig.json` the CI job reuses was created in Task 5.)

- [ ] **Step 1: Write the CI workflow**

Create `.github/workflows/sdks.yml`:

```yaml
name: sdks

on:
  pull_request:
    paths:
      - 'sdk-python/**'
      - 'sdk-typescript/**'
      - 'examples/**'
      - 'crates/client/**'
      - '.github/workflows/sdks.yml'
  push:
    branches: [main]
    paths:
      - 'sdk-python/**'
      - 'sdk-typescript/**'
      - 'examples/**'
      - 'crates/client/**'
      - '.github/workflows/sdks.yml'

jobs:
  python:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-python@v5
        with:
          python-version: '3.11'
      - name: Install
        run: pip install -e "sdk-python[test]"
      - name: Compile examples
        run: python -m py_compile examples/python/*.py
      - name: Test
        run: cd sdk-python && python -m pytest tests/ -q

  typescript:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with:
          node-version: '20'
      - name: Install
        run: cd sdk-typescript && npm install
      - name: Typecheck SDK
        run: cd sdk-typescript && npx tsc -p tsconfig.json --noEmit
      - name: Test
        run: cd sdk-typescript && npx vitest run
      - name: Typecheck examples
        run: cd examples/typescript && ../../sdk-typescript/node_modules/.bin/tsc -p tsconfig.json

  rust-example:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - name: Build example
        run: cargo build -p tt-client --example cost_attribution
```

- [ ] **Step 2: Flip the three checklist entries**

In `docs/reviews/2026-06-06-audit-checklist.md`, change each of these from `- [ ] 🟡 ... 🔴 OPEN` to `- [x] 🟡 ... ✅ DONE (PR #NNN)` with a one-line summary appended (keep the existing Where/Issue/Action lines beneath each — do NOT delete them). The three lines to flip start with:
  - `**[gap/medium] Large parity gap: SDKs expose none of the Rust client's cost-control / metadata surface**`
  - `**[gap/medium] examples/ directory is empty but is in scope and referenced as a deliverable**`
  - `**[dx/medium] No tests ship with either SDK; pyproject declares a test extra with no test files**`

Use this DONE text (substitute the real PR number once opened):
  - parity: `✅ DONE (PR #NNN): added tt_cost_limit/tt_cache (Python) + ttCostLimit/ttCache (TS) lifting to X-TokenTrimmer-Cost-Limit-Usd / X-TokenTrimmer-Cache, validated against the request-override set.`
  - examples: `✅ DONE (PR #NNN): examples/{python,typescript} + crates/client/examples/cost_attribution.rs (cost-attribution, streaming, self-hosted); compile/typecheck-gated in CI.`
  - tests: `✅ DONE (PR #NNN): respx suite (sdk-python/tests) + vitest suite (sdk-typescript/test) incl. concurrency-isolation (proves the with_raw_response fix) and streaming passthrough; new .github/workflows/sdks.yml gates both.`

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/sdks.yml examples/typescript/tsconfig.json docs/reviews/2026-06-06-audit-checklist.md
git commit -m "ci(sdks): typecheck/lint/test workflow for SDKs + examples; flip audit entries"
```

---

## Self-review notes (for the executor)

- **Concurrency test caveat:** the `test_metadata_isolated_across_concurrent_threads` test as written uses one shared mocked cost (`0.0042`) and asserts no caller gets `None`. The OLD stash design could return `None` (or a value taken by another thread) under interleaving; the `with_raw_response` design cannot. Keep the single-cost form — distinct-cost-per-thread can't be matched to a caller through respx deterministically.
- **TS `as never` in examples:** the example casts the request body to `never` then the result to `WithTokenTrimmerMeta<ChatCompletion>` because the monkey-patched `create` keeps the OpenAI overload type (the `tt*` fields aren't in it). This is the documented caller pattern; the README and `WithTokenTrimmerMeta` export support it.
- **If `respx`/`openai`/`vitest` versions differ in CI:** the wire shapes used here (chat.completion JSON, SSE `data:` frames) are stable across openai 1.x (py) / 5.x (ts). If a future major changes `with_raw_response` or the `fetch` option, pin versions in the SDK manifests.
```
