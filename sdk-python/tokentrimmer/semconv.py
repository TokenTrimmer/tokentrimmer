"""OpenTelemetry semantic-convention constants for TokenTrimmer cost/savings.

The gateway already stamps these exact attribute keys onto its own request
spans (see the Rust ``tt_telemetry::gen_ai`` module,
``crates/telemetry/src/gen_ai.rs``). This module mirrors that vocabulary on the
client side so a span recorded by a framework integration (e.g.
:mod:`tokentrimmer.integrations.langchain`) uses **identical** keys to the
gateway span — a distributed trace then carries one consistent set of
``gen_ai.*`` / ``tokentrimmer.*`` attributes end to end, and a dashboard keyed on
them resolves whether the span came from the gateway or the client SDK.

This module has **no third-party dependencies** — it is plain string constants
plus a pure ``CostInfo -> attributes`` mapping (a ``dict[str, Any]``). It is
therefore always importable, even when neither the ``langchain`` nor the ``otel``
optional extra is installed. Recording the returned dict onto a live span is the
integration layer's job (and the only place OpenTelemetry is actually needed).

Attribute vocabulary
---------------------

``gen_ai.*`` keys follow the OpenTelemetry `GenAI semantic conventions`_:

* ``gen_ai.system`` — the GenAI provider (``openai``, ``anthropic``, …).
* ``gen_ai.provider.name`` — the newer semconv spelling of ``gen_ai.system``;
  we emit **both** so a dashboard keyed on either resolves.
* ``gen_ai.operation.name`` — the operation (``chat``, ``embeddings``).
* ``gen_ai.request.model`` — the model the caller asked for.
* ``gen_ai.response.model`` — the model that actually served the request
  (differs from the request model after routing / cross-model failover).
* ``gen_ai.usage.input_tokens`` / ``gen_ai.usage.output_tokens`` — token counts.

``tokentrimmer.*`` keys mirror the ``x-tokentrimmer-*`` response headers and are
TokenTrimmer-specific (not part of the upstream semconv):

* ``tokentrimmer.cost_usd`` — what the provider actually bills.
* ``tokentrimmer.baseline_cost_usd`` — cost with no TokenTrimmer optimisation.
* ``tokentrimmer.saved_usd`` — TokenTrimmer-attributed savings (the headline).
* ``tokentrimmer.cache`` — cache outcome (``hit-l1``, ``hit-l2``, ``miss``, …).
* ``tokentrimmer.route`` — the matched route name, when routing applied.
* ``tokentrimmer.trace_id`` — the gateway trace id (``x-tokentrimmer-trace-id``),
  a client-side-only correlation key back to the gateway's own span.

.. _GenAI semantic conventions:
   https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-spans/
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, Dict

if TYPE_CHECKING:  # avoid a runtime import cycle; only needed for type checkers
    from tokentrimmer.client import TokenTrimmerMeta

# --- gen_ai.* (OpenTelemetry GenAI semantic conventions) --------------------

#: ``gen_ai.system`` — the GenAI provider identifier.
GEN_AI_SYSTEM = "gen_ai.system"
#: ``gen_ai.provider.name`` — the newer semconv spelling of :data:`GEN_AI_SYSTEM`.
GEN_AI_PROVIDER_NAME = "gen_ai.provider.name"
#: ``gen_ai.operation.name`` — the GenAI operation being performed.
GEN_AI_OPERATION_NAME = "gen_ai.operation.name"
#: ``gen_ai.request.model`` — the model the request was made to.
GEN_AI_REQUEST_MODEL = "gen_ai.request.model"
#: ``gen_ai.response.model`` — the model that generated the response.
GEN_AI_RESPONSE_MODEL = "gen_ai.response.model"
#: ``gen_ai.usage.input_tokens`` — prompt tokens.
GEN_AI_USAGE_INPUT_TOKENS = "gen_ai.usage.input_tokens"
#: ``gen_ai.usage.output_tokens`` — completion tokens.
GEN_AI_USAGE_OUTPUT_TOKENS = "gen_ai.usage.output_tokens"

# --- tokentrimmer.* (TokenTrimmer-specific cost/savings) --------------------

#: ``tokentrimmer.cost_usd`` — what the provider actually bills (USD).
TT_COST_USD = "tokentrimmer.cost_usd"
#: ``tokentrimmer.baseline_cost_usd`` — cost without TokenTrimmer (USD).
TT_BASELINE_COST_USD = "tokentrimmer.baseline_cost_usd"
#: ``tokentrimmer.saved_usd`` — TokenTrimmer-attributed savings (USD).
TT_SAVED_USD = "tokentrimmer.saved_usd"
#: ``tokentrimmer.cache`` — cache outcome (``hit-l1``, ``hit-l2``, ``miss``, …).
TT_CACHE = "tokentrimmer.cache"
#: ``tokentrimmer.route`` — the matched route name (when routing applied).
TT_ROUTE = "tokentrimmer.route"
#: ``tokentrimmer.trace_id`` — the gateway trace id, for correlation back to the
#: gateway's own span. Client-side-only (the gateway span already carries its own
#: OTel trace/span ids, so it does not re-emit this attribute).
TT_TRACE_ID = "tokentrimmer.trace_id"


def gen_ai_system(provider_id: str) -> str:
    """Map a TokenTrimmer provider id to the OTel ``gen_ai.system`` value.

    Mirrors the gateway's Rust ``gen_ai_system`` mapping so the client and the
    gateway agree on the well-known enum value. The semconv defines fixed values
    for common providers; unregistered ids (aggregators, OpenAI-compat shims,
    local runtimes, the synthetic ``cache``/``sandbox`` pseudo-providers on cache
    hits) pass through verbatim — the spec permits custom values and a stable
    string is more useful in a dashboard than a dropped attribute.
    """
    return {
        "openai": "openai",
        "anthropic": "anthropic",
        # The semconv well-known value for Google Gemini is ``gcp.gemini``.
        "gemini": "gcp.gemini",
        "groq": "groq",
        "mistral": "mistral_ai",
    }.get(provider_id, provider_id)


def cost_info_to_attributes(meta: "TokenTrimmerMeta") -> Dict[str, Any]:
    """Map a parsed :class:`~tokentrimmer.TokenTrimmerMeta` to a span-attribute dict.

    The returned mapping uses the semconv keys defined in this module and is
    ready to hand to ``span.set_attributes(...)``. Only fields the gateway
    actually populated (non-``None`` on ``meta``) appear — the mapping is
    **additive**, mirroring the gateway span's own behaviour, so a self-hosted
    deployment that emits no cost headers yields an empty dict rather than a span
    littered with nulls.

    Note that token counts (``gen_ai.usage.*``) and the requested model
    (``gen_ai.request.model``) are **not** carried on the cost headers, so they
    are not produced here; an integration that has them from the framework's own
    result object records them separately.
    """
    attrs: Dict[str, Any] = {}

    if meta.provider is not None:
        system = gen_ai_system(meta.provider)
        attrs[GEN_AI_SYSTEM] = system
        # Emit the newer semconv spelling too so either-keyed dashboards resolve.
        attrs[GEN_AI_PROVIDER_NAME] = system
    if meta.model_used is not None:
        attrs[GEN_AI_RESPONSE_MODEL] = meta.model_used

    if meta.cost_usd is not None:
        attrs[TT_COST_USD] = meta.cost_usd
    if meta.baseline_cost_usd is not None:
        attrs[TT_BASELINE_COST_USD] = meta.baseline_cost_usd
    if meta.saved_usd is not None:
        attrs[TT_SAVED_USD] = meta.saved_usd
    if meta.cache is not None:
        attrs[TT_CACHE] = meta.cache
    if meta.route is not None:
        attrs[TT_ROUTE] = meta.route
    if meta.trace_id is not None:
        attrs[TT_TRACE_ID] = meta.trace_id

    return attrs
