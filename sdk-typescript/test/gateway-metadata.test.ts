import { createHash } from 'node:crypto';

import { afterEach, describe, expect, it, vi } from 'vitest';

import {
  GatewayMetadataError,
  TokenTrimmer,
  type RequestPreflightBatchRequest,
  type RequestPreflightRequest,
} from '../src/index.js';

const BASE = 'http://127.0.0.1:18080/v1';

function jsonResponse(body: unknown, status = 200, extra: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: {
      'content-type': 'application/json',
      'cache-control': 'private, no-store',
      'x-content-type-options': 'nosniff',
      ...extra,
    },
  });
}

function modelDocument() {
  const data = [
    {
      id: 'gpt-4o-mini',
      object: 'model',
      owned_by: 'openai',
      tokentrimmer: {
        provider: 'openai',
        pricing: null,
        capabilities: ['text', 'tools', 'json_mode', 'streaming'],
        max_input_tokens: 128_000,
        max_output_tokens: 16_384,
      },
    },
  ];
  return {
    object: 'list',
    data,
    tokentrimmer: {
      schema_version: 1,
      snapshot_scope: 'responding_process',
      source: 'registered_provider_catalog',
      snapshot_sha256: createHash('sha256').update(JSON.stringify(data)).digest('hex'),
      limitations: {
        provider_credentials: 'not_inspected',
        provider_health: 'not_probed',
        request_acceptance: 'not_negotiated',
        fleet_consistency: 'not_attested',
      },
    },
  };
}

function pricedModelDocument() {
  const data = [
    {
      id: 'priced-model',
      object: 'model',
      owned_by: 'provider',
      tokentrimmer: {
        provider: 'provider',
        pricing: {
          input_per_million: 2.5,
          output_per_million: 10,
          cached_input_per_million: null,
          cache_write_per_million: null,
          batch_input_per_million: null,
          batch_output_per_million: null,
          flex_input_per_million: null,
          flex_output_per_million: null,
          prompt_cache_min_tokens: null,
          effective_at: '2026-07-01T00:00:00Z',
        },
        capabilities: ['text'],
        max_input_tokens: 1_024,
        max_output_tokens: 256,
      },
    },
  ];
  // serde_json retains the f64 type marker for an integer-valued rate.
  const rustData = JSON.stringify(data).replace(
    '"output_per_million":10,',
    '"output_per_million":10.0,',
  );
  return {
    object: 'list',
    data,
    tokentrimmer: {
      ...modelDocument().tokentrimmer,
      snapshot_sha256: createHash('sha256').update(rustData).digest('hex'),
    },
  };
}

function reason(code: string) {
  return { code, message: 'Bounded responder explanation.' };
}

function unknown(code: string) {
  return { state: 'unknown', source: 'not_negotiated', reason: reason(code) };
}

function capabilityDocument() {
  return {
    schema_version: 1,
    scope: 'gateway_runtime',
    snapshot_scope: 'responding_process',
    generated_at: '2026-07-26T12:34:56.000Z',
    features: {
      fusion: {
        enabled: {
          state: 'enabled',
          source: 'gateway_runtime',
          reason: reason('fusion_kill_switch_enabled'),
        },
        access: { state: 'available', reason: reason('fusion_gateway_gate_passed') },
        current_tier: {
          state: 'known',
          value: 'pro',
          source: 'authenticated_api_key',
          reason: reason('effective_tier_from_authenticated_key'),
        },
        minimum_tier: {
          state: 'known',
          value: 'pro',
          source: 'gateway_runtime',
          reason: reason('fusion_minimum_tier_configured'),
        },
        limits: {
          member_models_max: {
            value: 8,
            enforcement: 'gateway_runtime',
            reason: reason('fusion_member_cap'),
          },
        },
      },
    },
    provider_credentials: unknown('provider_credentials_not_inspected'),
    provider_health: unknown('provider_health_not_probed'),
    model_support: unknown('model_support_not_negotiated'),
    modality_support: unknown('modality_support_not_negotiated'),
    schema_versions: {
      capabilities_document: {
        state: 'known',
        version: 1,
        source: 'gateway_runtime',
        reason: reason('capabilities_document_version'),
      },
      fusion_request: {
        state: 'unversioned',
        version: null,
        source: 'gateway_runtime',
        reason: reason('fusion_request_schema_not_versioned'),
      },
    },
  };
}

function preflightRequest(): RequestPreflightRequest {
  return {
    schema_version: 1,
    model: 'gpt-4o-mini',
    provider: null,
    required_capabilities: ['text', 'tools'],
    declared_input_tokens: 1_024,
    requested_max_output_tokens: 4_096,
  };
}

