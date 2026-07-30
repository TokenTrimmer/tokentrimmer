"""Bounded typed reads of responder-scoped TokenTrimmer metadata."""

from __future__ import annotations

import hashlib
import ipaddress
import json
import math
import time
import unicodedata
from datetime import datetime, timezone
from typing import Any, Mapping, Optional, Sequence
from urllib.parse import SplitResult, urlsplit, urlunsplit

import httpx

from tokentrimmer.product_contracts_generated import (
    AccessEvidence,
    CapabilityReason,
    EnabledEvidence,
    FusionCapability,
    FusionLimits,
    GatewayCapabilitiesDocument,
    GatewayFeatures,
    ModelCatalogLimitations,
    ModelEntry,
    ModelPricing,
    ModelsDocumentMeta as ModelsDocumentMetadata,
    ModelsResponse,
    ModelTokenTrimmerMeta as ModelMetadata,
    NumericLimit,
    PreflightAction,
    PreflightCostEvidence,
    PreflightCredentialEvidence,
    PreflightLimitEvidence,
    PreflightModelSupportEvidence,
    PreflightProviderResolution,
    RequestPreflightBatchRequest,
    RequestPreflightBatchResponse,
    RequestPreflightRequest,
    RequestPreflightResponse,
    SchemaVersionEvidence,
    SchemaVersions,
    TierEvidence,
    UnknownEvidence,
)

MODELS_MAX_BYTES = 256 * 1024
CAPABILITIES_MAX_BYTES = 64 * 1024
PREFLIGHT_MAX_BYTES = 64 * 1024
PREFLIGHT_TOKEN_MAX = 4_294_967_295
REQUEST_TIMEOUT_SECONDS = 5.0
_CAPABILITIES = frozenset(
    {
        "text",
        "vision",
        "audio",
        "tools",
        "json_mode",
        "streaming",
        "reasoning",
        "prompt_caching",
    }
)


class GatewayMetadataError(Exception):
    """Fixed local metadata failure; remote response prose is never included."""

    def __init__(self, code: str, status: Optional[int] = None) -> None:
        self.code = code
        self.status = status
        message = f"gateway metadata error: {code}"
        if status is not None:
            message = f"gateway metadata HTTP {status}"
        super().__init__(message)


class GatewayMetadata:
    """Read one gateway responder's catalog and authenticated capability facts."""

    def __init__(self, base_url: str, api_key: str) -> None:
        self._base_url = base_url
        self._api_key = api_key

    def models(self) -> ModelsResponse:
        """Return anonymous catalog metadata, not provider/readiness evidence."""
        endpoint = _endpoint(self._base_url, "models", authenticated=False)
        body = _request(endpoint, {}, MODELS_MAX_BYTES)
        return _parse_models(body)

    def capabilities(self) -> GatewayCapabilitiesDocument:
        """Return this key's switch/tier evidence from one responding process."""
        if not self._api_key.startswith("tt_live_") or self._api_key == "tt_live_":
            raise GatewayMetadataError("api_key")
        endpoint = _endpoint(self._base_url, "capabilities", authenticated=True)
        body = _request(
            endpoint,
            {"authorization": f"Bearer {self._api_key}"},
            CAPABILITIES_MAX_BYTES,
        )
        return _parse_capabilities(body)

    def preflight(
        self, request: RequestPreflightRequest
    ) -> RequestPreflightResponse:
        """Compare one request with local responder facts, without provider I/O."""
        if not self._api_key.startswith("tt_live_") or self._api_key == "tt_live_":
            raise GatewayMetadataError("api_key")
        declaration = _normalize_preflight_request(request)
        endpoint = _endpoint(
            self._base_url, "capabilities/preflight", authenticated=True
        )
        body = _request(
            endpoint,
            {"authorization": f"Bearer {self._api_key}"},
            PREFLIGHT_MAX_BYTES,
            method="POST",
            json_body=_preflight_request_dict(declaration),
        )
        return _parse_preflight(body, declaration)

    def preflight_batch(
        self, request: RequestPreflightBatchRequest
    ) -> RequestPreflightBatchResponse:
        """Evaluate 1-9 declarations on one responder without provider I/O."""
        if not self._api_key.startswith("tt_live_") or self._api_key == "tt_live_":
            raise GatewayMetadataError("api_key")
        declaration = _normalize_preflight_batch_request(request)
        endpoint = _endpoint(
            self._base_url, "capabilities/preflight/batch", authenticated=True
        )
        body = _request(
            endpoint,
            {"authorization": f"Bearer {self._api_key}"},
            PREFLIGHT_MAX_BYTES,
            method="POST",
            json_body=_preflight_batch_request_dict(declaration),
        )
        return _parse_preflight_batch(body, declaration)


def _endpoint(base_url: str, suffix: str, authenticated: bool) -> str:
    try:
        parsed = urlsplit(base_url)
        _validate_split_url(parsed)
    except (TypeError, ValueError):
        raise GatewayMetadataError("base_url") from None
    if authenticated and parsed.scheme != "https":
        if parsed.scheme != "http" or not _is_literal_loopback(parsed.hostname):
            raise GatewayMetadataError("base_url")
    path = f"{parsed.path.rstrip('/')}/{suffix}"
    return urlunsplit((parsed.scheme, parsed.netloc, path, "", ""))


