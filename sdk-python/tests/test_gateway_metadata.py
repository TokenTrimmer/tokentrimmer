"""Bounded typed model/capability metadata regressions."""

from __future__ import annotations

import hashlib
import json
from dataclasses import FrozenInstanceError, fields
from typing import get_type_hints

import httpx
import pytest
import respx

from tokentrimmer import GatewayMetadataError, TokenTrimmer
from tokentrimmer.gateway_metadata import CAPABILITIES_MAX_BYTES
from tokentrimmer.product_contracts_generated import (
    GatewayCapabilitiesDocument,
    ModelTokenTrimmerMeta,
    ModelsResponse,
    RequestPreflightBatchRequest,
    RequestPreflightBatchResponse,
    RequestPreflightRequest,
    RequestPreflightResponse,
)

BASE = "http://127.0.0.1:18080/v1"


def test_generated_wire_dataclasses_are_resolvable_exact_and_frozen():
    assert [field.name for field in fields(ModelsResponse)] == [
        "data",
        "object",
        "tokentrimmer",
    ]
    assert [field.name for field in fields(GatewayCapabilitiesDocument)] == [
        "features",
        "generated_at",
        "modality_support",
        "model_support",
        "provider_credentials",
        "provider_health",
        "schema_version",
        "schema_versions",
        "scope",
        "snapshot_scope",
    ]
    assert [field.name for field in fields(RequestPreflightResponse)] == [
        "actions",
        "catalog_cost",
        "catalog_limits",
        "credential",
        "generated_at",
        "model_support",
        "provider_health",
        "provider_resolution",
        "request",
        "request_acceptance",
        "schema_version",
        "scope",
        "snapshot_scope",
    ]
    assert [field.name for field in fields(RequestPreflightBatchResponse)] == [
        "documents",
        "generated_at",
        "limitations",
        "request",
        "schema_version",
        "scope",
        "snapshot_scope",
    ]
    assert get_type_hints(ModelTokenTrimmerMeta)["provider"] is str

    metadata = ModelTokenTrimmerMeta(
        capabilities=(),
        max_input_tokens=1,
        max_output_tokens=1,
        provider="test",
        pricing=None,
    )
    with pytest.raises(FrozenInstanceError):
        setattr(metadata, "provider", "changed")


def _headers() -> dict[str, str]:
    return {
        "content-type": "application/json",
        "cache-control": "private, no-store",
        "x-content-type-options": "nosniff",
    }


def _model_document(priced: bool = False) -> dict:
    pricing = None
    if priced:
        pricing = {
            "input_per_million": 2.5,
            "output_per_million": 10.0,
            "cached_input_per_million": None,
            "cache_write_per_million": None,
            "batch_input_per_million": None,
            "batch_output_per_million": None,
            "flex_input_per_million": None,
            "flex_output_per_million": None,
            "prompt_cache_min_tokens": None,
            "effective_at": "2026-07-01T00:00:00Z",
        }
    data = [
        {
            "id": "gpt-4o-mini",
            "object": "model",
            "owned_by": "openai",
            "tokentrimmer": {
                "provider": "openai",
                "pricing": pricing,
                "capabilities": ["text", "tools", "json_mode", "streaming"],
                "max_input_tokens": 128_000,
                "max_output_tokens": 16_384,
            },
        }
    ]
    snapshot = json.dumps(
        data, ensure_ascii=False, separators=(",", ":"), allow_nan=False
    ).encode()
    return {
        "object": "list",
        "data": data,
        "tokentrimmer": {
            "schema_version": 1,
            "snapshot_scope": "responding_process",
            "source": "registered_provider_catalog",
            "snapshot_sha256": hashlib.sha256(snapshot).hexdigest(),
            "limitations": {
                "provider_credentials": "not_inspected",
                "provider_health": "not_probed",
                "request_acceptance": "not_negotiated",
                "fleet_consistency": "not_attested",
            },
        },
    }


def _reason(code: str) -> dict[str, str]:
    return {"code": code, "message": "Bounded responder explanation."}


def _unknown(code: str) -> dict:
    return {"state": "unknown", "source": "not_negotiated", "reason": _reason(code)}