function preflightDocument(request = preflightRequest()) {
  return {
    schema_version: 1,
    scope: 'request_preflight',
    snapshot_scope: 'responding_process',
    generated_at: '2026-07-26T12:34:56.000Z',
    request,
    provider_resolution: {
      state: 'exact_catalog_match',
      provider: 'openai',
      source: 'registered_provider_catalog',
      reason: reason('preflight_exact_model_match'),
    },
    credential: {
      state: 'configured',
      source: 'organization_credential_store',
      reason: reason('preflight_credential_record_configured'),
    },
    model_support: {
      state: 'supported_by_catalog',
      source: 'registered_provider_catalog',
      missing_capabilities: [],
      reason: reason('preflight_required_capabilities_catalog_match'),
    },
    catalog_limits: {
      state: 'within_catalog_metadata',
      source: 'registered_provider_catalog',
      catalog_max_input_tokens: 128_000,
      catalog_max_output_tokens: 16_384,
      reason: reason('preflight_declared_tokens_within_catalog'),
    },
    catalog_cost: {
      state: 'catalog_projection',
      source: 'registered_provider_pricing_catalog',
      standard_input_rate_usd_per_million: 0.15,
      standard_output_rate_usd_per_million: 0.60,
      input_tokens_low: 1_024,
      input_tokens_high: 1_024,
      output_tokens_low: 0,
      output_tokens_high: 4_096,
      standard_cost_usd_low: 0.000_153_6,
      standard_cost_usd_high: 0.002_611_2,
      reason: reason('preflight_standard_cost_catalog_projection'),
    },
    provider_health: unknown('provider_health_not_probed'),
    request_acceptance: unknown('request_acceptance_not_attempted'),
    actions: [
      {
        code: 'execute_request_and_handle_result',
        required_before_request: false,
        reason: reason('preflight_action_real_request_authoritative'),
      },
    ],
  };
}

function preflightBatchRequest(): RequestPreflightBatchRequest {
  return {
    schema_version: 1,
    requests: [preflightRequest(), preflightRequest()],
  };
}

