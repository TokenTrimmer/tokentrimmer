"""TokenTrimmer Python SDK.

A thin wrapper around the official OpenAI SDK that routes requests through
the TokenTrimmer Gateway. The Gateway accepts the OpenAI Chat Completions
schema verbatim, so the SDK is structurally identical to ``openai.OpenAI``
with three additions:

1. ``base_url`` defaults to ``https://api.tokentrimmer.com/v1``.
2. ``tt_tag`` keyword on ``chat.completions.create`` sets the
   ``X-TokenTrimmer-Tag`` header for cost attribution.
3. ``.tt`` metadata accessor on responses surfaces the
   ``X-TokenTrimmer-*`` headers (cost, baseline_cost, saved, cache, provider,
   model_used, trace_id) without parsing them manually.

Usage::

    from tokentrimmer import TokenTrimmer

    client = TokenTrimmer(api_key="tt_live_...")

    response = client.chat.completions.create(
        model="claude-haiku-4-5",
        messages=[{"role": "user", "content": "Hello"}],
        max_tokens=1024,
        tt_tag="feature=chat-support",
    )

    print(response.choices[0].message.content)
    print(f"Cost: ${response.tt.cost_usd:.4f}  Saved: ${response.tt.saved_usd:.4f}")
    print(f"Cache: {response.tt.cache}  Trace: {response.tt.trace_id}")
"""

from typing import TYPE_CHECKING, Any

from tokentrimmer import semconv
from tokentrimmer.agent import Agent, AgentOutcome, Run, RunUsage
from tokentrimmer.client import StreamCost, TokenTrimmer, TokenTrimmerMeta

if TYPE_CHECKING:  # for type checkers only — no runtime import of the extra
    from tokentrimmer.integrations.langchain import (
        BudgetExceeded,
        TokenTrimmerCostCallback,
    )

__all__ = [
    "TokenTrimmer",
    "TokenTrimmerMeta",
    "StreamCost",
    "Agent",
    "AgentOutcome",
    "Run",
    "RunUsage",
    "semconv",
    # Lazily importable (require the `langchain` extra) — see __getattr__.
    "TokenTrimmerCostCallback",
    "BudgetExceeded",
]
__version__ = "0.2.0"

# Names that live in an optional-extra submodule. Exposing them at the top level
# is a convenience, but importing them eagerly would make `import tokentrimmer`
# hard-depend on LangChain. Resolve them lazily instead: `import tokentrimmer`
# (and every base API above) works with no extras installed; touching
# `tokentrimmer.TokenTrimmerCostCallback` imports the integration on demand and,
# if LangChain is missing, raises the integration's actionable ImportError.
_LAZY_LANGCHAIN = {"TokenTrimmerCostCallback", "BudgetExceeded"}


def __getattr__(name: str) -> Any:
    if name in _LAZY_LANGCHAIN:
        from tokentrimmer.integrations import langchain as _lc

        return getattr(_lc, name)
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