def _capability_document() -> dict:
    return {
        "schema_version": 1,
        "scope": "gateway_runtime",
        "snapshot_scope": "responding_process",
        "generated_at": "2026-07-26T12:34:56.000Z",
        "features": {
            "fusion": {
                "enabled": {
                    "state": "enabled",
                    "source": "gateway_runtime",
                    "reason": _reason("fusion_kill_switch_enabled"),
                },
                "access": {
                    "state": "available",
                    "reason": _reason("fusion_gateway_gate_passed"),
                },
                "current_tier": {
                    "state": "known",
                    "value": "pro",
                    "source": "authenticated_api_key",
                    "reason": _reason("effective_tier_from_authenticated_key"),
                },
                "minimum_tier": {
                    "state": "known",
                    "value": "pro",
                    "source": "gateway_runtime",
                    "reason": _reason("fusion_minimum_tier_configured"),
                },
                "limits": {
                    "member_models_max": {
                        "value": 8,
                        "enforcement": "gateway_runtime",
                        "reason": _reason("fusion_member_cap"),
                    }
                },
            }
        },
        "provider_credentials": _unknown("provider_credentials_not_inspected"),
        "provider_health": _unknown("provider_health_not_probed"),
        "model_support": _unknown("model_support_not_negotiated"),
        "modality_support": _unknown("modality_support_not_negotiated"),
        "schema_versions": {
            "capabilities_document": {
                "state": "known",
                "version": 1,
                "source": "gateway_runtime",
                "reason": _reason("capabilities_document_version"),
            },
            "fusion_request": {
                "state": "unversioned",
                "version": None,
                "source": "gateway_runtime",
                "reason": _reason("fusion_request_schema_not_versioned"),
            },
        },
    }


def _preflight_request() -> RequestPreflightRequest:
    return RequestPreflightRequest(
        schema_version=1,
        model="gpt-4o-mini",
        provider=None,
        required_capabilities=("text", "tools"),
        declared_input_tokens=1_024,
        requested_max_output_tokens=4_096,
    )


def _preflight_document(request: RequestPreflightRequest | None = None) -> dict:
    request = request or _preflight_request()
    return {
        "schema_version": 1,
        "scope": "request_preflight",
        "snapshot_scope": "responding_process",
        "generated_at": "2026-07-26T12:34:56.000Z",
        "request": {
            "schema_version": request.schema_version,
            "model": request.model,
            "provider": request.provider,
            "required_capabilities": list(request.required_capabilities),
            "declared_input_tokens": request.declared_input_tokens,
            "requested_max_output_tokens": request.requested_max_output_tokens,
        },
        "provider_resolution": {
            "state": "exact_catalog_match",
            "provider": "openai",
            "source": "registered_provider_catalog",
            "reason": _reason("preflight_exact_model_match"),
        },
        "credential": {
            "state": "configured",
            "source": "organization_credential_store",
            "reason": _reason("preflight_credential_record_configured"),
        },
        "model_support": {
            "state": "supported_by_catalog",
            "source": "registered_provider_catalog",
            "missing_capabilities": [],
            "reason": _reason("preflight_required_capabilities_catalog_match"),
        },
        "catalog_limits": {
            "state": "within_catalog_metadata",
            "source": "registered_provider_catalog",
            "catalog_max_input_tokens": 128_000,
            "catalog_max_output_tokens": 16_384,
            "reason": _reason("preflight_declared_tokens_within_catalog"),
        },
        "catalog_cost": {
            "state": "catalog_projection",
            "source": "registered_provider_pricing_catalog",
            "standard_input_rate_usd_per_million": 0.15,
            "standard_output_rate_usd_per_million": 0.60,
            "input_tokens_low": 1_024,
            "input_tokens_high": 1_024,
            "output_tokens_low": 0,
            "output_tokens_high": 4_096,
            "standard_cost_usd_low": 0.000_153_6,
            "standard_cost_usd_high": 0.002_611_2,
            "reason": _reason("preflight_standard_cost_catalog_projection"),
        },
        "provider_health": _unknown("provider_health_not_probed"),
        "request_acceptance": _unknown("request_acceptance_not_attempted"),
        "actions": [
            {
                "code": "execute_request_and_handle_result",
                "required_before_request": False,
                "reason": _reason("preflight_action_real_request_authoritative"),
            }
        ],
    }

