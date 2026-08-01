/**
 * Bounded reads of TokenTrimmer's responder-scoped metadata contracts.
 *
 * The wire types are generated from Rust. Runtime checks remain explicit:
 * generated TypeScript alone cannot enforce safe integers, finite prices,
 * reason-code consistency, response headers, byte ceilings, or redirects.
 */

import type {
  CapabilityReason,
  GatewayCapabilitiesDocument,
  ModelEntry,
  ModelPricing,
  ModelsResponse,
  RequestPreflightBatchRequest,
  RequestPreflightBatchResponse,
  RequestPreflightRequest,
  RequestPreflightResponse,
  TierEvidence,
  UnknownEvidence,
} from './product-contracts.generated.js';

export type {
  GatewayCapabilitiesDocument,
  ModelEntry,
  ModelPricing,
  ModelsResponse,
  PreflightCostEvidence,
  RequestPreflightBatchRequest,
  RequestPreflightBatchResponse,
  RequestPreflightRequest,
  RequestPreflightResponse,
} from './product-contracts.generated.js';

const MODELS_MAX_BYTES = 256 * 1024;
const CAPABILITIES_MAX_BYTES = 64 * 1024;
const PREFLIGHT_MAX_BYTES = 64 * 1024;
const PREFLIGHT_TOKEN_MAX = 4_294_967_295;
const REQUEST_TIMEOUT_MS = 5_000;
const CAPABILITY_CODES = new Set([
  'text',
  'vision',
  'audio',
  'tools',
  'json_mode',
  'streaming',
  'reasoning',
  'prompt_caching',
]);

/** A fixed local failure code; response bodies and responder prose are omitted. */
export class GatewayMetadataError extends Error {
  constructor(
    public readonly code: string,
    public readonly status: number | null = null,
  ) {
    super(status === null ? `gateway metadata error: ${code}` : `gateway metadata HTTP ${status}`);
    this.name = 'GatewayMetadataError';
  }
}

/**
 * Read-only gateway metadata attached at `client.gateway`.
 *
 * `models()` is anonymous. `capabilities()` uses the configured `tt_live_*`
 * key, but only against HTTPS or literal-loopback HTTP. Both reads use the
 * runtime's independent Fetch implementation with `redirect: "error"` rather
 * than inheriting OpenAI resource retries or redirect behavior.
 */
export class GatewayMetadata {
  constructor(
    private readonly baseURL: string,
    private readonly apiKey: string,
  ) {}

  /** One responder's catalog metadata; never credential/readiness evidence. */
  async models(): Promise<ModelsResponse> {
    const endpoint = endpointURL(this.baseURL, 'models', false);
    const bytes = await request(endpoint, {}, MODELS_MAX_BYTES);
    const document = await parseModels(bytes);
    await validateModelDigest(document);
    return document;
  }

  /** One authenticated responder's Fusion switch/tier evidence. */
  async capabilities(): Promise<GatewayCapabilitiesDocument> {
    if (!this.apiKey.startsWith('tt_live_') || this.apiKey.length === 'tt_live_'.length) {
      throw new GatewayMetadataError('api_key');
    }
    const endpoint = endpointURL(this.baseURL, 'capabilities', true);
    const bytes = await request(
      endpoint,
      { authorization: `Bearer ${this.apiKey}` },
      CAPABILITIES_MAX_BYTES,
    );
    return parseCapabilities(bytes);
  }

  /**
   * Compare one request with local responder facts without provider I/O.
   *
   * The result is not credential validity, provider health/acceptance,
   * tokenization, a provider-accepted hard limit, reservation, or execution
   * evidence.
   */
  async preflight(input: RequestPreflightRequest): Promise<RequestPreflightResponse> {
    if (!this.apiKey.startsWith('tt_live_') || this.apiKey.length === 'tt_live_'.length) {
      throw new GatewayMetadataError('api_key');
    }
    const declaration = normalizePreflightRequest(input);
    const endpoint = endpointURL(this.baseURL, 'capabilities/preflight', true);
    const bytes = await request(
      endpoint,
      { authorization: `Bearer ${this.apiKey}` },
      PREFLIGHT_MAX_BYTES,
      JSON.stringify(declaration),
    );
    return parsePreflight(bytes, declaration);
  }

  /**
   * Evaluate 1–9 ordered declarations on one responding process.
   *
   * This removes cross-process drift, but is not an atomic credential/config
   * snapshot, executable panel admission, provider probe, or reservation.
   */
  async preflightBatch(
    input: RequestPreflightBatchRequest,
  ): Promise<RequestPreflightBatchResponse> {
    if (!this.apiKey.startsWith('tt_live_') || this.apiKey.length === 'tt_live_'.length) {
      throw new GatewayMetadataError('api_key');
    }
    const declaration = normalizePreflightBatchRequest(input);
    const endpoint = endpointURL(this.baseURL, 'capabilities/preflight/batch', true);
    const bytes = await request(
      endpoint,
      { authorization: `Bearer ${this.apiKey}` },
      PREFLIGHT_MAX_BYTES,
      JSON.stringify(declaration),
    );
    return parsePreflightBatch(bytes, declaration);
  }
}

