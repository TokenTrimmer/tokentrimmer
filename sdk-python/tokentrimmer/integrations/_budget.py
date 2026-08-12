"""Shared post-response budget exception for framework integrations.

:class:`BudgetExceeded` reports when accumulated *observed* served cost exceeds
a caller-configured budget. It is not the Gateway agent runner's pre-dispatch
``max_cost_usd`` admission guard: every integration raises only after receiving
the response whose cost crossed the budget. This dependency-free module keeps
the same exception type importable without any framework extra installed.

Backwards compatibility: ``BudgetExceeded`` was introduced in the LangChain
integration (0.2.0). It is still importable as
``tokentrimmer.integrations.langchain.BudgetExceeded`` (re-exported there) and as
``tokentrimmer.BudgetExceeded``; this module is the canonical definition both
paths resolve to.
"""

from __future__ import annotations


class BudgetExceeded(RuntimeError):
    """Raised after observed accumulated cost exceeds a configured budget.

    Carries the offending total and limit. Catching it lets a caller stop before
    its next framework step; it does not undo or prevent the completed call.

    How it stops a run differs per framework, because the frameworks expose
    different callback contracts:

    * **LangChain** (:mod:`~tokentrimmer.integrations.langchain`) sets
      ``raise_error = True`` on its handler, so raising this from ``on_llm_end``
      propagates out of ``invoke`` / ``stream`` after the completed LLM response,
      preventing a subsequent chain step when the framework honors the callback.
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
            f"TokenTrimmer observed budget exceeded: accumulated "
            f"${total_cost_usd:.6f} > limit ${limit_usd:.6f}"
        )
