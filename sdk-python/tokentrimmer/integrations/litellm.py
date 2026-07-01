"""LiteLLM logger that surfaces TokenTrimmer cost/savings into spans.

When you point LiteLLM at the TokenTrimmer gateway (a ``base_url`` /
``api_base`` swap, or a ``tokentrimmer/…`` custom-provider route), the gateway's
``x-tokentrimmer-*`` cost/savings response headers are invisible to the cost
dashboards people already run. This logger recovers them on every successful call
and:

1. records them as OpenTelemetry span attributes using the shared semconv keys
   (:mod:`tokentrimmer.semconv`) — the same ``gen_ai.*`` / ``tokentrimmer.*``
   attributes the gateway stamps on its own span and the LangChain callback
   emits; and
2. accumulates a per-run cost / savings total, optionally enforcing a
   ``max_cost_usd`` budget.

How the logger gets the headers
-------------------------------

LiteLLM only exposes the raw upstream (gateway) response headers when you opt in
with ``litellm.return_response_headers = True``. With it set, on a successful
call LiteLLM attaches them to the response object in two places, **both** of
which this logger reads in :meth:`~TokenTrimmerLiteLLMLogger.log_success_event`:

* ``response._response_headers`` — the raw headers, un-prefixed
  (``x-tokentrimmer-cost-usd`` etc.); and
* ``response._hidden_params["additional_headers"]`` — the same headers, but each
  provider key is prefixed ``llm_provider-`` (so
  ``llm_provider-x-tokentrimmer-cost-usd``). LiteLLM's
  ``process_response_headers`` applies that prefix; this logger strips it back
  off before parsing.

Wire it up like::

    import litellm
    from tokentrimmer.integrations.litellm import TokenTrimmerLiteLLMLogger

    logger = TokenTrimmerLiteLLMLogger.install(max_cost_usd=0.50)

    resp = litellm.completion(
        model="openai/claude-haiku-4-5",
        api_base="https://api.tokentrimmer.com/v1",
        api_key="tt_live_...",
        messages=[{"role": "user", "content": "Hello"}],
    )
    logger.raise_if_exceeded()          # enforce the budget at your checkpoint
    print(logger.total_cost_usd, logger.total_saved_usd)

:meth:`TokenTrimmerLiteLLMLogger.install` sets
``litellm.return_response_headers = True`` and appends the logger to
``litellm.callbacks`` for you.

Budget stop — how it differs from LangChain
-------------------------------------------

LiteLLM's callbacks are post-hoc **logging** events: LiteLLM catches (and merely
logs) any exception raised inside ``log_success_event`` / ``log_pre_api_call``,
so — unlike the LangChain callback's ``raise_error = True`` inline stop — a raise
from here **cannot** abort the in-flight ``completion`` call. So the budget stop
is a *checkpoint*: when the accumulated cost crosses ``max_cost_usd`` the logger
records the breach (:attr:`budget_exceeded` flips to ``True``) and the caller
enforces it by calling :meth:`raise_if_exceeded` between calls — the natural
place to cap a multi-call agent loop. This is a faithful adaptation of #268's
budget primitive to LiteLLM's callback contract, not a weaker one: the same
:class:`~tokentrimmer.integrations._budget.BudgetExceeded` carries the same
offending total + limit.

If ``return_response_headers`` is not set (or a response carries no gateway
headers, e.g. a self-hosted gateway without pricing), the logger degrades
gracefully: no headers are found, no attributes/totals are recorded, and nothing
is raised.
"""

from __future__ import annotations

import logging
from typing import Any, Dict, List, Mapping, Optional

try:
    from litellm.integrations.custom_logger import CustomLogger
except ImportError as exc:  # pragma: no cover - exercised via the extras guard
    raise ImportError(
        "tokentrimmer.integrations.litellm requires LiteLLM. Install the optional "
        "extra: pip install 'tokentrimmer[litellm]'"
    ) from exc

from tokentrimmer import semconv
from tokentrimmer.client import TokenTrimmerMeta
from tokentrimmer.integrations._budget import BudgetExceeded
from tokentrimmer.integrations._spans import record_on_current_span

_logger = logging.getLogger("tokentrimmer.integrations.litellm")

# LiteLLM prefixes each raw provider response header with this in
# `_hidden_params["additional_headers"]` (see litellm's `process_response_headers`).
_LLM_PROVIDER_PREFIX = "llm_provider-"

__all__ = [
    "BudgetExceeded",
    "TokenTrimmerLiteLLMLogger",
]


