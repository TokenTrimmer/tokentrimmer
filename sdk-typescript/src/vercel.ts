/**
 * Vercel AI SDK cost adapter — surfaces TokenTrimmer's `x-tokentrimmer-*`
 * cost/savings into OpenTelemetry spans and per-run totals.
 *
 * When you point the Vercel AI SDK (`ai`) at the TokenTrimmer gateway — a plain
 * `baseURL` swap on `@ai-sdk/openai` — the gateway's cost/savings response
 * headers ride on the raw HTTP response, which the AI SDK surfaces on every
 * result as `result.response.headers` (documented for both `generateText` and
 * `streamText`). Those headers are invisible to the observability people already
 * watch. This adapter recovers them and:
 *
 * 1. records them as OpenTelemetry span attributes using the shared semconv keys
 *    ({@link module:semconv}), so a Langfuse / Braintrust / Tempo / Grafana trace
 *    carries the same `gen_ai.*` / `tokentrimmer.*` cost attributes the gateway
 *    stamps on its own span **and** the Python LangChain callback records; and
 * 2. accumulates a per-run cost / savings total ({@link TokenTrimmerRunCost}),
 *    optionally checking a `postResponseBudgetUsd` after each completed call.
 *    A breach throws {@link BudgetExceededError} before the caller starts its
 *    next step. This never prevents or refunds the call already observed.
 *
 * ## Why a result-post-processor and not `wrapLanguageModel` middleware
 *
 * The AI SDK's `wrapLanguageModel` middleware type churns across majors
 * (`LanguageModelV2Middleware` in v5, `LanguageModelV4Middleware` in v6/v7), so
 * coupling to it is version-fragile. `result.response.headers` is the **stable,
 * documented** surface across every AI SDK major, so this adapter reads from
 * there. It is structurally typed (it never `import`s the `ai` package — not even
 * its types), which is what makes `ai` a genuinely *optional* peer dependency:
 * this module compiles and runs with `ai` absent.
 *
 * ## Usage
 *
 * ```ts
 * import { openai } from '@ai-sdk/openai';
 * import { generateText } from 'ai';
 * import { TokenTrimmerRunCost } from '@tokentrimmer/client/vercel';
 *
 * const gateway = openai.provider({ baseURL: 'https://api.tokentrimmer.com/v1', apiKey: 'tt_live_...' });
 * const run = new TokenTrimmerRunCost({ postResponseBudgetUsd: 0.5 });
 *
 * const result = await generateText({
 *   model: gateway('claude-haiku-4-5'),
 *   prompt: 'Hello',
 *   experimental_telemetry: { isEnabled: true }, // records into the active span
 * });
 * await run.record(result); // reads x-tokentrimmer-* headers, records span attrs, accumulates
 *
 * console.log(`run cost $${run.totalCostUsd} saved $${run.totalSavedUsd}`);
 * ```
 *
 * A response with no `x-tokentrimmer-*` headers (a non-gateway model, or a
 * self-hosted gateway without pricing) degrades quietly: nothing is recorded,
 * nothing is accumulated, and nothing is thrown.
 *
 * @module
 */

import { metaFromHeaders, type TokenTrimmerMeta } from './index.js';
import {
  costInfoToAttributes,
  GEN_AI_USAGE_INPUT_TOKENS,
  GEN_AI_USAGE_OUTPUT_TOKENS,
  type Attributes,
  type AttributeValue,
} from './semconv.js';

/**
 * The minimal OpenTelemetry-span shape this adapter records onto. A real
 * `@opentelemetry/api` `Span` satisfies it structurally, so callers pass their
 * span directly — but declaring it locally (rather than importing otel's `Span`)
 * keeps the published `.d.ts` free of any `@opentelemetry/api` reference, so
 * consumers who never installed otel can still typecheck against this module.
 */
export interface RecordingSpan {
  /** `false` for a non-recording span (no active tracer) → attributes are skipped. */
  isRecording?(): boolean;
  /** Set a single attribute (the always-present OTel span method). */
  setAttribute(key: string, value: AttributeValue): unknown;
  /** Set many attributes at once (used in preference when available). */
  setAttributes?(attributes: Record<string, AttributeValue>): unknown;
}

