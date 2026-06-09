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