function preflightBatchDocument(request = preflightBatchRequest()) {
  return {
    schema_version: 1,
    scope: 'request_preflight_batch',
    snapshot_scope: 'responding_process',
    generated_at: '2026-07-26T12:34:56.000Z',
    request,
    documents: request.requests.map((declaration) => preflightDocument(declaration)),
    limitations: [
      reason('preflight_batch_single_responder_not_atomic'),
      reason('preflight_batch_provider_execution_not_observed'),
    ],
  };
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe('bounded generated gateway metadata', () => {
  it('reads and validates the anonymous model contract without a bearer', async () => {
    const fetchMock = vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
      expect(init?.redirect).toBe('error');
      expect(new Headers(init?.headers).has('authorization')).toBe(false);
      return jsonResponse(modelDocument());
    });
    vi.stubGlobal('fetch', fetchMock);

    const document = await new TokenTrimmer({
      apiKey: 'configured-but-anonymous',
      baseURL: BASE,
    }).gateway.models();

    expect(document.data[0]?.tokentrimmer.max_input_tokens).toBe(128_000);
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it('sends one live bearer and validates capability reason consistency', async () => {
    const fetchMock = vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
      expect(new Headers(init?.headers).get('authorization')).toBe('Bearer tt_live_test');
      return jsonResponse(capabilityDocument());
    });
    vi.stubGlobal('fetch', fetchMock);

    const document = await new TokenTrimmer({
      apiKey: 'tt_live_test',
      baseURL: BASE,
    }).gateway.capabilities();

    expect(document.features.fusion.access.state).toBe('available');
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it('posts and validates one request-specific non-dispatching preflight', async () => {
    const declaration = preflightRequest();
    const fetchMock = vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
      expect(init?.method).toBe('POST');
      expect(init?.redirect).toBe('error');
      expect(new Headers(init?.headers).get('authorization')).toBe('Bearer tt_live_test');
      expect(new Headers(init?.headers).get('content-type')).toBe('application/json');
      expect(JSON.parse(String(init?.body))).toEqual(declaration);
      return jsonResponse(preflightDocument(declaration));
    });
    vi.stubGlobal('fetch', fetchMock);

    const document = await new TokenTrimmer({
      apiKey: 'tt_live_test',
      baseURL: BASE,
    }).gateway.preflight(declaration);

    expect(document.credential.state).toBe('configured');
    expect(document.catalog_cost.standard_cost_usd_high).toBe(0.002_611_2);
    expect(document.provider_health.state).toBe('unknown');
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it('posts one ordered responder-local preflight batch', async () => {
    const declaration = preflightBatchRequest();
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      expect(String(input)).toBe(`${BASE}/capabilities/preflight/batch`);
      expect(JSON.parse(String(init?.body))).toEqual(declaration);
      return jsonResponse(preflightBatchDocument(declaration));
    });
    vi.stubGlobal('fetch', fetchMock);

    const document = await new TokenTrimmer({
      apiKey: 'tt_live_test',
      baseURL: BASE,
    }).gateway.preflightBatch(declaration);

    expect(document.documents).toHaveLength(2);
    expect(document.documents[1]?.generated_at).toBe(document.generated_at);
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it('rejects unsafe declarations before fetch and contradictory preflight evidence', async () => {
    const fetchMock = vi.fn(async () => {
      const contradiction = preflightDocument();
      contradiction.request_acceptance.state = 'available';
      return jsonResponse(contradiction);
    });
    vi.stubGlobal('fetch', fetchMock);
    const client = new TokenTrimmer({ apiKey: 'tt_live_test', baseURL: BASE });

    await expect(
      client.gateway.preflight({
        ...preflightRequest(),
        requested_max_output_tokens: 0,
      }),
    ).rejects.toMatchObject({ code: 'preflight_request' });
    expect(fetchMock).not.toHaveBeenCalled();

    await expect(client.gateway.preflight(preflightRequest())).rejects.toMatchObject({
      code: 'unknown_evidence',
    });
    expect(fetchMock).toHaveBeenCalledTimes(1);

    fetchMock.mockImplementationOnce(async () => {
      const contradiction = preflightDocument();
      contradiction.catalog_cost.standard_cost_usd_high = 999;
      return jsonResponse(contradiction);
    });
    await expect(client.gateway.preflight(preflightRequest())).rejects.toMatchObject({
      code: 'preflight_cost',
    });
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });

  it('recomputes the Rust digest for integer-valued f64 pricing', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => jsonResponse(pricedModelDocument())));
    const document = await new TokenTrimmer({
      apiKey: 'configured-but-anonymous',
      baseURL: BASE,
    }).gateway.models();
    expect(document.data[0]?.tokentrimmer.pricing?.output_per_million).toBe(10);
  });

  it('rejects unsafe capability inputs before fetch', async () => {
    const fetchMock = vi.fn();
    vi.stubGlobal('fetch', fetchMock);

    await expect(
      new TokenTrimmer({ apiKey: 'tt_test_x', baseURL: BASE }).gateway.capabilities(),
    ).rejects.toMatchObject({ code: 'api_key' });
    await expect(
      new TokenTrimmer({
        apiKey: 'tt_live_test',
        baseURL: 'http://gateway.example/v1',
      }).gateway.capabilities(),
    ).rejects.toMatchObject({ code: 'base_url' });
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('rejects redirects without issuing a second fetch', async () => {
    const fetchMock = vi.fn(async () => new Response('', { status: 302 }));
    vi.stubGlobal('fetch', fetchMock);

    await expect(
      new TokenTrimmer({ apiKey: 'tt_live_test', baseURL: BASE }).gateway.capabilities(),
    ).rejects.toBeInstanceOf(GatewayMetadataError);
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it('rejects a digest mismatch and contradictory capability evidence', async () => {
    const badModel = modelDocument();
    badModel.tokentrimmer.snapshot_sha256 = '0'.repeat(64);
    const contradiction = capabilityDocument();
    contradiction.features.fusion.access.state = 'unavailable';
    contradiction.features.fusion.access.reason = reason('fusion_disabled');
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(jsonResponse(badModel))
      .mockResolvedValueOnce(jsonResponse(contradiction));
    vi.stubGlobal('fetch', fetchMock);
    const client = new TokenTrimmer({ apiKey: 'tt_live_test', baseURL: BASE });

    await expect(client.gateway.models()).rejects.toMatchObject({ code: 'snapshot_mismatch' });
    await expect(client.gateway.capabilities()).rejects.toMatchObject({ code: 'fusion_access' });
  });

  it('caps bodies and never exposes remote failure prose', async () => {
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(
        new Response('private provider diagnostic', {
          status: 503,
          headers: { 'content-type': 'text/plain' },
        }),
      )
      .mockResolvedValueOnce(
        jsonResponse('x'.repeat(64 * 1024 + 1), 200, {
          'content-length': String(64 * 1024 + 1),
        }),
      );
    vi.stubGlobal('fetch', fetchMock);
    const client = new TokenTrimmer({ apiKey: 'tt_live_test', baseURL: BASE });

    const status = await client.gateway.capabilities().catch((error: unknown) => error);
    expect(status).toMatchObject({ code: 'status', status: 503 });
    expect(String(status)).not.toContain('provider diagnostic');
    await expect(client.gateway.capabilities()).rejects.toMatchObject({
      code: 'response_too_large',
    });
  });
});