class TokenTrimmerLiteLLMLogger(CustomLogger):
    """A LiteLLM :class:`CustomLogger` that records TokenTrimmer cost/savings.

    :param max_cost_usd: optional per-run budget (USD). When the accumulated
        served cost across this logger's calls exceeds it, the logger records a
        breach — :attr:`budget_exceeded` flips to ``True`` — and
        :meth:`raise_if_exceeded` raises :class:`BudgetExceeded`. ``None``
        (default) disables the budget. (See the module docstring for why the stop
        is a checkpoint rather than an inline raise under LiteLLM.)
    :param record_spans: when ``True`` (default) the cost attributes are recorded
        onto the current OpenTelemetry span (if OpenTelemetry is installed and a
        span is recording). When OpenTelemetry is absent the logger still
        accumulates totals — span recording is a best-effort no-op.
    :param logger: optional logger for a per-call cost line (``DEBUG``). Defaults
        to the module logger.

    The handler is stateful: reuse one instance per logical run, read
    :attr:`total_cost_usd` / :attr:`total_saved_usd` after, and call
    :meth:`reset` to reuse it for another run. Register it on LiteLLM with
    :meth:`install` (recommended) or ``litellm.callbacks.append(logger)`` plus
    ``litellm.return_response_headers = True`` yourself.
    """

    def __init__(
        self,
        *,
        max_cost_usd: Optional[float] = None,
        record_spans: bool = True,
        logger: Optional[logging.Logger] = None,
    ) -> None:
        super().__init__()
        self.max_cost_usd = max_cost_usd
        self.record_spans = record_spans
        self._logger = logger or _logger
        #: Accumulated served cost (USD) across this logger's calls.
        self.total_cost_usd: float = 0.0
        #: Accumulated TokenTrimmer-attributed savings (USD).
        self.total_saved_usd: float = 0.0
        #: Accumulated baseline (un-optimised) cost (USD).
        self.total_baseline_usd: float = 0.0
        #: Number of calls that carried TokenTrimmer cost headers.
        self.attributed_calls: int = 0
        #: ``True`` once the accumulated cost has crossed ``max_cost_usd``.
        self.budget_exceeded: bool = False
        self._budget_error: Optional[BudgetExceeded] = None

    def reset(self) -> None:
        """Zero the accumulated totals + budget state to drive another run."""
        self.total_cost_usd = 0.0
        self.total_saved_usd = 0.0
        self.total_baseline_usd = 0.0
        self.attributed_calls = 0
        self.budget_exceeded = False
        self._budget_error = None

    def raise_if_exceeded(self) -> None:
        """Raise :class:`BudgetExceeded` if the run has crossed its budget.

        Call this at your loop checkpoint (typically right after each
        ``litellm.completion`` / ``acompletion``) to enforce ``max_cost_usd``.
        A no-op when no budget is set or the budget has not been breached.
        """
        if self._budget_error is not None:
            raise self._budget_error

    # -- LiteLLM callback hooks -----------------------------------------------

    def log_success_event(
        self, kwargs: Any, response_obj: Any, start_time: Any, end_time: Any
    ) -> None:
        """Record cost attributes + accumulate totals on a successful call.

        The synchronous LiteLLM success hook (fires for ``litellm.completion``).
        Reads the ``x-tokentrimmer-*`` headers surfaced by
        ``return_response_headers = True`` (see the module docstring), parses them
        through the SDK's canonical :meth:`TokenTrimmerMeta.from_headers`, maps
        them to OTel span attributes via :mod:`tokentrimmer.semconv`, and records
        the optional budget breach. A response with no TokenTrimmer headers is a
        no-op.
        """
        self._record(response_obj)

    async def async_log_success_event(
        self, kwargs: Any, response_obj: Any, start_time: Any, end_time: Any
    ) -> None:
        """Async counterpart of :meth:`log_success_event`.

        Fires for ``litellm.acompletion``; shares the same recording logic. Note
        that when LiteLLM dispatches the async callback off the calling task the
        current OTel span may differ, so span recording is best-effort there —
        the cost/savings totals still accumulate.
        """
        self._record(response_obj)

    # -- internals ------------------------------------------------------------

    def _record(self, response_obj: Any) -> None:
        headers = _extract_tt_headers(response_obj)
        if headers is None:
            # No cost headers on this response (e.g. return_response_headers not
            # set, or a self-hosted gateway without pricing). Degrade quietly —
            # never break the caller's flow over missing telemetry.
            return

        meta = TokenTrimmerMeta.from_headers(headers)
        attrs = semconv.cost_info_to_attributes(meta)
        # Token counts aren't on the headers, but the LiteLLM response carries
        # them — fold them in under the gen_ai.usage.* keys when present.
        attrs.update(_token_attributes(response_obj))

        if self.record_spans and attrs:
            record_on_current_span(attrs)

        if meta.cost_usd is not None:
            self.total_cost_usd += meta.cost_usd
        if meta.saved_usd is not None:
            self.total_saved_usd += meta.saved_usd
        if meta.baseline_cost_usd is not None:
            self.total_baseline_usd += meta.baseline_cost_usd
        self.attributed_calls += 1

        self._logger.debug(
            "tokentrimmer litellm success: cost=%s saved=%s route=%s cache=%s "
            "(run total cost=$%.6f saved=$%.6f)",
            meta.cost_usd,
            meta.saved_usd,
            meta.route,
            meta.cache,
            self.total_cost_usd,
            self.total_saved_usd,
        )

        if self.max_cost_usd is not None and self.total_cost_usd > self.max_cost_usd:
            # LiteLLM swallows exceptions raised from this callback, so record the
            # breach for the caller's raise_if_exceeded() checkpoint instead of
            # raising here (which LiteLLM would only log, not propagate).
            self.budget_exceeded = True
            self._budget_error = BudgetExceeded(self.total_cost_usd, self.max_cost_usd)

    @classmethod
    def install(
        cls,
        *,
        max_cost_usd: Optional[float] = None,
        record_spans: bool = True,
        logger: Optional[logging.Logger] = None,
    ) -> "TokenTrimmerLiteLLMLogger":
        """Build the logger, register it on LiteLLM, and return it.

        Sets the easy-to-miss ``litellm.return_response_headers = True`` (without
        which LiteLLM never exposes the ``x-tokentrimmer-*`` headers — the
        counterpart to ``include_response_headers=True`` on the LangChain side)
        and appends the logger to ``litellm.callbacks`` (idempotently). Requires
        the ``litellm`` extra.
        """
        import litellm as _litellm

        _litellm.return_response_headers = True
        handler = cls(
            max_cost_usd=max_cost_usd, record_spans=record_spans, logger=logger
        )
        callbacks = getattr(_litellm, "callbacks", None)
        if not isinstance(callbacks, list):  # pragma: no cover - defensive
            callbacks = []
            _litellm.callbacks = callbacks
        if handler not in callbacks:
            callbacks.append(handler)
        return handler


