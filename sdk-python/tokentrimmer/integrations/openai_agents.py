"""OpenAI Agents SDK adapter that surfaces TokenTrimmer cost/savings + budget-STOP.

The OpenAI Agents SDK (``agents``) wraps the OpenAI client, so pointing it at
the TokenTrimmer gateway is a base_url swap on the underlying ``AsyncOpenAI`` /
``OpenAI`` client. This adapter provides a ``TokenTrimmerTracingProcessor``
that hooks into the SDK's ``RunItemStreamEvent`` / ``AgentOutput`` lifecycle to:

1. Read the ``x-tokentrimmer-*`` response headers from the raw model response.
2. Accumulate a per-run cost / savings total.
3. Enforce a ``max_cost_usd`` budget — a breach raises
   :class:`~tokentrimmer.integrations._budget.BudgetExceeded`, which the
   Agents SDK surfaces as a run-level stop signal.

Usage::

    from tokentrimmer.integrations.openai_agents import TokenTrimmerTracingProcessor
    from agents import Runner, RunConfig

    tp = TokenTrimmerTracingProcessor(max_cost_usd=0.50)
    config = RunConfig(tracing_processor=tp)
    result = Runner.run_sync(my_agent, "Hello", run_config=config)
    print(tp.total_cost_usd, tp.total_saved_usd)

This mirrors the LangChain / LiteLLM integrations: same
:class:`BudgetExceeded`, same semconv (:mod:`~tokentrimmer.semconv`), same
``TokenTrimmerMeta.from_headers`` parsing.

Requirements: ``pip install tokentrimmer[openai-agents]`` (or ``agents``
separately). The import is lazy — this module imports cleanly with no ``agents``
installed.
"""

from __future__ import annotations

import logging
from typing import Any, Optional

from tokentrimmer.client import TokenTrimmerMeta
from tokentrimmer.integrations._budget import BudgetExceeded
from tokentrimmer import semconv

logger = logging.getLogger(__name__)


class TokenTrimmerTracingProcessor:
    """Per-run cost accumulator + budget guard for the OpenAI Agents SDK.

    Attach as a ``tracing_processor`` on a ``RunConfig``. The processor hooks
    into the SDK's ``process_*`` callbacks to extract cost headers from the
    raw model response + accumulate.
    """

    def __init__(self, max_cost_usd: Optional[float] = None) -> None:
        self.max_cost_usd = max_cost_usd
        self.total_cost_usd: float = 0.0
        self.total_saved_usd: float = 0.0
        self.events: int = 0

    def process_run_item_stream_event(self, event: Any) -> None:
        """Called on each RunItemStreamEvent during a streaming run."""
        self.events += 1
        headers = _extract_headers(event)
        if not headers:
            return

        meta = TokenTrimmerMeta.from_headers(headers)
        self.total_cost_usd += meta.cost_usd or 0.0
        self.total_saved_usd += (meta.baseline_cost_usd or 0.0) - (meta.cost_usd or 0.0)

        if self.max_cost_usd is not None and self.total_cost_usd > self.max_cost_usd:
            raise BudgetExceeded(
                total=self.total_cost_usd,
                limit=self.max_cost_usd,
                message=(
                    f"Agent run budget exceeded: ${self.total_cost_usd:.4f} > "
                    f"${self.max_cost_usd:.4f} cap (event {self.events})"
                ),
            )


def _extract_headers(event: Any) -> Optional[dict[str, str]]:
    """Best-effort extraction of HTTP response headers from an Agents SDK event.

    The Agents SDK's event shape varies; this tries the common paths where the
    raw OpenAI response might surface. Returns ``None`` when no headers are
    found (the guard degrades gracefully).
    """
    # RunItemStreamEvent → RunItem → raw_response
    item = getattr(event, "item", None) or getattr(event, "run_item", None)
    if item:
        raw = getattr(item, "raw_response", None) or getattr(item, "response", None)
        if raw:
            headers = getattr(raw, "headers", None)
            if headers:
                return dict(headers) if not isinstance(headers, dict) else headers
    return None