async function request(
  endpoint: URL,
  headers: Record<string, string>,
  limit: number,
  body?: string,
): Promise<Uint8Array> {
  const fetchImpl = globalThis.fetch;
  if (typeof fetchImpl !== 'function') throw new GatewayMetadataError('fetch_unavailable');

  let response: Response;
  try {
    response = await fetchImpl(endpoint, {
      method: body === undefined ? 'GET' : 'POST',
      headers: {
        accept: 'application/json',
        ...(body === undefined ? {} : { 'content-type': 'application/json' }),
        ...headers,
      },
      ...(body === undefined ? {} : { body }),
      redirect: 'error',
      cache: 'no-store',
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    });
  } catch {
    throw new GatewayMetadataError('request_failed');
  }

  if (response.url && response.url !== endpoint.href) {
    await readBounded(response, limit);
    throw new GatewayMetadataError('redirect');
  }
  if (response.status >= 300 && response.status < 400) {
    await readBounded(response, limit);
    throw new GatewayMetadataError('redirect');
  }
  if (!response.ok) {
    await readBounded(response, limit);
    throw new GatewayMetadataError('status', response.status);
  }

  const headerError = validateHeaders(response.headers);
  const bytes = await readBounded(response, limit);
  if (headerError) throw headerError;
  return bytes;
}

function validateHeaders(headers: Headers): GatewayMetadataError | null {
  const contentType = headers.get('content-type')?.split(';', 1)[0]?.trim().toLowerCase();
  if (contentType !== 'application/json') return new GatewayMetadataError('content_type');
  const cacheControl = headers
    .get('cache-control')
    ?.split(',')
    .some((value) => value.trim().toLowerCase() === 'no-store');
  if (!cacheControl) return new GatewayMetadataError('cache_control');
  if (headers.get('x-content-type-options')?.toLowerCase() !== 'nosniff') {
    return new GatewayMetadataError('content_type_options');
  }
  return null;
}

async function readBounded(response: Response, limit: number): Promise<Uint8Array> {
  const declared = response.headers.get('content-length');
  if (declared !== null && /^\d+$/.test(declared) && Number(declared) > limit) {
    throw new GatewayMetadataError('response_too_large');
  }
  if (response.body === null) return new Uint8Array();

  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      if (value.length > limit - total) {
        await reader.cancel();
        throw new GatewayMetadataError('response_too_large');
      }
      chunks.push(value);
      total += value.length;
    }
  } catch (error) {
    if (error instanceof GatewayMetadataError) throw error;
    throw new GatewayMetadataError('response_read');
  }

  const bytes = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.length;
  }
  return bytes;
}

function endpointURL(baseURL: string, path: string, authenticated: boolean): URL {
  let endpoint: URL;
  try {
    endpoint = new URL(baseURL);
  } catch {
    throw new GatewayMetadataError('base_url');
  }
  if (
    endpoint.username !== '' ||
    endpoint.password !== '' ||
    endpoint.search !== '' ||
    endpoint.hash !== ''
  ) {
    throw new GatewayMetadataError('base_url');
  }
  if (
    authenticated &&
    endpoint.protocol !== 'https:' &&
    !(endpoint.protocol === 'http:' && isLiteralLoopback(endpoint.hostname))
  ) {
    throw new GatewayMetadataError('base_url');
  }
  endpoint.pathname = `${endpoint.pathname.replace(/\/+$/, '')}/${path}`;
  return endpoint;
}

function isLiteralLoopback(hostname: string): boolean {
  const host = hostname.replace(/^\[|\]$/g, '');
  if (host === '::1') return true;
  const octets = host.split('.');
  return (
    octets.length === 4 &&
    octets.every((value) => /^\d{1,3}$/.test(value) && Number(value) <= 255) &&
    Number(octets[0]) === 127
  );
}

function responseJSON(bytes: Uint8Array): unknown {
  try {
    const text = new TextDecoder('utf-8', { fatal: true }).decode(bytes);
    return JSON.parse(text) as unknown;
  } catch {
    throw new GatewayMetadataError('invalid_json');
  }
}

async function parseModels(bytes: Uint8Array): Promise<ModelsResponse> {
  const raw = responseJSON(bytes);
  const root = record(raw, 'model_document');
    if (root.object !== 'list' || !Array.isArray(root.data)) invalid('model_document');
    const metadata = record(root.tokentrimmer, 'model_metadata');
    const limitations = record(metadata.limitations, 'model_metadata');
    if (
      metadata.schema_version !== 1 ||
      metadata.snapshot_scope !== 'responding_process' ||
      metadata.source !== 'registered_provider_catalog' ||
      typeof metadata.snapshot_sha256 !== 'string' ||
      !/^[0-9a-f]{64}$/.test(metadata.snapshot_sha256) ||
      limitations.provider_credentials !== 'not_inspected' ||
      limitations.provider_health !== 'not_probed' ||
      limitations.request_acceptance !== 'not_negotiated' ||
      limitations.fleet_consistency !== 'not_attested'
    ) {
      invalid('model_metadata');
    }

    const seen = new Set<string>();
    const data = root.data.map((value) => parseModelEntry(value, seen));
  return {
      object: 'list',
      data,
      tokentrimmer: {
        schema_version: 1,
        snapshot_scope: 'responding_process',
        source: 'registered_provider_catalog',
        snapshot_sha256: metadata.snapshot_sha256,
        limitations: {
          provider_credentials: 'not_inspected',
          provider_health: 'not_probed',
          request_acceptance: 'not_negotiated',
          fleet_consistency: 'not_attested',
        },
      },
  };
}