def _preflight_batch_request() -> RequestPreflightBatchRequest:
    return RequestPreflightBatchRequest(
        requests=(_preflight_request(), _preflight_request()),
        schema_version=1,
    )


def _preflight_batch_document(
    request: RequestPreflightBatchRequest | None = None,
) -> dict:
    request = request or _preflight_batch_request()
    return {
        "schema_version": 1,
        "scope": "request_preflight_batch",
        "snapshot_scope": "responding_process",
        "generated_at": "2026-07-26T12:34:56.000Z",
        "request": {
            "schema_version": 1,
            "requests": [
                _preflight_document(declaration)["request"]
                for declaration in request.requests
            ],
        },
        "documents": [
            _preflight_document(declaration) for declaration in request.requests
        ],
        "limitations": [
            _reason("preflight_batch_single_responder_not_atomic"),
            _reason("preflight_batch_provider_execution_not_observed"),
        ],
    }


@respx.mock
def test_models_is_anonymous_bounded_and_typed():
    route = respx.get(f"{BASE}/models").mock(
        return_value=httpx.Response(200, headers=_headers(), json=_model_document())
    )
    document = TokenTrimmer(
        api_key="configured-but-anonymous", base_url=BASE
    ).gateway.models()
    assert document.data[0].tokentrimmer.max_input_tokens == 128_000
    assert "authorization" not in route.calls.last.request.headers


@respx.mock
def test_models_recomputes_integer_valued_float_digest():
    respx.get(f"{BASE}/models").mock(
        return_value=httpx.Response(
            200, headers=_headers(), json=_model_document(priced=True)
        )
    )
    document = TokenTrimmer(api_key="ignored", base_url=BASE).gateway.models()
    assert document.data[0].tokentrimmer.pricing is not None
    assert document.data[0].tokentrimmer.pricing.output_per_million == 10.0


@respx.mock
def test_capabilities_sends_one_live_bearer_and_validates_semantics():
    route = respx.get(f"{BASE}/capabilities").mock(
        return_value=httpx.Response(
            200, headers=_headers(), json=_capability_document()
        )
    )
    document = TokenTrimmer(api_key="tt_live_test", base_url=BASE).gateway.capabilities()
    assert document.features.fusion.access.state == "available"
    assert route.calls.last.request.headers["authorization"] == "Bearer tt_live_test"


@respx.mock
def test_preflight_posts_one_typed_non_dispatching_declaration():
    declaration = _preflight_request()
    route = respx.post(f"{BASE}/capabilities/preflight").mock(
        return_value=httpx.Response(
            200, headers=_headers(), json=_preflight_document(declaration)
        )
    )
    document = TokenTrimmer(
        api_key="tt_live_test", base_url=BASE
    ).gateway.preflight(declaration)
    assert document.credential.state == "configured"
    assert document.catalog_cost.standard_cost_usd_high == 0.002_611_2
    assert document.provider_health.state == "unknown"
    assert route.calls.last.request.headers["authorization"] == "Bearer tt_live_test"
    assert json.loads(route.calls.last.request.content) == _preflight_document(declaration)[
        "request"
    ]

@respx.mock
def test_preflight_batch_posts_one_ordered_responder_request():
    declaration = _preflight_batch_request()
    route = respx.post(f"{BASE}/capabilities/preflight/batch").mock(
        return_value=httpx.Response(
            200, headers=_headers(), json=_preflight_batch_document(declaration)
        )
    )
    document = TokenTrimmer(
        api_key="tt_live_test", base_url=BASE
    ).gateway.preflight_batch(declaration)
    assert len(document.documents) == 2
    assert document.documents[1].generated_at == document.generated_at
    assert route.call_count == 1
    assert json.loads(route.calls.last.request.content) == _preflight_batch_document(
        declaration
    )["request"]


