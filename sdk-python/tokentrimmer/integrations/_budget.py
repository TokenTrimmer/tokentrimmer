"""Shared per-run budget primitive for the framework integrations.

:class:`BudgetExceeded` is raised when a run's accumulated served cost exceeds a
caller-configured cap. Both the LangChain callback
(:mod:`tokentrimmer.integrations.langchain`) and the LiteLLM logger
(:mod:`tokentrimmer.integrations.litellm`) enforce the *same* budget primitive,
so it lives here — a dependency-free module (a plain ``RuntimeError`` subclass)
that is always importable, with no framework extra installed.

Backwards compatibility: ``BudgetExceeded`` was introduced in the LangChain
integration (0.2.0). It is still importable as
``tokentrimmer.integrations.langchain.BudgetExceeded`` (re-exported there) and as
``tokentrimmer.BudgetExceeded``; this module is the canonical definition both
paths resolve to.
"""

from __future__ import annotations


class BudgetExceeded(RuntimeError):
    """Raised when a run's accumulated cost exceeds the configured budget.

    Carries the offending total and the limit so a caller catching it can report
    or react.

    How it stops a run differs per framework, because the frameworks expose
    different callback contracts:

    * **LangChain** (:mod:`~tokentrimmer.integrations.langchain`) sets
      ``raise_error = True`` on its handler, so raising this from ``on_llm_end``
      propagates straight out of ``invoke`` / ``stream`` — an *inline* stop on the
      finish that tips the accumulated cost past the cap.
    * **LiteLLM** (:mod:`~tokentrimmer.integrations.litellm`) runs its success
      callback as a post-hoc *logging* event that swallows exceptions, so an
      inline raise cannot abort the call. There the logger records the breach and
      the caller enforces the stop at a checkpoint via
      :meth:`~tokentrimmer.integrations.litellm.TokenTrimmerLiteLLMLogger.raise_if_exceeded`.
    """

    def __init__(self, total_cost_usd: float, limit_usd: float) -> None:
        self.total_cost_usd = total_cost_usd
        self.limit_usd = limit_usd
        super().__init__(
            f"TokenTrimmer run budget exceeded: accumulated ${total_cost_usd:.6f} "
            f"> limit ${limit_usd:.6f}"
        )
