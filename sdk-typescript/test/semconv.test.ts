import { describe, expect, it } from 'vitest';
import type { TokenTrimmerMeta } from '../src/index.js';
import * as semconv from '../src/semconv.js';

// A TokenTrimmerMeta with every field populated, mirroring the Python
// tests/test_semconv.py fixture so the two SDKs assert identical behaviour.
function meta(overrides: Partial<TokenTrimmerMeta> = {}): TokenTrimmerMeta {
  return {
    traceId: 'trace-1',
    provider: 'anthropic',
    modelUsed: 'claude-haiku-4-5',
    costUsd: 0.0034,
    baselineCostUsd: 0.02,
    savedUsd: 0.0166,
    cache: 'miss',
    route: 'cheap-route',
    ...overrides,
  };
}

describe('semconv constants', () => {
  it('match the gateway + Python vocabulary byte-for-byte', () => {
    // These MUST stay identical to crates/telemetry/src/gen_ai.rs AND the Python
    // sdk-python/tokentrimmer/semconv.py so a client-recorded span (TS or Python)
    // and the gateway span carry the same attribute keys.
    expect(semconv.GEN_AI_SYSTEM).toBe('gen_ai.system');
    expect(semconv.GEN_AI_PROVIDER_NAME).toBe('gen_ai.provider.name');
    expect(semconv.GEN_AI_OPERATION_NAME).toBe('gen_ai.operation.name');
    expect(semconv.GEN_AI_REQUEST_MODEL).toBe('gen_ai.request.model');
    expect(semconv.GEN_AI_RESPONSE_MODEL).toBe('gen_ai.response.model');
    expect(semconv.GEN_AI_USAGE_INPUT_TOKENS).toBe('gen_ai.usage.input_tokens');
    expect(semconv.GEN_AI_USAGE_OUTPUT_TOKENS).toBe('gen_ai.usage.output_tokens');
    expect(semconv.TT_COST_USD).toBe('tokentrimmer.cost_usd');
    expect(semconv.TT_BASELINE_COST_USD).toBe('tokentrimmer.baseline_cost_usd');
    expect(semconv.TT_SAVED_USD).toBe('tokentrimmer.saved_usd');
    expect(semconv.TT_CACHE).toBe('tokentrimmer.cache');
    expect(semconv.TT_ROUTE).toBe('tokentrimmer.route');
    expect(semconv.TT_TRACE_ID).toBe('tokentrimmer.trace_id');
  });
});

describe('genAiSystem', () => {
  it('maps known providers and passes unregistered ids through', () => {
    expect(semconv.genAiSystem('openai')).toBe('openai');
    expect(semconv.genAiSystem('anthropic')).toBe('anthropic');
    expect(semconv.genAiSystem('gemini')).toBe('gcp.gemini');
    expect(semconv.genAiSystem('mistral')).toBe('mistral_ai');
    expect(semconv.genAiSystem('groq')).toBe('groq');
    // Aggregators / cache pseudo-provider pass through verbatim.
    expect(semconv.genAiSystem('openrouter')).toBe('openrouter');
    expect(semconv.genAiSystem('cache')).toBe('cache');
  });
});

describe('costInfoToAttributes', () => {
  it('maps a fully-populated meta to the semconv attributes', () => {
    const attrs = semconv.costInfoToAttributes(meta());
    expect(attrs[semconv.GEN_AI_SYSTEM]).toBe('anthropic');
    expect(attrs[semconv.GEN_AI_PROVIDER_NAME]).toBe('anthropic');
    expect(attrs[semconv.GEN_AI_RESPONSE_MODEL]).toBe('claude-haiku-4-5');
    expect(attrs[semconv.TT_COST_USD]).toBe(0.0034);
    expect(attrs[semconv.TT_BASELINE_COST_USD]).toBe(0.02);
    expect(attrs[semconv.TT_SAVED_USD]).toBe(0.0166);
    expect(attrs[semconv.TT_CACHE]).toBe('miss');
    expect(attrs[semconv.TT_ROUTE]).toBe('cheap-route');
    expect(attrs[semconv.TT_TRACE_ID]).toBe('trace-1');
  });

  it('maps the provider to its well-known semconv value', () => {
    const attrs = semconv.costInfoToAttributes(meta({ provider: 'gemini' }));
    expect(attrs[semconv.GEN_AI_SYSTEM]).toBe('gcp.gemini');
    expect(attrs[semconv.GEN_AI_PROVIDER_NAME]).toBe('gcp.gemini');
  });

  it('is additive — omits null fields (no cost headers → empty object)', () => {
    const empty = semconv.costInfoToAttributes({
      traceId: null,
      provider: null,
      modelUsed: null,
      costUsd: null,
      baselineCostUsd: null,
      savedUsd: null,
      cache: null,
      route: null,
    });
    expect(empty).toEqual({});

    // Partial population: only present fields appear; nothing defaults to 0.
    const partial = semconv.costInfoToAttributes(
      meta({ cache: null, route: null, baselineCostUsd: null }),
    );
    expect(semconv.TT_CACHE in partial).toBe(false);
    expect(semconv.TT_ROUTE in partial).toBe(false);
    expect(semconv.TT_BASELINE_COST_USD in partial).toBe(false);
    expect(partial[semconv.TT_COST_USD]).toBe(0.0034);
  });
});