@respx.mock
def test_preflight_rejects_unsafe_declaration_and_contradictory_evidence():
    route = respx.post(f"{BASE}/capabilities/preflight")
    contradiction = _preflight_document()
    contradiction["request_acceptance"]["state"] = "available"
    route.mock(return_value=httpx.Response(200, headers=_headers(), json=contradiction))
    client = TokenTrimmer(api_key="tt_live_test", base_url=BASE)

    invalid = RequestPreflightRequest(
        schema_version=1,
        model="gpt-4o-mini",
        provider=None,
        required_capabilities=("text",),
        declared_input_tokens=1,
        requested_max_output_tokens=0,
    )
    with pytest.raises(GatewayMetadataError, match="preflight_request"):
        client.gateway.preflight(invalid)
    assert not route.called

    with pytest.raises(GatewayMetadataError, match="unknown_evidence"):
        client.gateway.preflight(_preflight_request())
    assert route.call_count == 1

    cost_contradiction = _preflight_document()
    cost_contradiction["catalog_cost"]["standard_cost_usd_high"] = 999
    route.mock(
        return_value=httpx.Response(
            200, headers=_headers(), json=cost_contradiction
        )
    )
    with pytest.raises(GatewayMetadataError, match="preflight_cost"):
        client.gateway.preflight(_preflight_request())
    assert route.call_count == 2


@respx.mock
def test_capabilities_rejects_unsafe_inputs_before_network():
    route = respx.get(f"{BASE}/capabilities").mock(return_value=httpx.Response(200))
    with pytest.raises(GatewayMetadataError, match="api_key"):
        TokenTrimmer(api_key="tt_test_x", base_url=BASE).gateway.capabilities()
    with pytest.raises(GatewayMetadataError, match="base_url"):
        TokenTrimmer(
            api_key="tt_live_test", base_url="http://gateway.example/v1"
        ).gateway.capabilities()
    assert not route.called


@respx.mock
def test_capabilities_does_not_follow_redirect_or_forward_bearer():
    source = respx.get(f"{BASE}/capabilities").mock(
        return_value=httpx.Response(302, headers={"location": f"{BASE}/target"})
    )
    target = respx.get(f"{BASE}/target").mock(return_value=httpx.Response(200))
    with pytest.raises(GatewayMetadataError, match="redirect"):
        TokenTrimmer(api_key="tt_live_test", base_url=BASE).gateway.capabilities()
    assert source.call_count == 1
    assert target.call_count == 0


@respx.mock
def test_digest_and_capability_contradictions_fail_closed():
    bad_model = _model_document()
    bad_model["tokentrimmer"]["snapshot_sha256"] = "0" * 64
    contradiction = _capability_document()
    contradiction["features"]["fusion"]["access"] = {
        "state": "unavailable",
        "reason": _reason("fusion_disabled"),
    }
    respx.get(f"{BASE}/models").mock(
        return_value=httpx.Response(200, headers=_headers(), json=bad_model)
    )
    respx.get(f"{BASE}/capabilities").mock(
        return_value=httpx.Response(200, headers=_headers(), json=contradiction)
    )
    client = TokenTrimmer(api_key="tt_live_test", base_url=BASE)
    with pytest.raises(GatewayMetadataError, match="snapshot_mismatch"):
        client.gateway.models()
    with pytest.raises(GatewayMetadataError, match="fusion_access"):
        client.gateway.capabilities()


@respx.mock
def test_remote_error_is_redacted_and_body_cap_is_enforced():
    route = respx.get(f"{BASE}/capabilities")
    route.side_effect = [
        httpx.Response(503, text="private provider diagnostic"),
        httpx.Response(
            200,
            headers={**_headers(), "content-length": str(CAPABILITIES_MAX_BYTES + 1)},
            content=b"x" * (CAPABILITIES_MAX_BYTES + 1),
        ),
    ]
    client = TokenTrimmer(api_key="tt_live_test", base_url=BASE)
    with pytest.raises(GatewayMetadataError) as failed:
        client.gateway.capabilities()
    assert failed.value.status == 503
    assert "provider diagnostic" not in str(failed.value)
    with pytest.raises(GatewayMetadataError, match="response_too_large"):
        client.gateway.capabilities()