# --- helpers ----------------------------------------------------------------


def _extract_tt_headers(response_obj: Any) -> Optional[Dict[str, str]]:
    """Recover the ``x-tokentrimmer-*`` headers from a LiteLLM response, if present.

    ``return_response_headers = True`` surfaces the raw gateway headers on the
    response object in two shapes (see the module docstring): ``_response_headers``
    (un-prefixed) and ``_hidden_params["additional_headers"]`` (each key prefixed
    ``llm_provider-``). We check both, strip the prefix, and return a normalised
    mapping only when it actually contains at least one ``x-tokentrimmer-*`` key —
    anything else yields ``None`` so a plain (non-gateway) response is treated as
    "no cost data".
    """
    candidates: List[Any] = []

    response_headers = getattr(response_obj, "_response_headers", None)
    if isinstance(response_headers, Mapping):
        candidates.append(response_headers)

    hidden = getattr(response_obj, "_hidden_params", None)
    additional: Any = None
    if isinstance(hidden, Mapping):
        additional = hidden.get("additional_headers")
    elif hidden is not None:
        additional = getattr(hidden, "additional_headers", None)
    if isinstance(additional, Mapping):
        candidates.append(additional)

    for mapping in candidates:
        normalised: Dict[str, str] = {}
        for k, v in mapping.items():
            key = str(k).lower()
            if key.startswith(_LLM_PROVIDER_PREFIX):
                key = key[len(_LLM_PROVIDER_PREFIX) :]
            normalised[key] = str(v)
        if any(k.startswith("x-tokentrimmer-") for k in normalised):
            return normalised
    return None


def _token_attributes(response_obj: Any) -> Dict[str, Any]:
    """Best-effort ``gen_ai.usage.*`` token counts from a LiteLLM response.

    LiteLLM's ``ModelResponse`` carries a ``usage`` object (or dict) with
    ``prompt_tokens`` / ``completion_tokens``; map those to the semconv token
    keys. Returns an empty dict when no counts are available.
    """
    usage = getattr(response_obj, "usage", None)
    if usage is None:
        return {}

    if isinstance(usage, Mapping):
        inp = usage.get("prompt_tokens")
        out = usage.get("completion_tokens")
    else:
        inp = getattr(usage, "prompt_tokens", None)
        out = getattr(usage, "completion_tokens", None)

    attrs: Dict[str, Any] = {}
    if inp is not None:
        attrs[semconv.GEN_AI_USAGE_INPUT_TOKENS] = int(inp)
    if out is not None:
        attrs[semconv.GEN_AI_USAGE_OUTPUT_TOKENS] = int(out)
    return attrs