function parseModelEntry(value: unknown, seen: Set<string>): ModelEntry {
  const entry = record(value, 'model_entry');
  const metadata = record(entry.tokentrimmer, 'model_entry');
  if (
    entry.object !== 'model' ||
    !nonempty(entry.id) ||
    !nonempty(entry.owned_by) ||
    metadata.provider !== entry.owned_by ||
    !Array.isArray(metadata.capabilities) ||
    metadata.capabilities.length === 0 ||
    !metadata.capabilities.every(
      (capability) => typeof capability === 'string' && CAPABILITY_CODES.has(capability),
    ) ||
    !positiveSafeInteger(metadata.max_input_tokens) ||
    !nonnegativeSafeInteger(metadata.max_output_tokens)
  ) {
    invalid('model_entry');
  }
  const identity = `${entry.owned_by}\u0000${entry.id}`;
  if (seen.has(identity)) invalid('duplicate_model');
  seen.add(identity);

  return {
    id: entry.id,
    object: 'model',
    owned_by: entry.owned_by,
    tokentrimmer: {
      provider: entry.owned_by,
      pricing: metadata.pricing === null ? null : parsePricing(metadata.pricing),
      capabilities: metadata.capabilities as ModelEntry['tokentrimmer']['capabilities'],
      max_input_tokens: metadata.max_input_tokens,
      max_output_tokens: metadata.max_output_tokens,
    },
  };
}

function parsePricing(value: unknown): ModelPricing {
  const pricing = record(value, 'pricing');
  const required = ['input_per_million', 'output_per_million'] as const;
  const optional = [
    'cached_input_per_million',
    'cache_write_per_million',
    'batch_input_per_million',
    'batch_output_per_million',
    'flex_input_per_million',
    'flex_output_per_million',
  ] as const;
  for (const key of required) {
    if (!(key in pricing) || !nonnegativeFinite(pricing[key])) invalid('pricing');
  }
  for (const key of optional) {
    if (
      !(key in pricing) ||
      (pricing[key] !== null && !nonnegativeFinite(pricing[key]))
    ) {
      invalid('pricing');
    }
  }
  if (
    !('prompt_cache_min_tokens' in pricing) ||
    pricing.prompt_cache_min_tokens !== null &&
    !nonnegativeSafeInteger(pricing.prompt_cache_min_tokens)
  ) {
    invalid('pricing');
  }
  if (!canonicalEffectiveAt(pricing.effective_at)) invalid('pricing');
  return {
    input_per_million: pricing.input_per_million,
    output_per_million: pricing.output_per_million,
    cached_input_per_million: pricing.cached_input_per_million,
    cache_write_per_million: pricing.cache_write_per_million,
    batch_input_per_million: pricing.batch_input_per_million,
    batch_output_per_million: pricing.batch_output_per_million,
    flex_input_per_million: pricing.flex_input_per_million,
    flex_output_per_million: pricing.flex_output_per_million,
    prompt_cache_min_tokens: pricing.prompt_cache_min_tokens,
    effective_at: pricing.effective_at,
  } as ModelPricing;
}

async function validateModelDigest(document: ModelsResponse): Promise<void> {
  const bytes = new TextEncoder().encode(canonicalModelData(document.data));
  const crypto = globalThis.crypto;
  if (!crypto?.subtle) throw new GatewayMetadataError('digest_unavailable');
  let digest: Uint8Array;
  try {
    digest = new Uint8Array(await crypto.subtle.digest('SHA-256', bytes));
  } catch {
    throw new GatewayMetadataError('digest_unavailable');
  }
  const actual = [...digest].map((value) => value.toString(16).padStart(2, '0')).join('');
  if (actual !== document.tokentrimmer.snapshot_sha256) invalid('snapshot_mismatch');
}

function canonicalModelData(entries: ModelEntry[]): string {
  return `[${entries.map(canonicalModelEntry).join(',')}]`;
}

function canonicalModelEntry(entry: ModelEntry): string {
  const metadata = entry.tokentrimmer;
  return (
    `{"id":${JSON.stringify(entry.id)},"object":"model","owned_by":${JSON.stringify(entry.owned_by)},` +
    `"tokentrimmer":{"provider":${JSON.stringify(metadata.provider)},"pricing":` +
    `${metadata.pricing == null ? 'null' : canonicalPricing(metadata.pricing)},` +
    `"capabilities":${JSON.stringify(metadata.capabilities)},` +
    `"max_input_tokens":${metadata.max_input_tokens},` +
    `"max_output_tokens":${metadata.max_output_tokens}}}`
  );
}