/** The parsed cost metadata plus the semconv attributes derived from it. */
export interface TokenTrimmerCostRecord {
  /** The parsed `x-tokentrimmer-*` metadata (reused from the base SDK). */
  meta: TokenTrimmerMeta;
  /** The `gen_ai.*` / `tokentrimmer.*` span attributes (semconv-keyed). */
  attributes: Attributes;
}

/** Options for {@link recordTokenTrimmerCost}. */
export interface RecordTokenTrimmerCostOptions {
  /**
   * Target span. When omitted, the current active OpenTelemetry span is used
   * (best-effort — a no-op if `@opentelemetry/api` isn't installed or no span is
   * recording). The AI SDK records its own `ai.generateText` span when
   * `experimental_telemetry.isEnabled` is set, which becomes that active span.
   */
  span?: RecordingSpan;
  /** Record attributes onto a span. Default `true`; set `false` to only parse. */
  recordSpan?: boolean;
  /**
   * Post-response single-call budget (USD). After this call completes, throws
   * {@link BudgetExceededError} when its served `costUsd` exceeds the budget.
   * This cannot prevent the completed call's spend. For an accumulated
   * post-response budget across many calls, use {@link TokenTrimmerRunCost}.
   */
  postResponseBudgetUsd?: number;
}

/** Options for {@link TokenTrimmerRunCost}. */
export interface TokenTrimmerRunCostOptions {
  /**
   * Post-response run budget (USD). After each completed call, `record` folds in
   * its served cost and throws {@link BudgetExceededError} when the total exceeds
   * this value. The exception can stop the next step, not the call just observed.
   * `undefined` (default) disables the check.
   */
  postResponseBudgetUsd?: number;
  /** Record attributes onto a span on each `record`. Default `true`. */
  recordSpan?: boolean;
}

/**
 * Thrown when observed cost exceeds the configured post-response budget.
 *
 * Carries the offending total and the limit so a caller catching it can stop
 * before its next call. It does not make the completed call pre-admission safe.
 */
export class BudgetExceededError extends Error {
  /** The accumulated (or single-call) served cost that tripped the budget. */
  readonly totalCostUsd: number;
  /** The configured budget that was exceeded. */
  readonly limitUsd: number;

  constructor(totalCostUsd: number, limitUsd: number) {
    super(
      `TokenTrimmer observed budget exceeded: accumulated $${totalCostUsd.toFixed(6)} ` +
        `> limit $${limitUsd.toFixed(6)}`,
    );
    this.name = 'BudgetExceededError';
    this.totalCostUsd = totalCostUsd;
    this.limitUsd = limitUsd;
    // Restore the prototype chain for `instanceof` under transpiled targets.
    Object.setPrototypeOf(this, BudgetExceededError.prototype);
  }
}

/**
 * Record TokenTrimmer cost/savings from a Vercel AI SDK result onto a span.
 *
 * Reads the `x-tokentrimmer-*` headers off the result (see the module docs for
 * where the AI SDK exposes them), parses them through the base SDK's canonical
 * {@link metaFromHeaders}, maps them to OTel span attributes via the shared
 * {@link module:semconv} vocabulary, folds in `gen_ai.usage.*` token counts from
 * the result's `usage`, and records the lot on the target (or active) span.
 *
 * @param result A `generateText` / `streamText` result, a response-like object
 *   (`{ headers }`), or a raw headers container (`Headers` / `Record`).
 * @returns the parsed `{ meta, attributes }`, or `null` when the result carried
 *   no TokenTrimmer headers (a quiet no-op — never throws over missing telemetry).
 * @throws {BudgetExceededError} after the call when
 *   `options.postResponseBudgetUsd` is set and observed served cost exceeds it.
 */
export async function recordTokenTrimmerCost(
  result: unknown,
  options: RecordTokenTrimmerCostOptions = {},
): Promise<TokenTrimmerCostRecord | null> {
  const record = await buildRecord(result);
  if (record === null) return null;

  await applyToSpan(record.attributes, options.span, options.recordSpan ?? true);

  const { postResponseBudgetUsd } = options;
  if (
    postResponseBudgetUsd != null &&
    record.meta.costUsd != null &&
    record.meta.costUsd > postResponseBudgetUsd
  ) {
    throw new BudgetExceededError(record.meta.costUsd, postResponseBudgetUsd);
  }
  return record;
}