def _validate_split_url(parsed: SplitResult) -> None:
    if (
        parsed.scheme not in {"http", "https"}
        or not parsed.netloc
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
    ):
        raise GatewayMetadataError("base_url")


def _is_literal_loopback(hostname: Optional[str]) -> bool:
    if hostname is None:
        return False
    try:
        return ipaddress.ip_address(hostname).is_loopback
    except ValueError:
        return False


def _request(
    endpoint: str,
    headers: Mapping[str, str],
    limit: int,
    *,
    method: str = "GET",
    json_body: Optional[Mapping[str, Any]] = None,
) -> bytes:
    deadline = time.monotonic() + REQUEST_TIMEOUT_SECONDS
    try:
        with httpx.Client(
            timeout=httpx.Timeout(REQUEST_TIMEOUT_SECONDS),
            follow_redirects=False,
        ) as client:
            stream_options: dict[str, Any] = {}
            if json_body is not None:
                stream_options["json"] = json_body
            with client.stream(
                method,
                endpoint,
                headers={"accept": "application/json", **headers},
                **stream_options,
            ) as response:
                if 300 <= response.status_code < 400:
                    _read_bounded(response, limit, deadline)
                    raise GatewayMetadataError("redirect")
                if not response.is_success:
                    _read_bounded(response, limit, deadline)
                    raise GatewayMetadataError("status", response.status_code)
                header_error = _validate_headers(response.headers)
                body = _read_bounded(response, limit, deadline)
                if header_error is not None:
                    raise header_error
                return body
    except GatewayMetadataError:
        raise
    except (httpx.HTTPError, TimeoutError):
        raise GatewayMetadataError("request_failed") from None


def _validate_headers(headers: httpx.Headers) -> Optional[GatewayMetadataError]:
    content_type = headers.get("content-type", "").split(";", 1)[0].strip().lower()
    if content_type != "application/json":
        return GatewayMetadataError("content_type")
    cache_control = {
        item.strip().lower() for item in headers.get("cache-control", "").split(",")
    }
    if "no-store" not in cache_control:
        return GatewayMetadataError("cache_control")
    if headers.get("x-content-type-options", "").lower() != "nosniff":
        return GatewayMetadataError("content_type_options")
    return None


def _read_bounded(response: httpx.Response, limit: int, deadline: float) -> bytes:
    declared = response.headers.get("content-length")
    if declared is not None and declared.isdigit() and int(declared) > limit:
        raise GatewayMetadataError("response_too_large")
    chunks = bytearray()
    for chunk in response.iter_bytes():
        if time.monotonic() > deadline:
            raise GatewayMetadataError("request_timeout")
        if len(chunk) > limit - len(chunks):
            raise GatewayMetadataError("response_too_large")
        chunks.extend(chunk)
    if time.monotonic() > deadline:
        raise GatewayMetadataError("request_timeout")
    return bytes(chunks)