function canonicalPricing(pricing: ModelPricing): string {
  const optionalFloat = (value: number | null | undefined): string =>
    value == null ? 'null' : rustFloat(value);
  return (
    `{"input_per_million":${rustFloat(pricing.input_per_million)},` +
    `"output_per_million":${rustFloat(pricing.output_per_million)},` +
    `"cached_input_per_million":${optionalFloat(pricing.cached_input_per_million)},` +
    `"cache_write_per_million":${optionalFloat(pricing.cache_write_per_million)},` +
    `"batch_input_per_million":${optionalFloat(pricing.batch_input_per_million)},` +
    `"batch_output_per_million":${optionalFloat(pricing.batch_output_per_million)},` +
    `"flex_input_per_million":${optionalFloat(pricing.flex_input_per_million)},` +
    `"flex_output_per_million":${optionalFloat(pricing.flex_output_per_million)},` +
    `"prompt_cache_min_tokens":${pricing.prompt_cache_min_tokens ?? 'null'},` +
    `"effective_at":${JSON.stringify(pricing.effective_at)}}`
  );
}

function rustFloat(value: number): string {
  return Number.isInteger(value) ? `${String(value)}.0` : String(value);
}

async function parseCapabilities(bytes: Uint8Array): Promise<GatewayCapabilitiesDocument> {
  const raw = responseJSON(bytes);
  const document = record(raw, 'capability_document');
    if (
      document.schema_version !== 1 ||
      document.scope !== 'gateway_runtime' ||
      document.snapshot_scope !== 'responding_process' ||
      !canonicalTimestamp(document.generated_at)
    ) {
      invalid('capability_metadata');
    }
    const features = record(document.features, 'capability_document');
    const fusion = record(features.fusion, 'fusion');
    const enabled = record(fusion.enabled, 'fusion');
    const access = record(fusion.access, 'fusion');
    const currentTier = record(fusion.current_tier, 'fusion') as unknown as TierEvidence;
    const minimumTier = record(fusion.minimum_tier, 'fusion') as unknown as TierEvidence;
    const limits = record(fusion.limits, 'fusion');
    const memberLimit = record(limits.member_models_max, 'fusion');

    let switchEnabled: boolean;
    if (enabled.state === 'enabled') {
      validateEvidenceReason(enabled, 'gateway_runtime', 'fusion_kill_switch_enabled');
      switchEnabled = true;
    } else if (enabled.state === 'disabled') {
      validateEvidenceReason(enabled, 'gateway_runtime', 'fusion_kill_switch_disabled');
      switchEnabled = false;
    } else {
      invalid('fusion_enabled');
    }
    const currentRank = validateCurrentTier(currentTier);
    const minimumRank = validateMinimumTier(minimumTier);
    const expectedAccess = !switchEnabled
      ? ['unavailable', 'fusion_disabled']
      : currentRank < minimumRank
        ? ['unavailable', 'fusion_tier_below_minimum']
        : ['available', 'fusion_gateway_gate_passed'];
    if (access.state !== expectedAccess[0]) invalid('fusion_access');
    validateReason(record(access.reason, 'reason') as unknown as CapabilityReason, expectedAccess[1]!);
    if (
      !positiveSafeInteger(memberLimit.value) ||
      memberLimit.enforcement !== 'gateway_runtime'
    ) {
      invalid('member_models_max');
    }
    validateReason(
      record(memberLimit.reason, 'reason') as unknown as CapabilityReason,
      'fusion_member_cap',
    );

    validateUnknown(document.provider_credentials, 'provider_credentials_not_inspected');
    validateUnknown(document.provider_health, 'provider_health_not_probed');
    validateUnknown(document.model_support, 'model_support_not_negotiated');
    validateUnknown(document.modality_support, 'modality_support_not_negotiated');

    const versions = record(document.schema_versions, 'schema_versions');
    validateSchemaVersion(
      versions.capabilities_document,
      'known',
      1,
      'capabilities_document_version',
    );
    validateSchemaVersion(
      versions.fusion_request,
      'unversioned',
      null,
      'fusion_request_schema_not_versioned',
    );
  return raw as GatewayCapabilitiesDocument;
}

function normalizePreflightRequest(value: RequestPreflightRequest): RequestPreflightRequest {
  const input = record(value, 'preflight_request');
  if (
    input.schema_version !== 1 ||
    typeof input.model !== 'string' ||
    !boundedText(input.model, 256) ||
    /[\u0000-\u001f\u007f-\u009f]/u.test(input.model) ||
    !Array.isArray(input.required_capabilities) ||
    input.required_capabilities.length > 8 ||
    !input.required_capabilities.every(
      (capability) => typeof capability === 'string' && CAPABILITY_CODES.has(capability),
    ) ||
    new Set(input.required_capabilities).size !== input.required_capabilities.length
  ) {
    invalid('preflight_request');
  }
  const provider = input.provider ?? null;
  if (
    provider !== null &&
    (typeof provider !== 'string' ||
      !boundedText(provider, 64) ||
      !/^[a-z0-9_-]+$/.test(provider))
  ) {
    invalid('preflight_request');
  }
  const declared = input.declared_input_tokens ?? null;
  const requested = input.requested_max_output_tokens ?? null;
  if (
    (declared !== null &&
      (!nonnegativeSafeInteger(declared) || declared > PREFLIGHT_TOKEN_MAX)) ||
    (requested !== null &&
      (!positiveSafeInteger(requested) || requested > PREFLIGHT_TOKEN_MAX))
  ) {
    invalid('preflight_request');
  }
  return {
    schema_version: 1,
    model: input.model,
    provider,
    required_capabilities:
      input.required_capabilities as RequestPreflightRequest['required_capabilities'],
    declared_input_tokens: declared,
    requested_max_output_tokens: requested,
  };
}

