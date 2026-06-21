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

Streaming (``stream=True``) returns the SDK ``Stream`` with two TokenTrimmer
adaptations. The Gateway appends a terminal ``event: tokentrimmer.usage`` SSE
frame (cost JSON) after the OpenAI chunk stream; the underlying OpenAI SSE
parser doesn't expect it and turns it into a malformed ``ChatCompletionChunk``
(``choices=None``) that crashes naive iteration. We (a) STRIP that frame before
the OpenAI parser sees it, so chunk iteration is clean, and (b) SURFACE its cost
payload on ``stream.tt`` (a :class:`StreamCost`) once the stream is drained —
mirroring the non-streaming ``.tt`` accessor. ``stream.tt`` is ``None`` until
the terminal frame has been consumed (and stays ``None`` if the Gateway emits
no usage frame, e.g. self-hosted without pricing).
"""

from __future__ import annotations

import json
import math
import os
from dataclasses import dataclass
from typing import Any, Iterator, Optional

import httpx
from openai import OpenAI

from tokentrimmer.agent import Agent

# Internals dependency: the streaming strip below imports `ServerSentEvent` and
# swaps the stream's injectable `_decoder` (built via `client._make_sse_decoder()`).
# These require openai>=1.70.0 (the tested floor pinned in pyproject.toml); on
# older releases the strip silently no-ops and re-exposes the malformed-chunk crash.
from openai._streaming import ServerSentEvent

DEFAULT_BASE_URL = "https://api.tokentrimmer.com/v1"

# The SSE event name the Gateway uses for its terminal cost/usage frame
# (crates/core/src/routes/sse.rs::usage_event; docs/04-gateway-api-reference.md).
_USAGE_EVENT = "tokentrimmer.usage"

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


@dataclass(frozen=True)
class StreamCost:
    """Cost/usage from the Gateway's terminal ``tokentrimmer.usage`` SSE frame.

    Surfaced on a streaming response as ``stream.tt`` once the stream has been
    drained. This is the streaming counterpart to :class:`TokenTrimmerMeta`
    (which is parsed from response headers on the non-streaming path); the
    streaming cost can't ride on headers because it isn't known until the whole
    response has been generated. Field shape mirrors the Gateway frame
    (crates/core/src/routes/sse.rs::usage_event).
    """

    cost_usd: float
    baseline_cost_usd: float
    saved_usd: float
    provider_cache_saved_usd: float
    input_tokens: int
    output_tokens: int
    cached_tokens: int

    @classmethod
    def _from_payload(cls, data: dict[str, Any]) -> "StreamCost":
        return cls(
            cost_usd=float(data.get("cost_usd", 0.0)),
            baseline_cost_usd=float(data.get("baseline_cost_usd", 0.0)),
            saved_usd=float(data.get("saved_usd", 0.0)),
            provider_cache_saved_usd=float(data.get("provider_cache_saved_usd", 0.0)),
            input_tokens=int(data.get("input_tokens", 0)),
            output_tokens=int(data.get("output_tokens", 0)),
            cached_tokens=int(data.get("cached_tokens", 0)),
        )


class _UsageStrippingDecoder:
    """Wraps the OpenAI SSE decoder to remove the ``tokentrimmer.usage`` frame.

    The OpenAI ``Stream``/``AsyncStream`` drive iteration through an injected SSE
    decoder (``client._make_sse_decoder()``). We swap in this wrapper so the
    Gateway's terminal cost frame never reaches the OpenAI chunk parser (which
    would otherwise emit a malformed chunk). When we see the frame we parse its
    payload into a :class:`StreamCost` and write it onto ``stream.tt`` so the
    caller can read the cost after draining the stream.

    Implements both ``iter_bytes`` (sync ``Stream``) and ``aiter_bytes`` (async
    ``AsyncStream``); the ``SSEBytesDecoder`` protocol declares both.
    """

    def __init__(self, inner: Any, stream: Any) -> None:
        self._inner = inner
        self._stream = stream

    def _intercept(self, sse: ServerSentEvent) -> bool:
        """Return True if the event is the usage frame (and should be dropped)."""
        if sse.event != _USAGE_EVENT:
            return False
        try:
            payload = json.loads(sse.data)
            if isinstance(payload, dict):
                self._stream.tt = StreamCost._from_payload(payload)
        except (ValueError, TypeError):
            # A malformed usage frame is still stripped (never forwarded to the
            # OpenAI parser); we just can't surface cost from it.
            pass
        return True

    def iter_bytes(self, iterator: Iterator[bytes]) -> Iterator[ServerSentEvent]:
        for sse in self._inner.iter_bytes(iterator):
            if not self._intercept(sse):
                yield sse

    async def aiter_bytes(self, iterator: Any) -> Any:
        async for sse in self._inner.aiter_bytes(iterator):
            if not self._intercept(sse):
                yield sse


def _attach_stream_cost(stream: Any) -> Any:
    """Strip the Gateway usage frame from a stream and expose ``stream.tt``.

    Wraps the OpenAI ``Stream``/``AsyncStream``'s SSE decoder so the terminal
    ``tokentrimmer.usage`` frame is dropped before the chunk parser runs, and
    seeds ``stream.tt`` (a :class:`StreamCost`, or ``None`` until the stream is
    drained / if no usage frame was emitted).

    Swapping ``_decoder`` is safe because the OpenAI stream's iterator is a lazy
    generator that reads ``self._decoder`` only when first iterated — which is
    strictly after this returns.
    """
    decoder = getattr(stream, "_decoder", None)
    if decoder is None:
        # Unexpected stream shape (e.g. a future SDK refactor). Don't crash the
        # call; just return it unwrapped (degrades to no .tt, no stripping).
        return stream

    # `tt` is None until the terminal frame is consumed; the decoder writes the
    # parsed StreamCost here when it strips the frame.
    stream.tt = None
    stream._decoder = _UsageStrippingDecoder(decoder, stream)
    return stream


class TokenTrimmer(OpenAI):
    """OpenAI SDK subclass that routes through TokenTrimmer Gateway.

    All ``openai.OpenAI`` constructor parameters are accepted unchanged.
    ``base_url`` defaults to the hosted gateway; override for self-hosted.

    Batch + Files (``client.files.*`` / ``client.batches.*``) are supported via
    the INHERITED OpenAI surface — the Gateway's ``/v1/files`` + ``/v1/batches``
    endpoints are OpenAI-compatible, so do NOT reimplement them here (that would
    shadow the OpenAI typed resources). See ``tests/test_batch.py``.
    """

    def __init__(
        self,
        api_key: Optional[str] = None,
        base_url: str = DEFAULT_BASE_URL,
        **kwargs: Any,
    ) -> None:
        # API-key precedence: explicit `api_key` arg > TOKENTRIMMER_API_KEY env >
        # the base OpenAI SDK's own OPENAI_API_KEY fallback (which kicks in when
        # we pass api_key=None). We only consult TOKENTRIMMER_API_KEY when the
        # caller passed no key, so an explicit argument always wins.
        if api_key is None:
            api_key = os.environ.get("TOKENTRIMMER_API_KEY")
        super().__init__(api_key=api_key, base_url=base_url, **kwargs)
        self._wrap_chat_completions()
        # Driver for the server-side agent loop (`POST /v1/agent/runs`). Reuses
        # this client's base URL / key / httpx transport. See agent.py.
        self._agent = Agent(self)

    @property
    def agent(self) -> Agent:
        """Driver for the server-side agent loop (``client.agent.run(...)``).

        See :class:`tokentrimmer.agent.Agent`. Mirrors the Rust ``tt-client``
        agent driver: it creates an agent run and, whenever the run pauses on a
        client (non-gateway) tool, executes it via the caller's ``executor`` and
        resumes — until a terminal answer or the resume cap.
        """
        return self._agent

    def _wrap_chat_completions(self) -> None:
        completions = self.chat.completions
        original_create = completions.create
        # Capture with_raw_response.create BEFORE patching completions.create,
        # so it references the original underlying method and does not recurse.
        raw_response_create = completions.with_raw_response.create

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
                limit = float(tt_cost_limit)
                if not math.isfinite(limit) or limit < 0:
                    raise ValueError(
                        f"tt_cost_limit must be a non-negative finite number; got {tt_cost_limit!r}"
                    )
                extra_headers["X-TokenTrimmer-Cost-Limit-Usd"] = str(limit)
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

            # Streaming: the Gateway appends a terminal `tokentrimmer.usage` SSE
            # frame after the OpenAI chunk stream. Strip it before the OpenAI
            # parser sees it (else it becomes a malformed chunk) and surface its
            # cost on `stream.tt` (None until the stream is drained).
            if kwargs.get("stream"):
                stream = original_create(*args, **kwargs)
                return _attach_stream_cost(stream)

            # Non-streaming: read the per-call raw response so we can parse the
            # X-TokenTrimmer-* headers without any shared mutable state.
            raw = raw_response_create(*args, **kwargs)
            result = raw.parse()
            meta = _parse_meta(raw.headers)
            try:
                object.__setattr__(result, "tt", meta)
            except Exception:
                setattr(result, "tt", meta)
            return result

        # Monkey-patch the bound method to add the tt_* lifts + .tt metadata.
        self.chat.completions.create = create  # type: ignore[method-assign]