def _load_json(body: bytes) -> Any:
    try:
        return json.loads(body.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        raise GatewayMetadataError("invalid_json") from None


def _parse_models(body: bytes) -> ModelsResponse:
    root = _record(_load_json(body), "model_document")
    if root.get("object") != "list" or not isinstance(root.get("data"), list):
        _invalid("model_document")
    metadata = _record(root.get("tokentrimmer"), "model_metadata")
    limitations = _record(metadata.get("limitations"), "model_metadata")
    digest = metadata.get("snapshot_sha256")
    if (
        metadata.get("schema_version") != 1
        or metadata.get("snapshot_scope") != "responding_process"
        or metadata.get("source") != "registered_provider_catalog"
        or not isinstance(digest, str)
        or len(digest) != 64
        or any(ch not in "0123456789abcdef" for ch in digest)
        or limitations.get("provider_credentials") != "not_inspected"
        or limitations.get("provider_health") != "not_probed"
        or limitations.get("request_acceptance") != "not_negotiated"
        or limitations.get("fleet_consistency") != "not_attested"
    ):
        _invalid("model_metadata")

    seen: set[tuple[str, str]] = set()
    entries = tuple(_parse_model_entry(item, seen) for item in root["data"])
    canonical = [_model_entry_dict(entry) for entry in entries]
    snapshot = json.dumps(
        canonical,
        ensure_ascii=False,
        separators=(",", ":"),
        allow_nan=False,
    ).encode("utf-8")
    if hashlib.sha256(snapshot).hexdigest() != digest:
        _invalid("snapshot_mismatch")
    return ModelsResponse(
        object="list",
        data=entries,
        tokentrimmer=ModelsDocumentMetadata(
            schema_version=1,
            snapshot_scope="responding_process",
            source="registered_provider_catalog",
            snapshot_sha256=digest,
            limitations=ModelCatalogLimitations(
                provider_credentials="not_inspected",
                provider_health="not_probed",
                request_acceptance="not_negotiated",
                fleet_consistency="not_attested",
            ),
        ),
    )


def _parse_model_entry(
    value: Any, seen: set[tuple[str, str]]
) -> ModelEntry:
    entry = _record(value, "model_entry")
    metadata = _record(entry.get("tokentrimmer"), "model_entry")
    model_id = entry.get("id")
    provider = entry.get("owned_by")
    capabilities = metadata.get("capabilities")
    if (
        entry.get("object") != "model"
        or not _nonempty(model_id)
        or not _nonempty(provider)
        or metadata.get("provider") != provider
        or not isinstance(capabilities, list)
        or not capabilities
        or any(not isinstance(item, str) or item not in _CAPABILITIES for item in capabilities)
        or not _positive_int(metadata.get("max_input_tokens"))
        or not _nonnegative_int(metadata.get("max_output_tokens"))
        or "pricing" not in metadata
    ):
        _invalid("model_entry")
    identity = (provider, model_id)
    if identity in seen:
        _invalid("duplicate_model")
    seen.add(identity)
    pricing_raw = metadata["pricing"]
    return ModelEntry(
        id=model_id,
        object="model",
        owned_by=provider,
        tokentrimmer=ModelMetadata(
            provider=provider,
            pricing=None if pricing_raw is None else _parse_pricing(pricing_raw),
            capabilities=tuple(capabilities),
            max_input_tokens=metadata["max_input_tokens"],
            max_output_tokens=metadata["max_output_tokens"],
        ),
    )


def _parse_pricing(value: Any) -> ModelPricing:
    pricing = _record(value, "pricing")
    required = ("input_per_million", "output_per_million")
    optional = (
        "cached_input_per_million",
        "cache_write_per_million",
        "batch_input_per_million",
        "batch_output_per_million",
        "flex_input_per_million",
        "flex_output_per_million",
    )
    if any(key not in pricing or not _nonnegative_float(pricing[key]) for key in required):
        _invalid("pricing")
    if any(
        key not in pricing
        or (pricing[key] is not None and not _nonnegative_float(pricing[key]))
        for key in optional
    ):
        _invalid("pricing")
    cache_min = pricing.get("prompt_cache_min_tokens")
    if "prompt_cache_min_tokens" not in pricing or (
        cache_min is not None and not _nonnegative_int(cache_min)
    ):
        _invalid("pricing")
    if not _canonical_effective_at(pricing.get("effective_at")):
        _invalid("pricing")
    return ModelPricing(
        input_per_million=float(pricing["input_per_million"]),
        output_per_million=float(pricing["output_per_million"]),
        cached_input_per_million=_optional_float(pricing["cached_input_per_million"]),
        cache_write_per_million=_optional_float(pricing["cache_write_per_million"]),
        batch_input_per_million=_optional_float(pricing["batch_input_per_million"]),
        batch_output_per_million=_optional_float(pricing["batch_output_per_million"]),
        flex_input_per_million=_optional_float(pricing["flex_input_per_million"]),
        flex_output_per_million=_optional_float(pricing["flex_output_per_million"]),
        prompt_cache_min_tokens=cache_min,
        effective_at=pricing["effective_at"],
    )


def _optional_float(value: Any) -> Optional[float]:
    return None if value is None else float(value)


def _model_entry_dict(entry: ModelEntry) -> dict[str, Any]:
    metadata = entry.tokentrimmer
    pricing = metadata.pricing
    pricing_dict = None
    if pricing is not None:
        pricing_dict = {
            "input_per_million": pricing.input_per_million,
            "output_per_million": pricing.output_per_million,
            "cached_input_per_million": pricing.cached_input_per_million,
            "cache_write_per_million": pricing.cache_write_per_million,
            "batch_input_per_million": pricing.batch_input_per_million,
            "batch_output_per_million": pricing.batch_output_per_million,
            "flex_input_per_million": pricing.flex_input_per_million,
            "flex_output_per_million": pricing.flex_output_per_million,
            "prompt_cache_min_tokens": pricing.prompt_cache_min_tokens,
            "effective_at": pricing.effective_at,
        }
    return {
        "id": entry.id,
        "object": entry.object,
        "owned_by": entry.owned_by,
        "tokentrimmer": {
            "provider": metadata.provider,
            "pricing": pricing_dict,
            "capabilities": list(metadata.capabilities),
            "max_input_tokens": metadata.max_input_tokens,
            "max_output_tokens": metadata.max_output_tokens,
        },
    }


def _parse_capabilities(body: bytes) -> GatewayCapabilitiesDocument:
    root = _record(_load_json(body), "capability_document")
    if (
        root.get("schema_version") != 1
        or root.get("scope") != "gateway_runtime"
        or root.get("snapshot_scope") != "responding_process"
        or not _canonical_timestamp(root.get("generated_at"))
    ):
        _invalid("capability_metadata")
    features = _record(root.get("features"), "capability_document")
    fusion = _record(features.get("fusion"), "fusion")
    enabled = _record(fusion.get("enabled"), "fusion")
    access = _record(fusion.get("access"), "fusion")
    current = _parse_current_tier(fusion.get("current_tier"))
    minimum = _parse_minimum_tier(fusion.get("minimum_tier"))
    limits = _record(fusion.get("limits"), "fusion")
    member = _record(limits.get("member_models_max"), "member_models_max")

    if enabled.get("state") == "enabled":
        switch_enabled = True
        enabled_reason = "fusion_kill_switch_enabled"
    elif enabled.get("state") == "disabled":
        switch_enabled = False
        enabled_reason = "fusion_kill_switch_disabled"
    else:
        _invalid("fusion_enabled")
    if enabled.get("source") != "gateway_runtime":
        _invalid("fusion_enabled")
    parsed_enabled_reason = _parse_reason(enabled.get("reason"), enabled_reason)

    if not switch_enabled:
        expected_access = ("unavailable", "fusion_disabled")
    elif _tier_rank(current.value) < _tier_rank(minimum.value):
        expected_access = ("unavailable", "fusion_tier_below_minimum")
    else:
        expected_access = ("available", "fusion_gateway_gate_passed")
    if access.get("state") != expected_access[0]:
        _invalid("fusion_access")
    parsed_access_reason = _parse_reason(access.get("reason"), expected_access[1])

    if (
        not _positive_int(member.get("value"))
        or member.get("enforcement") != "gateway_runtime"
    ):
        _invalid("member_models_max")
    parsed_member_reason = _parse_reason(member.get("reason"), "fusion_member_cap")

    versions = _record(root.get("schema_versions"), "schema_versions")
    parsed_versions = SchemaVersions(
        capabilities_document=_parse_schema_version(
            versions.get("capabilities_document"),
            "known",
            1,
            "capabilities_document_version",
        ),
        fusion_request=_parse_schema_version(
            versions.get("fusion_request"),
            "unversioned",
            None,
            "fusion_request_schema_not_versioned",
        ),
    )
    return GatewayCapabilitiesDocument(
        schema_version=1,
        scope="gateway_runtime",
        snapshot_scope="responding_process",
        generated_at=root["generated_at"],
        features=GatewayFeatures(
            fusion=FusionCapability(
                enabled=EnabledEvidence(
                    state=enabled["state"],
                    source="gateway_runtime",
                    reason=parsed_enabled_reason,
                ),
                access=AccessEvidence(
                    state=access["state"],
                    reason=parsed_access_reason,
                ),
                current_tier=current,
                minimum_tier=minimum,
                limits=FusionLimits(
                    member_models_max=NumericLimit(
                        value=member["value"],
                        enforcement="gateway_runtime",
                        reason=parsed_member_reason,
                    )
                ),
            )
        ),
        provider_credentials=_parse_unknown(
            root.get("provider_credentials"), "provider_credentials_not_inspected"
        ),
        provider_health=_parse_unknown(
            root.get("provider_health"), "provider_health_not_probed"
        ),
        model_support=_parse_unknown(
            root.get("model_support"), "model_support_not_negotiated"
        ),
        modality_support=_parse_unknown(
            root.get("modality_support"), "modality_support_not_negotiated"
        ),
        schema_versions=parsed_versions,
    )


def _normalize_preflight_request(
    request: RequestPreflightRequest,
) -> RequestPreflightRequest:
    if not isinstance(request, RequestPreflightRequest):
        _invalid("preflight_request")
    if (
        request.schema_version != 1
        or not _bounded_text(request.model, 256)
        or any(unicodedata.category(ch) == "Cc" for ch in request.model)
        or not isinstance(request.required_capabilities, tuple)
        or len(request.required_capabilities) > 8
        or any(
            not isinstance(capability, str) or capability not in _CAPABILITIES
            for capability in request.required_capabilities
        )
        or len(set(request.required_capabilities)) != len(request.required_capabilities)
    ):
        _invalid("preflight_request")
    if request.provider is not None and (
        not _bounded_text(request.provider, 64)
        or any(
            not (ch.isascii() and (ch.islower() or ch.isdigit() or ch in "_-"))
            for ch in request.provider
        )
    ):
        _invalid("preflight_request")
    if (
        request.declared_input_tokens is not None
        and (
            not _nonnegative_int(request.declared_input_tokens)
            or request.declared_input_tokens > PREFLIGHT_TOKEN_MAX
        )
    ) or (
        request.requested_max_output_tokens is not None
        and (
            not _positive_int(request.requested_max_output_tokens)
            or request.requested_max_output_tokens > PREFLIGHT_TOKEN_MAX
        )
    ):
        _invalid("preflight_request")
    return request


def _preflight_request_dict(request: RequestPreflightRequest) -> dict[str, Any]:
    return {
        "schema_version": 1,
        "model": request.model,
        "provider": request.provider,
        "required_capabilities": list(request.required_capabilities),
        "declared_input_tokens": request.declared_input_tokens,
        "requested_max_output_tokens": request.requested_max_output_tokens,
    }

def _normalize_preflight_batch_request(
    request: RequestPreflightBatchRequest,
) -> RequestPreflightBatchRequest:
    if (
        not isinstance(request, RequestPreflightBatchRequest)
        or request.schema_version != 1
        or not isinstance(request.requests, tuple)
        or not 1 <= len(request.requests) <= 9
    ):
        _invalid("preflight_batch_request")
    for declaration in request.requests:
        _normalize_preflight_request(declaration)
    return request


def _preflight_batch_request_dict(
    request: RequestPreflightBatchRequest,
) -> dict[str, Any]:
    return {
        "schema_version": 1,
        "requests": [
            _preflight_request_dict(declaration)
            for declaration in request.requests
        ],
    }


def _parse_preflight_request(value: Any) -> RequestPreflightRequest:
    raw = _record(value, "preflight_request")
    for key in (
        "schema_version",
        "model",
        "provider",
        "required_capabilities",
        "declared_input_tokens",
        "requested_max_output_tokens",
    ):
        if key not in raw:
            _invalid("preflight_request")
    capabilities = raw["required_capabilities"]
    if not isinstance(capabilities, list):
        _invalid("preflight_request")
    request = RequestPreflightRequest(
        schema_version=raw["schema_version"],
        model=raw["model"],
        provider=raw["provider"],
        required_capabilities=tuple(capabilities),
        declared_input_tokens=raw["declared_input_tokens"],
        requested_max_output_tokens=raw["requested_max_output_tokens"],
    )
    return _normalize_preflight_request(request)


def _parse_preflight(
    body: bytes, expected_request: RequestPreflightRequest
) -> RequestPreflightResponse:
    return _parse_preflight_value(_load_json(body), expected_request)


def _parse_preflight_value(
    value: Any, expected_request: RequestPreflightRequest
) -> RequestPreflightResponse:
    root = _record(value, "preflight_document")
    if (
        root.get("schema_version") != 1
        or root.get("scope") != "request_preflight"
        or root.get("snapshot_scope") != "responding_process"
        or not _canonical_timestamp(root.get("generated_at"))
    ):
        _invalid("preflight_metadata")
    request = _parse_preflight_request(root.get("request"))
    if request != expected_request:
        _invalid("preflight_request_echo")
    resolution = _parse_preflight_resolution(root.get("provider_resolution"), request)
    credential = _parse_preflight_credential(root.get("credential"), resolution)
    support = _parse_preflight_support(
        root.get("model_support"), resolution, request
    )
    limits = _parse_preflight_limits(
        root.get("catalog_limits"), resolution, request
    )
    catalog_cost = _parse_preflight_cost(
        root.get("catalog_cost"), resolution, limits, request
    )
    provider_health = _parse_unknown(
        root.get("provider_health"), "provider_health_not_probed"
    )
    request_acceptance = _parse_unknown(
        root.get("request_acceptance"), "request_acceptance_not_attempted"
    )
    actions = _parse_preflight_actions(
        root.get("actions"), resolution, credential, support, limits
    )
    return RequestPreflightResponse(
        schema_version=1,
        scope="request_preflight",
        snapshot_scope="responding_process",
        generated_at=root["generated_at"],
        request=request,
        provider_resolution=resolution,
        credential=credential,
        model_support=support,
        catalog_limits=limits,
        catalog_cost=catalog_cost,
        provider_health=provider_health,
        request_acceptance=request_acceptance,
        actions=actions,
    )

def _parse_preflight_batch(
    body: bytes, expected_request: RequestPreflightBatchRequest
) -> RequestPreflightBatchResponse:
    root = _record(_load_json(body), "preflight_batch_document")
    if (
        root.get("schema_version") != 1
        or root.get("scope") != "request_preflight_batch"
        or root.get("snapshot_scope") != "responding_process"
        or not _canonical_timestamp(root.get("generated_at"))
        or not isinstance(root.get("documents"), list)
        or len(root["documents"]) != len(expected_request.requests)
        or not isinstance(root.get("limitations"), list)
        or len(root["limitations"]) != 2
    ):
        _invalid("preflight_batch_metadata")
    request_raw = _record(root.get("request"), "preflight_batch_request")
    requests_raw = request_raw.get("requests")
    if request_raw.get("schema_version") != 1 or not isinstance(requests_raw, list):
        _invalid("preflight_batch_request")
    request = RequestPreflightBatchRequest(
        requests=tuple(_parse_preflight_request(item) for item in requests_raw),
        schema_version=1,
    )
    _normalize_preflight_batch_request(request)
    if request != expected_request:
        _invalid("preflight_batch_request_echo")
    documents = tuple(
        _parse_preflight_value(value, expected_request.requests[index])
        for index, value in enumerate(root["documents"])
    )
    if any(document.generated_at != root["generated_at"] for document in documents):
        _invalid("preflight_batch_generated_at")
    limitations = (
        _parse_reason(
            root["limitations"][0], "preflight_batch_single_responder_not_atomic"
        ),
        _parse_reason(
            root["limitations"][1],
            "preflight_batch_provider_execution_not_observed",
        ),
    )
    return RequestPreflightBatchResponse(
        documents=documents,
        generated_at=root["generated_at"],
        limitations=limitations,
        request=request,
        schema_version=1,
        scope="request_preflight_batch",
        snapshot_scope="responding_process",
    )


def _parse_preflight_cost(
    value: Any,
    resolution: PreflightProviderResolution,
    limits: PreflightLimitEvidence,
    request: RequestPreflightRequest,
) -> PreflightCostEvidence:
    raw = _record(value, "preflight_cost")
    numeric_fields = (
        "standard_input_rate_usd_per_million",
        "standard_output_rate_usd_per_million",
        "standard_cost_usd_low",
        "standard_cost_usd_high",
    )
    token_fields = (
        "input_tokens_low",
        "input_tokens_high",
        "output_tokens_low",
        "output_tokens_high",
    )
    if any(field not in raw for field in numeric_fields + token_fields):
        _invalid("preflight_cost")
    state = raw.get("state")
    source = raw.get("source")
    if state == "unknown":
        if source != "not_negotiated" or any(
            raw[field] is not None for field in numeric_fields + token_fields
        ):
            _invalid("preflight_cost")
        return PreflightCostEvidence(
            state="unknown",
            source="not_negotiated",
            standard_input_rate_usd_per_million=None,
            standard_output_rate_usd_per_million=None,
            input_tokens_low=None,
            input_tokens_high=None,
            output_tokens_low=None,
            output_tokens_high=None,
            standard_cost_usd_low=None,
            standard_cost_usd_high=None,
            reason=_parse_reason(
                raw.get("reason"), "preflight_standard_cost_unavailable"
            ),
        )
    if (
        state != "catalog_projection"
        or source != "registered_provider_pricing_catalog"
        or resolution.state != "exact_catalog_match"
        or any(not _nonnegative_float(raw[field]) for field in numeric_fields)
        or any(
            not _nonnegative_int(raw[field])
            or raw[field] > PREFLIGHT_TOKEN_MAX
            for field in token_fields
        )
        or not _positive_int(limits.catalog_max_input_tokens)
        or not _nonnegative_int(limits.catalog_max_output_tokens)
    ):
        _invalid("preflight_cost")
    input_low = raw["input_tokens_low"]
    input_high = raw["input_tokens_high"]
    output_low = raw["output_tokens_low"]
    output_high = raw["output_tokens_high"]
    expected_input = (
        (0, limits.catalog_max_input_tokens)
        if request.declared_input_tokens is None
        else (request.declared_input_tokens, request.declared_input_tokens)
    )
    expected_output_high = (
        request.requested_max_output_tokens
        if request.requested_max_output_tokens is not None
        else limits.catalog_max_output_tokens
    )
    cost_low = float(raw["standard_cost_usd_low"])
    cost_high = float(raw["standard_cost_usd_high"])
    input_rate = float(raw["standard_input_rate_usd_per_million"])
    output_rate = float(raw["standard_output_rate_usd_per_million"])
    if (
        (input_low, input_high) != expected_input
        or output_low != 0
        or output_high != expected_output_high
        or cost_high < cost_low
    ):
        _invalid("preflight_cost")
    expected_low = _projected_standard_cost(
        input_low, output_low, input_rate, output_rate
    )
    expected_high = _projected_standard_cost(
        input_high, output_high, input_rate, output_rate
    )
    if not math.isclose(cost_low, expected_low, rel_tol=1e-12, abs_tol=1e-15) or not math.isclose(
        cost_high, expected_high, rel_tol=1e-12, abs_tol=1e-15
    ):
        _invalid("preflight_cost")
    return PreflightCostEvidence(
        state="catalog_projection",
        source="registered_provider_pricing_catalog",
        standard_input_rate_usd_per_million=input_rate,
        standard_output_rate_usd_per_million=output_rate,
        input_tokens_low=input_low,
        input_tokens_high=input_high,
        output_tokens_low=output_low,
        output_tokens_high=output_high,
        standard_cost_usd_low=cost_low,
        standard_cost_usd_high=cost_high,
        reason=_parse_reason(
            raw.get("reason"), "preflight_standard_cost_catalog_projection"
        ),
    )


def _projected_standard_cost(
    input_tokens: int, output_tokens: int, input_rate: float, output_rate: float
) -> float:
    return (input_tokens * input_rate + output_tokens * output_rate) / 1_000_000


def _parse_preflight_resolution(
    value: Any, request: RequestPreflightRequest
) -> PreflightProviderResolution:
    raw = _record(value, "preflight_resolution")
    state = raw.get("state")
    provider = raw.get("provider")
    source = raw.get("source")
    if state == "exact_catalog_match":
        if request.provider is not None:
            if provider != request.provider or source != "gateway_runtime":
                _invalid("preflight_resolution")
            code = "preflight_exact_provider_model_match"
        else:
            if not _provider_id(provider) or source != "registered_provider_catalog":
                _invalid("preflight_resolution")
            code = "preflight_exact_model_match"
    elif state == "provider_registered_catalog_miss":
        if (
            request.provider is None
            or provider != request.provider
            or source != "gateway_runtime"
        ):
            _invalid("preflight_resolution")
        code = "preflight_provider_registered_model_unlisted"
    elif state == "provider_unregistered":
        if (
            request.provider is None
            or provider is not None
            or source != "gateway_runtime"
        ):
            _invalid("preflight_resolution")
        code = "preflight_provider_unregistered"
    elif state == "dispatch_resolved_catalog_unknown":
        if (
            request.provider is not None
            or not _provider_id(provider)
            or source != "gateway_dispatch_resolution"
        ):
            _invalid("preflight_resolution")
        code = "preflight_dispatch_provider_inferred"
    elif state == "unresolved":
        if provider is not None or source != "gateway_runtime":
            _invalid("preflight_resolution")
        code = "preflight_provider_unresolved"
    else:
        _invalid("preflight_resolution")
    return PreflightProviderResolution(
        state=state,
        provider=provider,
        source=source,
        reason=_parse_reason(raw.get("reason"), code),
    )


def _parse_preflight_credential(
    value: Any, resolution: PreflightProviderResolution
) -> PreflightCredentialEvidence:
    raw = _record(value, "preflight_credential")
    state = raw.get("state")
    source = raw.get("source")
    if resolution.provider is None:
        if state != "unknown" or source != "not_inspected":
            _invalid("preflight_credential")
        code = "preflight_credential_provider_unresolved"
    else:
        expected = {
            "configured": (
                "organization_credential_store",
                "preflight_credential_record_configured",
            ),
            "missing": (
                "organization_credential_store",
                "preflight_credential_record_missing",
            ),
            "unavailable": (
                "organization_credential_store",
                "preflight_credential_store_unavailable",
            ),
            "unknown": (
                "not_inspected",
                "preflight_credential_store_not_configured",
            ),
        }
        match = expected.get(state)
        if match is None or source != match[0]:
            _invalid("preflight_credential")
        code = match[1]
    return PreflightCredentialEvidence(
        state=state,
        source=source,
        reason=_parse_reason(raw.get("reason"), code),
    )


def _parse_preflight_support(
    value: Any,
    resolution: PreflightProviderResolution,
    request: RequestPreflightRequest,
) -> PreflightModelSupportEvidence:
    raw = _record(value, "preflight_support")
    missing_raw = raw.get("missing_capabilities")
    if not isinstance(missing_raw, list):
        _invalid("preflight_support")
    missing = tuple(missing_raw)
    state = raw.get("state")
    source = raw.get("source")
    if resolution.state != "exact_catalog_match":
        if state != "unknown" or source != "not_negotiated" or missing:
            _invalid("preflight_support")
        code = "preflight_model_support_catalog_unknown"
    else:
        if (
            source != "registered_provider_catalog"
            or len(missing) > 8
            or any(
                not isinstance(capability, str)
                or capability not in request.required_capabilities
                for capability in missing
            )
            or len(set(missing)) != len(missing)
        ):
            _invalid("preflight_support")
        if state == "supported_by_catalog" and not missing:
            code = "preflight_required_capabilities_catalog_match"
        elif state == "unsupported_by_catalog" and missing:
            code = "preflight_required_capabilities_catalog_miss"
        else:
            _invalid("preflight_support")
    return PreflightModelSupportEvidence(
        state=state,
        source=source,
        missing_capabilities=missing,
        reason=_parse_reason(raw.get("reason"), code),
    )


def _parse_preflight_limits(
    value: Any,
    resolution: PreflightProviderResolution,
    request: RequestPreflightRequest,
) -> PreflightLimitEvidence:
    raw = _record(value, "preflight_limits")
    if (
        "catalog_max_input_tokens" not in raw
        or "catalog_max_output_tokens" not in raw
    ):
        _invalid("preflight_limits")
    state = raw.get("state")
    source = raw.get("source")
    max_input = raw["catalog_max_input_tokens"]
    max_output = raw["catalog_max_output_tokens"]
    if resolution.state != "exact_catalog_match" or state == "unknown":
        code = (
            "preflight_catalog_limits_outside_v1_wire"
            if resolution.state == "exact_catalog_match"
            else "preflight_catalog_limits_unknown"
        )
        if (
            state != "unknown"
            or source != "not_negotiated"
            or max_input is not None
            or max_output is not None
        ):
            _invalid("preflight_limits")
    else:
        if (
            not _positive_int(max_input)
            or max_input > PREFLIGHT_TOKEN_MAX
            or not _nonnegative_int(max_output)
            or max_output > PREFLIGHT_TOKEN_MAX
        ):
            _invalid("preflight_limits")
        no_values = (
            request.declared_input_tokens is None
            and request.requested_max_output_tokens is None
        )
        if no_values:
            if state != "not_evaluated" or source != "caller_not_supplied":
                _invalid("preflight_limits")
            code = "preflight_declared_tokens_not_supplied"
        else:
            exceeds = (
                request.declared_input_tokens is not None
                and request.declared_input_tokens > max_input
            ) or (
                request.requested_max_output_tokens is not None
                and request.requested_max_output_tokens > max_output
            )
            expected_state, code = (
                (
                    "exceeds_catalog_metadata",
                    "preflight_declared_tokens_exceed_catalog",
                )
                if exceeds
                else (
                    "within_catalog_metadata",
                    "preflight_declared_tokens_within_catalog",
                )
            )
            if state != expected_state or source != "registered_provider_catalog":
                _invalid("preflight_limits")
    return PreflightLimitEvidence(
        state=state,
        source=source,
        catalog_max_input_tokens=max_input,
        catalog_max_output_tokens=max_output,
        reason=_parse_reason(raw.get("reason"), code),
    )


def _parse_preflight_actions(
    value: Any,
    resolution: PreflightProviderResolution,
    credential: PreflightCredentialEvidence,
    support: PreflightModelSupportEvidence,
    limits: PreflightLimitEvidence,
) -> tuple[PreflightAction, ...]:
    if not isinstance(value, list):
        _invalid("preflight_actions")
    expected: list[tuple[str, bool, str]] = []
    if resolution.provider is None:
        expected.append(
            (
                "choose_registered_provider_or_model",
                True,
                "preflight_action_provider_required",
            )
        )
    if credential.state == "missing":
        expected.append(
            (
                "configure_provider_credential",
                True,
                "preflight_action_configure_credential",
            )
        )
    elif credential.state == "unavailable":
        expected.append(
            (
                "retry_preflight_or_contact_operator",
                True,
                "preflight_action_retry_credential_check",
            )
        )
    if support.state == "unsupported_by_catalog":
        expected.append(
            (
                "change_model_or_required_capabilities",
                True,
                "preflight_action_change_capability_request",
            )
        )
    if limits.state == "exceeds_catalog_metadata":
        expected.append(
            (
                "reduce_declared_tokens_or_choose_model",
                True,
                "preflight_action_reduce_declared_tokens",
            )
        )
    expected.append(
        (
            "execute_request_and_handle_result",
            False,
            "preflight_action_real_request_authoritative",
        )
    )
    if len(value) != len(expected):
        _invalid("preflight_actions")
    actions = []
    for raw_action, (code, required, reason_code) in zip(value, expected):
        action = _record(raw_action, "preflight_actions")
        if (
            action.get("code") != code
            or action.get("required_before_request") is not required
        ):
            _invalid("preflight_actions")
        actions.append(
            PreflightAction(
                code=code,
                required_before_request=required,
                reason=_parse_reason(action.get("reason"), reason_code),
            )
        )
    return tuple(actions)


def _provider_id(value: Any) -> bool:
    return _bounded_text(value, 64) and all(
        ch.isascii() and (ch.islower() or ch.isdigit() or ch in "_-")
        for ch in value
    )


def _parse_current_tier(value: Any) -> TierEvidence:
    evidence = _record(value, "current_tier")
    if evidence.get("state") != "known" or _tier_rank(evidence.get("value")) < 0:
        _invalid("current_tier")
    if evidence.get("source") == "authenticated_api_key":
        code = "effective_tier_from_authenticated_key"
    elif evidence.get("source") == "gateway_free_default" and evidence.get("value") == "free":
        code = "effective_tier_defaulted_to_free"
    else:
        _invalid("current_tier")
    return TierEvidence(
        state="known",
        value=evidence["value"],
        source=evidence["source"],
        reason=_parse_reason(evidence.get("reason"), code),
    )


def _parse_minimum_tier(value: Any) -> TierEvidence:
    evidence = _record(value, "minimum_tier")
    if (
        evidence.get("state") != "known"
        or evidence.get("source") != "gateway_runtime"
        or _tier_rank(evidence.get("value")) < 0
    ):
        _invalid("minimum_tier")
    return TierEvidence(
        state="known",
        value=evidence["value"],
        source="gateway_runtime",
        reason=_parse_reason(evidence.get("reason"), "fusion_minimum_tier_configured"),
    )


def _tier_rank(value: Any) -> int:
    try:
        return ("free", "pro", "team", "scale").index(value)
    except ValueError:
        return -1


def _parse_unknown(value: Any, code: str) -> UnknownEvidence:
    evidence = _record(value, "unknown_evidence")
    if evidence.get("state") != "unknown" or evidence.get("source") != "not_negotiated":
        _invalid("unknown_evidence")
    return UnknownEvidence(
        state="unknown",
        source="not_negotiated",
        reason=_parse_reason(evidence.get("reason"), code),
    )


def _parse_schema_version(
    value: Any, state: str, version: Optional[int], code: str
) -> SchemaVersionEvidence:
    evidence = _record(value, "schema_versions")
    if (
        evidence.get("state") != state
        or evidence.get("version") != version
        or evidence.get("source") != "gateway_runtime"
    ):
        _invalid("schema_versions")
    return SchemaVersionEvidence(
        state=state,
        version=version,
        source="gateway_runtime",
        reason=_parse_reason(evidence.get("reason"), code),
    )


def _parse_reason(value: Any, expected_code: str) -> CapabilityReason:
    reason = _record(value, "reason")
    code = reason.get("code")
    message = reason.get("message")
    if (
        code != expected_code
        or not _bounded_text(code, 96)
        or any(
            not (ch.isascii() and (ch.islower() or ch.isdigit() or ch in "_-:"))
            for ch in code
        )
        or not _bounded_text(message, 600)
        or any(unicodedata.category(ch) == "Cc" for ch in message)
    ):
        _invalid("reason")
    return CapabilityReason(code=code, message=message)


def _canonical_timestamp(value: Any) -> bool:
    if (
        not _bounded_text(value, 64)
        or len(value) != 24
        or value[10] != "T"
        or value[19] != "."
        or not value.endswith("Z")
    ):
        return False
    try:
        parsed = datetime.fromisoformat(value[:-1] + "+00:00")
    except ValueError:
        return False
    return (
        parsed.tzinfo is not None
        and parsed.astimezone(timezone.utc).isoformat(timespec="milliseconds")
        .replace("+00:00", "Z")
        == value
    )


def _canonical_effective_at(value: Any) -> bool:
    if (
        not _bounded_text(value, 64)
        or len(value) != 20
        or value[10] != "T"
        or not value.endswith("Z")
    ):
        return False
    try:
        parsed = datetime.fromisoformat(value[:-1] + "+00:00")
    except ValueError:
        return False
    return (
        parsed.tzinfo is not None
        and parsed.astimezone(timezone.utc).isoformat(timespec="seconds")
        .replace("+00:00", "Z")
        == value
    )


def _record(value: Any, code: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        _invalid(code)
    return value


def _nonempty(value: Any) -> bool:
    return isinstance(value, str) and bool(value) and value.strip() == value


def _bounded_text(value: Any, limit: int) -> bool:
    return _nonempty(value) and len(value.encode("utf-8")) <= limit


def _positive_int(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def _nonnegative_int(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def _nonnegative_float(value: Any) -> bool:
    return (
        isinstance(value, (int, float))
        and not isinstance(value, bool)
        and math.isfinite(float(value))
        and float(value) >= 0
    )


def _invalid(code: str) -> None:
    raise GatewayMetadataError(code)