function normalizePreflightBatchRequest(
  value: RequestPreflightBatchRequest,
): RequestPreflightBatchRequest {
  const input = record(value, 'preflight_batch_request');
  if (
    input.schema_version !== 1 ||
    !Array.isArray(input.requests) ||
    input.requests.length < 1 ||
    input.requests.length > 9
  ) {
    invalid('preflight_batch_request');
  }
  return {
    schema_version: 1,
    requests: input.requests.map((request) =>
      normalizePreflightRequest(request as RequestPreflightRequest),
    ),
  };
}

function parsePreflight(
  bytes: Uint8Array,
  expectedRequest: RequestPreflightRequest,
): RequestPreflightResponse {
  return parsePreflightValue(responseJSON(bytes), expectedRequest);
}

function parsePreflightValue(
  raw: unknown,
  expectedRequest: RequestPreflightRequest,
): RequestPreflightResponse {
  const root = record(raw, 'preflight_document');
  if (
    root.schema_version !== 1 ||
    root.scope !== 'request_preflight' ||
    root.snapshot_scope !== 'responding_process' ||
    !canonicalTimestamp(root.generated_at)
  ) {
    invalid('preflight_metadata');
  }
  const echoed = normalizePreflightRequest(root.request as RequestPreflightRequest);
  if (JSON.stringify(echoed) !== JSON.stringify(expectedRequest)) invalid('preflight_request_echo');

  const resolution = validatePreflightResolution(root.provider_resolution, echoed);
  const credential = validatePreflightCredential(root.credential, resolution);
  const support = validatePreflightSupport(root.model_support, resolution, echoed);
  const limits = validatePreflightLimits(root.catalog_limits, resolution, echoed);
  validatePreflightCost(root.catalog_cost, resolution, limits, echoed);
  validateUnknown(root.provider_health, 'provider_health_not_probed');
  validateUnknown(root.request_acceptance, 'request_acceptance_not_attempted');
  validatePreflightActions(root.actions, resolution, credential, support, limits);
  return raw as RequestPreflightResponse;
}

function parsePreflightBatch(
  bytes: Uint8Array,
  expectedRequest: RequestPreflightBatchRequest,
): RequestPreflightBatchResponse {
  const raw = responseJSON(bytes);
  const root = record(raw, 'preflight_batch_document');
  if (
    root.schema_version !== 1 ||
    root.scope !== 'request_preflight_batch' ||
    root.snapshot_scope !== 'responding_process' ||
    !canonicalTimestamp(root.generated_at) ||
    !Array.isArray(root.documents) ||
    root.documents.length !== expectedRequest.requests.length ||
    !Array.isArray(root.limitations) ||
    root.limitations.length !== 2
  ) {
    invalid('preflight_batch_metadata');
  }
  const echoed = normalizePreflightBatchRequest(root.request as RequestPreflightBatchRequest);
  if (JSON.stringify(echoed) !== JSON.stringify(expectedRequest)) {
    invalid('preflight_batch_request_echo');
  }
  for (const [index, value] of root.documents.entries()) {
    const document = parsePreflightValue(value, expectedRequest.requests[index]!);
    if (document.generated_at !== root.generated_at) {
      invalid('preflight_batch_generated_at');
    }
  }
  validatePreflightReason(
    root.limitations[0],
    'preflight_batch_single_responder_not_atomic',
  );
  validatePreflightReason(
    root.limitations[1],
    'preflight_batch_provider_execution_not_observed',
  );
  return raw as RequestPreflightBatchResponse;
}