/**
 * A stateful per-run cost accumulator for the Vercel AI SDK.
 *
 * Reuse one instance across the LLM calls of a logical run (an agent loop, a
 * multi-step `generateText`, a chain of calls); each {@link record} folds the
 * call's TokenTrimmer cost/savings into the running totals and records the
 * semconv attributes on a span. With `postResponseBudgetUsd` set, the completed
 * call that tips observed accumulated cost over the budget throws
 * {@link BudgetExceededError} before the caller's next step.
 */
export class TokenTrimmerRunCost {
  /** The post-response run budget (USD), or `undefined` when disabled. */
  readonly postResponseBudgetUsd: number | undefined;
  /** Whether {@link record} writes attributes onto a span. */
  readonly recordSpan: boolean;

  /** Accumulated served cost (USD) across this tracker's calls. */
  totalCostUsd = 0;
  /** Accumulated TokenTrimmer-attributed savings (USD). */
  totalSavedUsd = 0;
  /** Accumulated baseline (un-optimised) cost (USD). */
  totalBaselineUsd = 0;
  /** Number of calls that carried TokenTrimmer cost headers. */
  attributedCalls = 0;

  constructor(options: TokenTrimmerRunCostOptions = {}) {
    this.postResponseBudgetUsd = options.postResponseBudgetUsd;
    this.recordSpan = options.recordSpan ?? true;
  }

  /** Zero the accumulated totals so this tracker can drive another run. */
  reset(): void {
    this.totalCostUsd = 0;
    this.totalSavedUsd = 0;
    this.totalBaselineUsd = 0;
    this.attributedCalls = 0;
  }

  /**
   * Record one AI SDK result: parse its TokenTrimmer headers, record the semconv
   * attributes on a span, and accumulate the totals — enforcing the run budget.
   *
   * @returns the parsed `{ meta, attributes }`, or `null` when the result had no
   *   TokenTrimmer headers (a no-op that does not advance the totals).
   * @throws {BudgetExceededError} after a call when `postResponseBudgetUsd` is
   *   set and observed accumulated cost exceeds it.
   */
  async record(
    result: unknown,
    opts: { span?: RecordingSpan } = {},
  ): Promise<TokenTrimmerCostRecord | null> {
    const record = await buildRecord(result);
    if (record === null) return null;

    await applyToSpan(record.attributes, opts.span, this.recordSpan);

    const { meta } = record;
    if (meta.costUsd != null) this.totalCostUsd += meta.costUsd;
    if (meta.savedUsd != null) this.totalSavedUsd += meta.savedUsd;
    if (meta.baselineCostUsd != null) this.totalBaselineUsd += meta.baselineCostUsd;
    this.attributedCalls += 1;

    if (
      this.postResponseBudgetUsd != null &&
      this.totalCostUsd > this.postResponseBudgetUsd
    ) {
      throw new BudgetExceededError(this.totalCostUsd, this.postResponseBudgetUsd);
    }
    return record;
  }
}

// --- internals --------------------------------------------------------------

/** A headers container the AI SDK might expose (raw record or a `Headers`). */
type HeadersContainer = Headers | Record<string, string | string[] | undefined>;

/**
 * Parse a result into `{ meta, attributes }`, or `null` when it carried no
 * TokenTrimmer headers. Shared by the free function and the run tracker.
 */
async function buildRecord(result: unknown): Promise<TokenTrimmerCostRecord | null> {
  const container = await resolveHeaders(result);
  if (container === null) return null;

  const meta = metaFromHeaders(container);
  if (!metaHasData(meta)) return null;

  const attributes: Attributes = {
    ...costInfoToAttributes(meta),
    ...(await usageAttributes(result)),
  };
  return { meta, attributes };
}

/**
 * Locate the response-headers container on an AI SDK result (or accept one
 * directly). Handles `generateText` (`result.response` is an object), `streamText`
 * (`result.response` is a Promise), the multi-step `result.finalStep.response`,
 * a bare response-like `{ headers }`, a `Headers`, or a plain headers record.
 * Returns `null` when nothing header-shaped is present.
 */
