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
4. ``client.gateway`` provides bounded typed model-catalog, capability, and
   request-preflight operations.

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
from tokentrimmer.gateway_metadata import (
    GatewayCapabilitiesDocument,
    GatewayMetadata,
    GatewayMetadataError,
    ModelEntry,
    ModelPricing,
    ModelsResponse,
    PreflightCostEvidence,
    RequestPreflightBatchRequest,
    RequestPreflightBatchResponse,
    RequestPreflightRequest,
    RequestPreflightResponse,
)
# D3: client-side document distillation. The module imports cleanly without the
# `doc-distill` extra (pypdf is imported lazily inside `distill_document`), so
# eager import is safe — `import tokentrimmer` still works with no extras.
from tokentrimmer.document import (
    DistilledDocument,
    DocumentError,
    EmptyExtraction,
    UnsupportedDocument,
    distill_document,
    user_with_document,
    user_with_document_raw,
)

if TYPE_CHECKING:  # for type checkers only — no runtime import of the extra
    from tokentrimmer.integrations._budget import BudgetExceeded
    from tokentrimmer.integrations.langchain import TokenTrimmerCostCallback
    from tokentrimmer.integrations.litellm import TokenTrimmerLiteLLMLogger

__all__ = [
    "TokenTrimmer",
    "TokenTrimmerMeta",
    "StreamCost",
    "Agent",
    "AgentOutcome",
    "Run",
    "RunUsage",
    "GatewayMetadata",
    "GatewayMetadataError",
    "ModelsResponse",
    "ModelEntry",
    "ModelPricing",
    "GatewayCapabilitiesDocument",
    "PreflightCostEvidence",
    "RequestPreflightBatchRequest",
    "RequestPreflightBatchResponse",
    "RequestPreflightRequest",
    "RequestPreflightResponse",
    "semconv",
    # D3: client-side document distillation (PDF text layers). The distill helpers
    # require the `doc-distill` extra (pypdf) at call time; `user_with_document_raw`
    # needs no extra.
    "distill_document",
    "user_with_document",
    "user_with_document_raw",
    "DistilledDocument",
    "DocumentError",
    "UnsupportedDocument",
    "EmptyExtraction",
    # Lazily importable — see __getattr__. The callbacks require their framework
    # extra (`langchain` / `litellm`); BudgetExceeded is dependency-free.
    "TokenTrimmerCostCallback",
    "TokenTrimmerLiteLLMLogger",
    "BudgetExceeded",
]
__version__ = "0.3.0"

# Names that live in an optional-extra submodule. Exposing them at the top level
# is a convenience, but importing them eagerly would make `import tokentrimmer`
# hard-depend on a framework. Resolve them lazily instead: `import tokentrimmer`
# (and every base API above) works with no extras installed; touching an
# integration name imports it on demand and, if the framework is missing, raises
# the integration's actionable ImportError. `BudgetExceeded` is the shared,
# dependency-free budget primitive and resolves without any extra.
_LAZY_LANGCHAIN = {"TokenTrimmerCostCallback"}
_LAZY_LITELLM = {"TokenTrimmerLiteLLMLogger"}


def __getattr__(name: str) -> Any:
    if name == "BudgetExceeded":
        from tokentrimmer.integrations._budget import BudgetExceeded

        return BudgetExceeded
    if name in _LAZY_LANGCHAIN:
        from tokentrimmer.integrations import langchain as _lc

        return getattr(_lc, name)
    if name in _LAZY_LITELLM:
        from tokentrimmer.integrations import litellm as _ll

        return getattr(_ll, name)
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