function validatePreflightCost(
  value: unknown,
  resolution: Record<string, unknown>,
  limits: Record<string, unknown>,
  request: RequestPreflightRequest,
): void {
  const cost = record(value, 'preflight_cost');
  const numericFields = [
    'standard_input_rate_usd_per_million',
    'standard_output_rate_usd_per_million',
    'standard_cost_usd_low',
    'standard_cost_usd_high',
  ] as const;
  const tokenFields = [
    'input_tokens_low',
    'input_tokens_high',
    'output_tokens_low',
    'output_tokens_high',
  ] as const;
  if ([...numericFields, ...tokenFields].some((field) => !(field in cost))) {
    invalid('preflight_cost');
  }
  if (cost.state === 'unknown') {
    if (
      cost.source !== 'not_negotiated' ||
      [...numericFields, ...tokenFields].some((field) => cost[field] !== null)
    ) {
      invalid('preflight_cost');
    }
    validatePreflightReason(cost.reason, 'preflight_standard_cost_unavailable');
    return;
  }
  if (
    cost.state !== 'catalog_projection' ||
    cost.source !== 'registered_provider_pricing_catalog' ||
    resolution.state !== 'exact_catalog_match'
  ) {
    invalid('preflight_cost');
  }
  const inputRate = cost.standard_input_rate_usd_per_million;
  const outputRate = cost.standard_output_rate_usd_per_million;
  const costLow = cost.standard_cost_usd_low;
  const costHigh = cost.standard_cost_usd_high;
  const inputLow = cost.input_tokens_low;
  const inputHigh = cost.input_tokens_high;
  const outputLow = cost.output_tokens_low;
  const outputHigh = cost.output_tokens_high;
  if (
    !nonnegativeFinite(inputRate) ||
    !nonnegativeFinite(outputRate) ||
    !nonnegativeFinite(costLow) ||
    !nonnegativeFinite(costHigh) ||
    !nonnegativeSafeInteger(inputLow) ||
    !nonnegativeSafeInteger(inputHigh) ||
    !nonnegativeSafeInteger(outputLow) ||
    !nonnegativeSafeInteger(outputHigh) ||
    [inputLow, inputHigh, outputLow, outputHigh].some((tokens) => tokens > PREFLIGHT_TOKEN_MAX)
  ) {
    invalid('preflight_cost');
  }
  if (
    !positiveSafeInteger(limits.catalog_max_input_tokens) ||
    !nonnegativeSafeInteger(limits.catalog_max_output_tokens)
  ) {
    invalid('preflight_cost');
  }
  const expectedInput = request.declared_input_tokens === null
    ? [0, limits.catalog_max_input_tokens]
    : [request.declared_input_tokens, request.declared_input_tokens];
  const expectedOutputHigh =
    request.requested_max_output_tokens ?? limits.catalog_max_output_tokens;
  if (
    inputLow !== expectedInput[0] ||
    inputHigh !== expectedInput[1] ||
    outputLow !== 0 ||
    outputHigh !== expectedOutputHigh ||
    costHigh < costLow
  ) {
    invalid('preflight_cost');
  }
  const expectedLow = projectedStandardCost(
    inputLow,
    outputLow,
    inputRate,
    outputRate,
  );
  const expectedHigh = projectedStandardCost(
    inputHigh,
    outputHigh,
    inputRate,
    outputRate,
  );
  if (
    !approximatelyEqual(costLow, expectedLow) ||
    !approximatelyEqual(costHigh, expectedHigh)
  ) {
    invalid('preflight_cost');
  }
  validatePreflightReason(cost.reason, 'preflight_standard_cost_catalog_projection');
}

function projectedStandardCost(
  inputTokens: number,
  outputTokens: number,
  inputRate: number,
  outputRate: number,
): number {
  return (inputTokens * inputRate + outputTokens * outputRate) / 1_000_000;
}

function approximatelyEqual(left: number, right: number): boolean {
  const scale = Math.max(Math.abs(left), Math.abs(right), 1);
  return Math.abs(left - right) <= Number.EPSILON * 16 * scale;
}

function validatePreflightResolution(
  value: unknown,
  request: RequestPreflightRequest,
): Record<string, unknown> {
  const resolution = record(value, 'preflight_resolution');
  switch (resolution.state) {
    case 'exact_catalog_match':
      if (request.provider !== null && request.provider !== undefined) {
        if (
          resolution.provider !== request.provider ||
          resolution.source !== 'gateway_runtime'
        ) {
          invalid('preflight_resolution');
        }
        validatePreflightReason(resolution.reason, 'preflight_exact_provider_model_match');
      } else {
        if (!providerId(resolution.provider) || resolution.source !== 'registered_provider_catalog') {
          invalid('preflight_resolution');
        }
        validatePreflightReason(resolution.reason, 'preflight_exact_model_match');
      }
      break;
    case 'provider_registered_catalog_miss':
      if (
        request.provider === null ||
        request.provider === undefined ||
        resolution.provider !== request.provider ||
        resolution.source !== 'gateway_runtime'
      ) {
        invalid('preflight_resolution');
      }
      validatePreflightReason(resolution.reason, 'preflight_provider_registered_model_unlisted');
      break;
    case 'provider_unregistered':
      if (
        request.provider === null ||
        request.provider === undefined ||
        resolution.provider !== null ||
        resolution.source !== 'gateway_runtime'
      ) {
        invalid('preflight_resolution');
      }
      validatePreflightReason(resolution.reason, 'preflight_provider_unregistered');
      break;
    case 'dispatch_resolved_catalog_unknown':
      if (
        request.provider !== null &&
        request.provider !== undefined ||
        !providerId(resolution.provider) ||
        resolution.source !== 'gateway_dispatch_resolution'
      ) {
        invalid('preflight_resolution');
      }
      validatePreflightReason(resolution.reason, 'preflight_dispatch_provider_inferred');
      break;
    case 'unresolved':
      if (resolution.provider !== null || resolution.source !== 'gateway_runtime') {
        invalid('preflight_resolution');
      }
      validatePreflightReason(resolution.reason, 'preflight_provider_unresolved');
      break;
    default:
      invalid('preflight_resolution');
  }
  return resolution;
}

