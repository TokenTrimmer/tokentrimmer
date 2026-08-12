import { describe, expect, it } from 'vitest';
// Importing the base client here (alongside the Vercel adapter) proves the whole
// package graph loads WITHOUT the optional `ai` package installed — this test
// suite runs in an env where `ai` is absent (it is an optional peer dep).
import { TokenTrimmer } from '../src/index.js';
import * as semconv from '../src/semconv.js';
import type { AttributeValue } from '../src/semconv.js';
import {
  BudgetExceededError,
  recordTokenTrimmerCost,
  TokenTrimmerRunCost,
} from '../src/vercel.js';
import type { RecordingSpan } from '../src/vercel.js';

// The gateway's cost/savings response headers, as the Vercel AI SDK surfaces
// them on `result.response.headers` (a plain record of raw HTTP headers).
const TT_HEADERS: Record<string, string> = {
  'x-tokentrimmer-trace-id': 'trace-1',
  'x-tokentrimmer-provider': 'anthropic',
  'x-tokentrimmer-model-used': 'claude-haiku-4-5',
  'x-tokentrimmer-cost-usd': '0.0034',
  'x-tokentrimmer-baseline-cost-usd': '0.02',
  'x-tokentrimmer-saved-usd': '0.0166',
  'x-tokentrimmer-cache': 'miss',
  'x-tokentrimmer-route-matched': 'cheap-route',
};

/** A `generateText`-shaped result (response + usage are plain objects). */
function generateTextResult(headers: Record<string, string> = TT_HEADERS) {
  return {
    text: 'hi',
    usage: { inputTokens: 10, outputTokens: 20, totalTokens: 30 },
    response: { id: 'r1', modelId: 'claude-haiku-4-5', headers },
  };
}

/** A `streamText`-shaped result (response + usage are Promises). */
function streamTextResult(headers: Record<string, string> = TT_HEADERS) {
  return {
    usage: Promise.resolve({ inputTokens: 5, outputTokens: 7 }),
    response: Promise.resolve({ id: 'r2', headers }),
  };
}

/** A capturing span with the full OTel `setAttributes` fast-path. */
class FakeSpan implements RecordingSpan {
  attributes: Record<string, AttributeValue> = {};
  recording = true;
  isRecording(): boolean {
    return this.recording;
  }
  setAttribute(key: string, value: AttributeValue): this {
    this.attributes[key] = value;
    return this;
  }
  setAttributes(attributes: Record<string, AttributeValue>): this {
    Object.assign(this.attributes, attributes);
    return this;
  }
}

/** A span exposing only `setAttribute` (exercises the per-key fallback path). */
class SetAttributeOnlySpan implements RecordingSpan {
  attributes: Record<string, AttributeValue> = {};
  setAttribute(key: string, value: AttributeValue): this {
    this.attributes[key] = value;
    return this;
  }
}

