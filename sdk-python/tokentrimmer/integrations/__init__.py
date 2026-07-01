"""Optional framework integrations for TokenTrimmer.

Each submodule here depends on a framework that is **not** a dependency of the
base ``tokentrimmer`` package — import them only when the corresponding optional
extra is installed:

* :mod:`tokentrimmer.integrations.langchain` — a LangChain ``BaseCallbackHandler``
  that surfaces the ``x-tokentrimmer-*`` cost/savings headers as OpenTelemetry
  span attributes and enforces an optional per-run budget. Requires the
  ``langchain`` extra (``pip install tokentrimmer[langchain]``); span recording
  additionally uses the ``otel`` extra. The same callback drives **LangGraph**
  unchanged (pass it via ``graph.invoke(..., config={"callbacks": [cb]})``); the
  optional ``langgraph`` extra pulls in LangGraph for that usage.
* :mod:`tokentrimmer.integrations.litellm` — a LiteLLM ``CustomLogger`` that does
  the same for LiteLLM's own callback system. Requires the ``litellm`` extra
  (``pip install tokentrimmer[litellm]``).

Both integrations reuse :mod:`tokentrimmer.semconv`,
:meth:`tokentrimmer.TokenTrimmerMeta.from_headers`, and the shared
:class:`tokentrimmer.integrations._budget.BudgetExceeded`.

Importing this package does **not** import any framework — ``import tokentrimmer``
and ``import tokentrimmer.integrations`` both work with no extras installed.
"""