function validatePreflightCredential(
  value: unknown,
  resolution: Record<string, unknown>,
): Record<string, unknown> {
  const credential = record(value, 'preflight_credential');
  if (resolution.provider === null) {
    if (credential.state !== 'unknown' || credential.source !== 'not_inspected') {
      invalid('preflight_credential');
    }
    validatePreflightReason(credential.reason, 'preflight_credential_provider_unresolved');
    return credential;
  }
  const expected = {
    configured: ['organization_credential_store', 'preflight_credential_record_configured'],
    missing: ['organization_credential_store', 'preflight_credential_record_missing'],
    unavailable: ['organization_credential_store', 'preflight_credential_store_unavailable'],
    unknown: ['not_inspected', 'preflight_credential_store_not_configured'],
  } as const;
  const match = expected[credential.state as keyof typeof expected];
  if (match === undefined || credential.source !== match[0]) invalid('preflight_credential');
  validatePreflightReason(credential.reason, match[1]);
  return credential;
}

function validatePreflightSupport(
  value: unknown,
  resolution: Record<string, unknown>,
  request: RequestPreflightRequest,
): Record<string, unknown> {
  const support = record(value, 'preflight_support');
  if (!Array.isArray(support.missing_capabilities)) invalid('preflight_support');
  const missing = support.missing_capabilities;
  if (resolution.state !== 'exact_catalog_match') {
    if (
      support.state !== 'unknown' ||
      support.source !== 'not_negotiated' ||
      missing.length !== 0
    ) {
      invalid('preflight_support');
    }
    validatePreflightReason(support.reason, 'preflight_model_support_catalog_unknown');
    return support;
  }
  if (
    support.source !== 'registered_provider_catalog' ||
    missing.length > 8 ||
    new Set(missing).size !== missing.length ||
    missing.some(
      (capability) =>
        typeof capability !== 'string' ||
        !request.required_capabilities.includes(
          capability as RequestPreflightRequest['required_capabilities'][number],
        ),
    )
  ) {
    invalid('preflight_support');
  }
  if (support.state === 'supported_by_catalog' && missing.length === 0) {
    validatePreflightReason(support.reason, 'preflight_required_capabilities_catalog_match');
  } else if (support.state === 'unsupported_by_catalog' && missing.length > 0) {
    validatePreflightReason(support.reason, 'preflight_required_capabilities_catalog_miss');
  } else {
    invalid('preflight_support');
  }
  return support;
}

function validatePreflightLimits(
  value: unknown,
  resolution: Record<string, unknown>,
  request: RequestPreflightRequest,
): Record<string, unknown> {
  const limits = record(value, 'preflight_limits');
  if (!('catalog_max_input_tokens' in limits) || !('catalog_max_output_tokens' in limits)) {
    invalid('preflight_limits');
  }
  if (resolution.state !== 'exact_catalog_match' || limits.state === 'unknown') {
    const code =
      resolution.state === 'exact_catalog_match'
        ? 'preflight_catalog_limits_outside_v1_wire'
        : 'preflight_catalog_limits_unknown';
    if (
      limits.state !== 'unknown' ||
      limits.source !== 'not_negotiated' ||
      limits.catalog_max_input_tokens !== null ||
      limits.catalog_max_output_tokens !== null
    ) {
      invalid('preflight_limits');
    }
    validatePreflightReason(limits.reason, code);
    return limits;
  }
  if (
    !positiveSafeInteger(limits.catalog_max_input_tokens) ||
    limits.catalog_max_input_tokens > PREFLIGHT_TOKEN_MAX ||
    !nonnegativeSafeInteger(limits.catalog_max_output_tokens) ||
    limits.catalog_max_output_tokens > PREFLIGHT_TOKEN_MAX
  ) {
    invalid('preflight_limits');
  }
  const noValues =
    request.declared_input_tokens === null &&
    request.requested_max_output_tokens === null;
  if (noValues) {
    if (limits.state !== 'not_evaluated' || limits.source !== 'caller_not_supplied') {
      invalid('preflight_limits');
    }
    validatePreflightReason(limits.reason, 'preflight_declared_tokens_not_supplied');
    return limits;
  }
  const exceeds =
    (request.declared_input_tokens ?? 0) > limits.catalog_max_input_tokens ||
    (request.requested_max_output_tokens ?? 0) > limits.catalog_max_output_tokens;
  const expected = exceeds
    ? ['exceeds_catalog_metadata', 'preflight_declared_tokens_exceed_catalog']
    : ['within_catalog_metadata', 'preflight_declared_tokens_within_catalog'];
  if (limits.state !== expected[0] || limits.source !== 'registered_provider_catalog') {
    invalid('preflight_limits');
  }
  validatePreflightReason(limits.reason, expected[1]!);
  return limits;
}