describe('recordTokenTrimmerCost', () => {
  it('records semconv attributes from a generateText result onto the span', async () => {
    const span = new FakeSpan();
    const rec = await recordTokenTrimmerCost(generateTextResult(), { span });
    expect(rec).not.toBeNull();

    // Parsed meta (reused from the base SDK's header extraction).
    expect(rec!.meta.savedUsd).toBe(0.0166);
    expect(rec!.meta.route).toBe('cheap-route');

    // Cost/savings semconv attributes recorded on the span.
    expect(span.attributes[semconv.GEN_AI_SYSTEM]).toBe('anthropic');
    expect(span.attributes[semconv.GEN_AI_PROVIDER_NAME]).toBe('anthropic');
    expect(span.attributes[semconv.GEN_AI_RESPONSE_MODEL]).toBe('claude-haiku-4-5');
    expect(span.attributes[semconv.TT_COST_USD]).toBe(0.0034);
    expect(span.attributes[semconv.TT_SAVED_USD]).toBe(0.0166);
    expect(span.attributes[semconv.TT_CACHE]).toBe('miss');
    expect(span.attributes[semconv.TT_ROUTE]).toBe('cheap-route');
    expect(span.attributes[semconv.TT_TRACE_ID]).toBe('trace-1');
    // Token counts folded in from result.usage (not carried on headers).
    expect(span.attributes[semconv.GEN_AI_USAGE_INPUT_TOKENS]).toBe(10);
    expect(span.attributes[semconv.GEN_AI_USAGE_OUTPUT_TOKENS]).toBe(20);
  });

  it('handles the streamText shape (response + usage are Promises)', async () => {
    const span = new FakeSpan();
    const rec = await recordTokenTrimmerCost(streamTextResult(), { span });
    expect(rec!.meta.costUsd).toBe(0.0034);
    expect(span.attributes[semconv.TT_COST_USD]).toBe(0.0034);
    expect(span.attributes[semconv.GEN_AI_USAGE_INPUT_TOKENS]).toBe(5);
    expect(span.attributes[semconv.GEN_AI_USAGE_OUTPUT_TOKENS]).toBe(7);
  });

  it('records via the per-key fallback when the span has no setAttributes', async () => {
    const span = new SetAttributeOnlySpan();
    await recordTokenTrimmerCost(generateTextResult(), { span });
    expect(span.attributes[semconv.TT_COST_USD]).toBe(0.0034);
    expect(span.attributes[semconv.GEN_AI_SYSTEM]).toBe('anthropic');
  });

  it('accepts a raw Headers instance and is header-case-insensitive', async () => {
    const span = new FakeSpan();
    const headers = new Headers({ 'X-TokenTrimmer-Cost-Usd': '0.5', 'X-TokenTrimmer-Provider': 'openai' });
    const rec = await recordTokenTrimmerCost(headers, { span });
    expect(rec!.meta.costUsd).toBe(0.5);
    expect(span.attributes[semconv.GEN_AI_SYSTEM]).toBe('openai');
  });

  it('degrades quietly on a non-gateway result (no TT headers → null, no span writes)', async () => {
    const span = new FakeSpan();
    const rec = await recordTokenTrimmerCost(
      { text: 'hi', usage: { inputTokens: 1 }, response: { headers: { 'content-type': 'application/json' } } },
      { span },
    );
    expect(rec).toBeNull();
    expect(Object.keys(span.attributes)).toHaveLength(0);
  });

  it('does not record when the span is not recording', async () => {
    const span = new FakeSpan();
    span.recording = false;
    const rec = await recordTokenTrimmerCost(generateTextResult(), { span });
    expect(rec).not.toBeNull(); // still parsed + returned
    expect(Object.keys(span.attributes)).toHaveLength(0); // but nothing written
  });

  it('recordSpan:false parses + returns without touching the span', async () => {
    const span = new FakeSpan();
    const rec = await recordTokenTrimmerCost(generateTextResult(), { span, recordSpan: false });
    expect(rec!.meta.costUsd).toBe(0.0034);
    expect(Object.keys(span.attributes)).toHaveLength(0);
  });

  it('does not throw when no span is provided and no OTel tracer is active', async () => {
    // `@opentelemetry/api` may be installed but there is no active recording span
    // here, so the best-effort active-span lookup resolves to a no-op.
    const rec = await recordTokenTrimmerCost(generateTextResult());
    expect(rec!.meta.costUsd).toBe(0.0034);
  });

  it('throws after a call exceeds postResponseBudgetUsd', async () => {
    await expect(
      recordTokenTrimmerCost(generateTextResult(), { postResponseBudgetUsd: 0.001 }),
    ).rejects.toBeInstanceOf(BudgetExceededError);
  });

  it('does not throw when observed call cost is within postResponseBudgetUsd', async () => {
    const rec = await recordTokenTrimmerCost(generateTextResult(), {
      postResponseBudgetUsd: 1.0,
    });
    expect(rec!.meta.costUsd).toBe(0.0034);
  });
});

describe('TokenTrimmerRunCost', () => {
  it('accumulates cost / savings / baseline across records', async () => {
    const run = new TokenTrimmerRunCost();
    const span = new FakeSpan();
    await run.record(generateTextResult(), { span });
    await run.record(generateTextResult(), { span });

    expect(run.attributedCalls).toBe(2);
    expect(run.totalCostUsd).toBeCloseTo(0.0068, 10);
    expect(run.totalSavedUsd).toBeCloseTo(0.0332, 10);
    expect(run.totalBaselineUsd).toBeCloseTo(0.04, 10);
  });

  it('ignores a result with no TokenTrimmer headers (no-op, totals unchanged)', async () => {
    const run = new TokenTrimmerRunCost();
    const rec = await run.record({ response: { headers: { 'content-type': 'text/plain' } } });
    expect(rec).toBeNull();
    expect(run.attributedCalls).toBe(0);
    expect(run.totalCostUsd).toBe(0);
  });

  it('throws after the record that tips observed run cost over budget', async () => {
    const run = new TokenTrimmerRunCost({ postResponseBudgetUsd: 0.005 });
    // First record: 0.0034 <= 0.005, OK.
    await run.record(generateTextResult());
    expect(run.totalCostUsd).toBeCloseTo(0.0034, 10);
    // Second record: 0.0068 > 0.005 → throws, carrying the accumulated total.
    let thrown: unknown;
    try {
      await run.record(generateTextResult());
    } catch (err) {
      thrown = err;
    }
    expect(thrown).toBeInstanceOf(BudgetExceededError);
    const budget = thrown as BudgetExceededError;
    expect(budget.limitUsd).toBe(0.005);
    expect(budget.totalCostUsd).toBeCloseTo(0.0068, 10);
    expect(run.totalCostUsd).toBeCloseTo(0.0068, 10);
  });

  it('reset() zeroes the totals for reuse', async () => {
    const run = new TokenTrimmerRunCost();
    await run.record(generateTextResult());
    expect(run.attributedCalls).toBe(1);
    run.reset();
    expect(run.attributedCalls).toBe(0);
    expect(run.totalCostUsd).toBe(0);
    expect(run.totalSavedUsd).toBe(0);
  });
});

describe('optional-dep independence', () => {
  it('constructs the base TokenTrimmer client with the ai package absent', () => {
    // If any part of the package graph statically imported `ai`, this module
    // would have failed to load (ai is not installed). Reaching here is proof.
    const client = new TokenTrimmer({ apiKey: 'tt_test' });
    expect(client).toBeInstanceOf(TokenTrimmer);
  });
});
