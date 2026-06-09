"""TokenTrimmer client implementation.

Wraps the official ``openai.OpenAI`` client. Override points:

- Default ``base_url`` points at the hosted Gateway.
- A custom ``httpx`` event hook captures ``X-TokenTrimmer-*`` headers per
  response into a ``threading.local``-style stash, then attaches them to
  the parsed response object via the ``.tt`` attribute.
- ``chat.completions.create`` is wrapped to lift the ``tt_tag=`` keyword
  into the ``X-TokenTrimmer-Tag`` request header.
- A default ``max_tokens=4096`` is injected when the caller does not supply
  one (or an equivalent ``max_completion_tokens`` / ``max_output_tokens``).
  Pass an explicit value to override.
"""

from __future__ import annotations

import threading
from dataclasses import dataclass
from typing import Any, Optional

import httpx
from openai import OpenAI

DEFAULT_BASE_URL = "https://api.tokentrimmer.com/v1"


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


class _MetaStash:
    """Thread-local stash for the most recent response's TT metadata.

    The OpenAI SDK doesn't surface response headers on the parsed model, so
    we capture them in an httpx event hook and pin them to a ``threading.local``
    until the wrapping ``create()`` returns and attaches ``.tt``.
    """

    def __init__(self) -> None:
        self._local = threading.local()

    def stash(self, meta: TokenTrimmerMeta) -> None:
        self._local.meta = meta

    def take(self) -> Optional[TokenTrimmerMeta]:
        meta = getattr(self._local, "meta", None)
        if meta is not None:
            self._local.meta = None
        return meta


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
        self._meta_stash = _MetaStash()
        http_client = kwargs.pop("http_client", None) or httpx.Client(
            event_hooks={"response": [self._capture_meta]},
        )
        super().__init__(
            api_key=api_key,
            base_url=base_url,
            http_client=http_client,
            **kwargs,
        )
        # Re-wrap chat.completions.create to lift `tt_tag` kwarg into a header.
        self._wrap_chat_completions()

    def _capture_meta(self, response: httpx.Response) -> None:
        self._meta_stash.stash(_parse_meta(response.headers))

    def _wrap_chat_completions(self) -> None:
        original_create = self.chat.completions.create

        def create(*args: Any, **kwargs: Any) -> Any:
            # Sensible default to prevent unbounded output. User-provided
            # max_tokens / max_completion_tokens / max_output_tokens win.
            if not any(
                k in kwargs
                for k in ("max_tokens", "max_completion_tokens", "max_output_tokens")
            ):
                kwargs["max_tokens"] = 4096
            tt_tag = kwargs.pop("tt_tag", None)
            extra_headers = dict(kwargs.pop("extra_headers", {}) or {})
            if tt_tag is not None:
                extra_headers["X-TokenTrimmer-Tag"] = tt_tag
            if extra_headers:
                kwargs["extra_headers"] = extra_headers
            result = original_create(*args, **kwargs)
            meta = self._meta_stash.take()
            if meta is not None:
                # The OpenAI SDK returns pydantic BaseModel instances; setting
                # an attribute on them works because BaseModel allows it.
                try:
                    object.__setattr__(result, "tt", meta)
                except Exception:
                    # If the model is frozen, fall back to a wrapper attribute.
                    setattr(result, "tt", meta)
            return result

        # Monkey-patch the bound method.
        self.chat.completions.create = create  # type: ignore[method-assign]  # default max_tokens=4096 set in `create` above