function validatePreflightActions(
  value: unknown,
  resolution: Record<string, unknown>,
  credential: Record<string, unknown>,
  support: Record<string, unknown>,
  limits: Record<string, unknown>,
): void {
  if (!Array.isArray(value)) invalid('preflight_actions');
  const expected: Array<[string, boolean, string]> = [];
  if (resolution.provider === null) {
    expected.push([
      'choose_registered_provider_or_model',
      true,
      'preflight_action_provider_required',
    ]);
  }
  if (credential.state === 'missing') {
    expected.push([
      'configure_provider_credential',
      true,
      'preflight_action_configure_credential',
    ]);
  } else if (credential.state === 'unavailable') {
    expected.push([
      'retry_preflight_or_contact_operator',
      true,
      'preflight_action_retry_credential_check',
    ]);
  }
  if (support.state === 'unsupported_by_catalog') {
    expected.push([
      'change_model_or_required_capabilities',
      true,
      'preflight_action_change_capability_request',
    ]);
  }
  if (limits.state === 'exceeds_catalog_metadata') {
    expected.push([
      'reduce_declared_tokens_or_choose_model',
      true,
      'preflight_action_reduce_declared_tokens',
    ]);
  }
  expected.push([
    'execute_request_and_handle_result',
    false,
    'preflight_action_real_request_authoritative',
  ]);
  if (value.length !== expected.length) invalid('preflight_actions');
  value.forEach((rawAction, index) => {
    const action = record(rawAction, 'preflight_actions');
    const item = expected[index]!;
    if (action.code !== item[0] || action.required_before_request !== item[1]) {
      invalid('preflight_actions');
    }
    validatePreflightReason(action.reason, item[2]);
  });
}

function validatePreflightReason(value: unknown, code: string): void {
  validateReason(record(value, 'reason') as unknown as CapabilityReason, code);
}

function providerId(value: unknown): value is string {
  return typeof value === 'string' && boundedText(value, 64) && /^[a-z0-9_-]+$/.test(value);
}

function validateEvidenceReason(
  evidence: Record<string, unknown>,
  source: string,
  code: string,
): true {
  if (evidence.source !== source) invalid('fusion_enabled');
  validateReason(record(evidence.reason, 'reason') as unknown as CapabilityReason, code);
  return true;
}

function validateCurrentTier(evidence: TierEvidence): number {
  if (evidence.state !== 'known') invalid('current_tier');
  const rank = tierRank(evidence.value);
  const code =
    evidence.source === 'authenticated_api_key'
      ? 'effective_tier_from_authenticated_key'
      : evidence.source === 'gateway_free_default' && evidence.value === 'free'
        ? 'effective_tier_defaulted_to_free'
        : invalid('current_tier');
  validateReason(evidence.reason, code);
  return rank;
}

function validateMinimumTier(evidence: TierEvidence): number {
  if (evidence.state !== 'known' || evidence.source !== 'gateway_runtime') {
    invalid('minimum_tier');
  }
  const rank = tierRank(evidence.value);
  validateReason(evidence.reason, 'fusion_minimum_tier_configured');
  return rank;
}

function tierRank(value: string): number {
  const rank = ['free', 'pro', 'team', 'scale'].indexOf(value);
  if (rank < 0) invalid('tier');
  return rank;
}

function validateUnknown(value: unknown, code: string): void {
  const evidence = record(value, 'unknown_evidence') as unknown as UnknownEvidence;
  if (evidence.state !== 'unknown' || evidence.source !== 'not_negotiated') {
    invalid('unknown_evidence');
  }
  validateReason(evidence.reason, code);
}

function validateSchemaVersion(
  value: unknown,
  state: string,
  version: number | null,
  code: string,
): void {
  const evidence = record(value, 'schema_versions');
  if (
    evidence.state !== state ||
    evidence.version !== version ||
    evidence.source !== 'gateway_runtime'
  ) {
    invalid('schema_versions');
  }
  validateReason(record(evidence.reason, 'reason') as unknown as CapabilityReason, code);
}

function validateReason(reason: CapabilityReason, code: string): void {
  if (
    reason.code !== code ||
    !boundedText(reason.code, 96) ||
    !/^[a-z0-9_:-]+$/.test(reason.code) ||
    !boundedText(reason.message, 600) ||
    /[\u0000-\u001f\u007f-\u009f]/u.test(reason.message)
  ) {
    invalid('reason');
  }
}

function canonicalTimestamp(value: unknown): value is string {
  if (typeof value !== 'string' || !boundedText(value, 64)) return false;
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/.test(value)) return false;
  const parsed = new Date(value);
  return !Number.isNaN(parsed.valueOf()) && parsed.toISOString() === value;
}

function canonicalEffectiveAt(value: unknown): value is string {
  if (
    typeof value !== 'string' ||
    !boundedText(value, 64) ||
    !/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/.test(value)
  ) {
    return false;
  }
  const parsed = new Date(value);
  return (
    !Number.isNaN(parsed.valueOf()) && parsed.toISOString().replace('.000Z', 'Z') === value
  );
}

function record(value: unknown, code: string): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) invalid(code);
  return value as Record<string, unknown>;
}

function nonempty(value: unknown): value is string {
  return typeof value === 'string' && value.length > 0 && value.trim() === value;
}

function boundedText(value: string, limit: number): boolean {
  return value.length > 0 && new TextEncoder().encode(value).length <= limit && value.trim() === value;
}

function positiveSafeInteger(value: unknown): value is number {
  return Number.isSafeInteger(value) && (value as number) > 0;
}

function nonnegativeSafeInteger(value: unknown): value is number {
  return Number.isSafeInteger(value) && (value as number) >= 0;
}

function nonnegativeFinite(value: unknown): value is number {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0;
}

function invalid(code: string): never {
  throw new GatewayMetadataError(code);
}
