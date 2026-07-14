"""CrewAI adapter that surfaces TokenTrimmer cost/savings + budget-STOP.

CrewAI wraps OpenAI under the hood, so pointing it at the TokenTrimmer gateway
is a base_url swap. This adapter provides a lightweight ``TokenTrimmerBudgetGuard``
you attach as a CrewAI tool-check hook (or a ``step_callback``) that:

1. Reads the ``x-tokentrimmer-*`` response headers from the OpenAI client's
   raw response (CrewAI exposes it via ``task_output.raw`` when
   ``return_intermediate_steps=True``).
2. Accumulates a per-crew cost / savings total.
3. Enforces a ``max_cost_usd`` budget — a breach raises
   :class:`~tokentrimmer.integrations._budget.BudgetExceeded` which CrewAI
   surfaces as a run-level stop signal (the crew stops, the exception
   propagates to the caller).

Usage::

    from tokentrimmer.integrations.crewai import TokenTrimmerBudgetGuard
    from crewai import Crew, Agent, Task

    guard = TokenTrimmerBudgetGuard(max_cost_usd=0.50)
    crew = Crew(
        agents=[my_agent],
        tasks=[my_task],
        step_callback=guard.on_step,
    )
    crew.kickoff()
    print(guard.total_cost_usd, guard.total_saved_usd)

This mirrors the LangChain (:mod:`~tokentrimmer.integrations.langchain`) and
LiteLLM (:mod:`~tokentrimmer.integrations.litellm`) integrations: same
:class:`BudgetExceeded`, same semconv (:mod:`~tokentrimmer.semconv`), same
``TokenTrimmerMeta.from_headers`` parsing.

Requirements: ``pip install tokentrimmer[crewai]`` (or ``crewai`` separately).
The import is lazy — this module imports cleanly with no ``crewai`` installed.
"""

from __future__ import annotations

import logging
from typing import Any, Optional

from tokentrimmer.client import TokenTrimmerMeta
from tokentrimmer.integrations._budget import BudgetExceeded
from tokentrimmer import semconv

logger = logging.getLogger(__name__)


class TokenTrimmerBudgetGuard:
    """Per-crew cost accumulator + budget guard.

    Attach as a ``step_callback`` on a :class:`crewai.Crew`. Each step's
    ``task_output`` may carry raw response headers (when
    ``return_intermediate_steps=True``); this guard reads them, accumulates
    the cost, and raises :class:`BudgetExceeded` on a breach.
    """

    def __init__(self, max_cost_usd: Optional[float] = None) -> None:
        self.max_cost_usd = max_cost_usd
        self.total_cost_usd: float = 0.0
        self.total_saved_usd: float = 0.0
        self.steps: int = 0

    def on_step(self, step: Any) -> None:
        """CrewAI step_callback — called after each agent step.

        Extracts the cost headers from the step's raw output (if available)
        and accumulates. Raises :class:`BudgetExceeded` on a budget breach.
        """
        self.steps += 1
        # CrewAI's step object is dialect-dependent; try the common accessors.
        headers = _extract_headers(step)
        if not headers:
            logger.debug("step %d: no x-tokentrimmer-* headers found", self.steps)
            return

        meta = TokenTrimmerMeta.from_headers(headers)
        self.total_cost_usd += meta.cost_usd or 0.0
        self.total_saved_usd += (meta.baseline_cost_usd or 0.0) - (meta.cost_usd or 0.0)

        if self.max_cost_usd is not None and self.total_cost_usd > self.max_cost_usd:
            raise BudgetExceeded(
                total=self.total_cost_usd,
                limit=self.max_cost_usd,
                message=(
                    f"Crew budget exceeded: ${self.total_cost_usd:.4f} > "
                    f"${self.max_cost_usd:.4f} cap (step {self.steps})"
                ),
            )


def _extract_headers(step: Any) -> Optional[dict[str, str]]:
    """Best-effort extraction of HTTP response headers from a CrewAI step.

    CrewAI's step object shape varies across versions; this tries the common
    paths where raw HTTP headers might surface. Returns ``None`` when no
    headers are found (the guard degrades gracefully — no cost attribution).
    """
    # CrewAI TaskOutput / Step with response_metadata
    raw = getattr(step, "raw", None)
    if raw and isinstance(raw, dict):
        headers = raw.get("response_metadata", {}).get("headers")
        if headers:
            return headers
    # Some versions nest under task_output
    task_output = getattr(step, "task_output", None) or getattr(step, "output", None)
    if task_output:
        raw = getattr(task_output, "raw", None)
        if raw and isinstance(raw, dict):
            headers = raw.get("response_metadata", {}).get("headers")
            if headers:
                return headers
    return None