async function resolveHeaders(input: unknown): Promise<HeadersContainer | null> {
  if (input == null) return null;
  if (input instanceof Headers) return input;
  if (typeof input !== 'object') return null;
  const rec = input as Record<string, unknown>;

  // 1) An AI SDK result: `.response` — an object (generateText) or a Promise
  //    that resolves after the stream finishes (streamText).
  if (rec.response != null) {
    const resp = (await Promise.resolve(rec.response)) as { headers?: unknown } | null;
    if (resp != null && resp.headers != null) return resp.headers as HeadersContainer;
  }
  // 2) Multi-step generateText: the final step's response headers.
  if (rec.finalStep != null) {
    const fs = (await Promise.resolve(rec.finalStep)) as { response?: { headers?: unknown } } | null;
    if (fs?.response?.headers != null) return fs.response.headers as HeadersContainer;
  }
  // 3) A response-like object passed directly.
  if (rec.headers != null) return rec.headers as HeadersContainer;
  // 4) A plain headers record — but NOT a result-shaped object missing headers.
  if (!('response' in rec) && !('usage' in rec) && !('finalStep' in rec)) {
    return rec as Record<string, string>;
  }
  return null;
}

/** True when the gateway populated at least one mapped `x-tokentrimmer-*` field. */
function metaHasData(meta: TokenTrimmerMeta): boolean {
  return (
    meta.traceId != null ||
    meta.provider != null ||
    meta.modelUsed != null ||
    meta.costUsd != null ||
    meta.baselineCostUsd != null ||
    meta.savedUsd != null ||
    meta.cache != null ||
    meta.route != null
  );
}

/** The AI SDK usage shape — `inputTokens`/`outputTokens` (v5+) or the older aliases. */
interface UsageLike {
  inputTokens?: number;
  outputTokens?: number;
  promptTokens?: number;
  completionTokens?: number;
}

/**
 * Best-effort `gen_ai.usage.*` token counts from a result's `usage` (an object
 * on `generateText`, a Promise on `streamText`). Token counts don't ride on the
 * cost headers, so they're read from the framework result and folded in. Empty
 * object when unavailable.
 */
async function usageAttributes(result: unknown): Promise<Attributes> {
  if (result == null || typeof result !== 'object') return {};
  const raw = (result as { usage?: UsageLike | Promise<UsageLike> }).usage;
  if (raw == null) return {};
  let usage: UsageLike;
  try {
    usage = await Promise.resolve(raw);
  } catch {
    return {};
  }
  if (usage == null || typeof usage !== 'object') return {};

  const attrs: Attributes = {};
  const input = usage.inputTokens ?? usage.promptTokens;
  const output = usage.outputTokens ?? usage.completionTokens;
  if (typeof input === 'number' && Number.isFinite(input)) attrs[GEN_AI_USAGE_INPUT_TOKENS] = input;
  if (typeof output === 'number' && Number.isFinite(output)) {
    attrs[GEN_AI_USAGE_OUTPUT_TOKENS] = output;
  }
  return attrs;
}

/**
 * Set `attributes` on the target span (or the active OTel span). No-op when
 * recording is disabled, there are no attributes, no span is available, or the
 * span isn't recording.
 */
async function applyToSpan(
  attributes: Attributes,
  span: RecordingSpan | undefined,
  recordSpan: boolean,
): Promise<void> {
  if (!recordSpan) return;
  const keys = Object.keys(attributes);
  if (keys.length === 0) return;

  const target = span ?? (await activeSpan());
  if (target == null) return;
  if (typeof target.isRecording === 'function' && !target.isRecording()) return;

  if (typeof target.setAttributes === 'function') {
    target.setAttributes(attributes);
  } else {
    for (const key of keys) target.setAttribute(key, attributes[key]!);
  }
}

/**
 * The current active OpenTelemetry span, or `undefined`. `@opentelemetry/api` is
 * an OPTIONAL peer dependency: when it isn't installed the dynamic import throws
 * and this returns `undefined`, so recording degrades to a no-op (mirrors the
 * Python callback's best-effort span lookup).
 */
async function activeSpan(): Promise<RecordingSpan | undefined> {
  try {
    const otel = await import('@opentelemetry/api');
    return (otel.trace.getActiveSpan() as unknown as RecordingSpan | undefined) ?? undefined;
  } catch {
    return undefined;
  }
}
