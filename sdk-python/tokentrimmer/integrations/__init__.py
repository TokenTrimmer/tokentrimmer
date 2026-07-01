"""Optional framework integrations for TokenTrimmer.

Each submodule here depends on a framework that is **not** a dependency of the
base ``tokentrimmer`` package — import them only when the corresponding optional
extra is installed:

* :mod:`tokentrimmer.integrations.langchain` — a LangChain ``BaseCallbackHandler``
  that surfaces the ``x-tokentrimmer-*`` cost/savings headers as OpenTelemetry
  span attributes and enforces an optional per-run budget. Requires the
  ``langchain`` extra (``pip install tokentrimmer[langchain]``); span recording
  additionally uses the ``otel`` extra.

Importing this package does **not** import any framework — ``import tokentrimmer``
and ``import tokentrimmer.integrations`` both work with no extras installed.
"""
