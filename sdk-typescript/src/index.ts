/**
 * TokenTrimmer Node SDK.
 *
 * Thin wrapper around the official `openai` Node SDK that:
 *   1. Defaults `baseURL` to the hosted Gateway.
 *   2. Lifts a convenience `ttTag` field on chat-completion calls into the
 *      `X-TokenTrimmer-Tag` header for per-feature cost attribution.
 *   3. Attaches a `.tt` accessor to each response carrying parsed
 *      `X-TokenTrimmer-*` headers (cost, baseline, saved, cache, provider,
 *      model_used, trace_id).
 *
 * @example
 *
 * ```ts
 * import { TokenTrimmer } from '@tokentrimmer/client';
 *
 * const client = new TokenTrimmer({ apiKey: 'tt_live_...' });
 *
 * const response = await client.chat.completions.create({
 *   model: 'claude-haiku-4-5',
 *   messages: [{ role: 'user', content: 'Hello' }],
 *   max_tokens: 1024,
 *   ttTag: 'feature=chat-support',
 * });
 *
 * console.log(response.choices[0].message.content);
 * console.log(`cost $${response.tt.costUsd?.toFixed(4)}`);
 * console.log(`cache ${response.tt.cache}`);
 * ```
 */

import OpenAI from 'openai';
import type { ClientOptions } from 'openai';

export const DEFAULT_BASE_URL = 'https://api.tokentrimmer.com/v1';

/**
 * Parsed `X-TokenTrimmer-*` response headers. Every field is `null` when the
 * Gateway didn't populate the corresponding header.
 */
export interface TokenTrimmerMeta {
  traceId: string | null;
  provider: string | null;
  modelUsed: string | null;
  costUsd: number | null;
  baselineCostUsd: number | null;
  savedUsd: number | null;
  /** `'hit-l1' | 'hit-l2' | 'neg-hit' | 'miss' | 'none' | 'sandbox'` */
  cache: string | null;
}

function parseFloatOrNull(s: string | null | undefined): number | null {
  if (s === null || s === undefined) return null;
  const n = Number(s);
  return Number.isFinite(n) ? n : null;
}

function parseMeta(headers: Headers): TokenTrimmerMeta {
  return {
    traceId: headers.get('x-tokentrimmer-trace-id'),
    provider: headers.get('x-tokentrimmer-provider'),
    modelUsed: headers.get('x-tokentrimmer-model-used'),
    costUsd: parseFloatOrNull(headers.get('x-tokentrimmer-cost-usd')),
    baselineCostUsd: parseFloatOrNull(headers.get('x-tokentrimmer-baseline-cost-usd')),
    savedUsd: parseFloatOrNull(headers.get('x-tokentrimmer-saved-usd')),
    cache: headers.get('x-tokentrimmer-cache'),
  };
}

// Valid X-TokenTrimmer-Cache REQUEST-override values (API reference §6.1).
// Distinct from the response cache-status values (hit-l1/hit-l2/neg-hit/...).
const VALID_CACHE_OVERRIDES = new Set(['bypass', 'force-write', 'read-only', 'disabled']);

export class TokenTrimmer extends OpenAI {
  constructor(options: ClientOptions = {}) {
    super({
      ...options,
      baseURL: options.baseURL ?? DEFAULT_BASE_URL,
    });

    const originalCreate = this.chat.completions.create.bind(this.chat.completions);

    // The OpenAI SDK's `create` is a heavily-overloaded method; assigning a
    // replacement requires a localized cast at this boundary. Inside the
    // wrapper we keep types explicit and only treat the request body as a
    // loose record so we can read/move the `tt*` convenience fields.
    const wrapped = async (body: Record<string, unknown>, opts: Record<string, unknown> = {}) => {
      const { ttTag, ttCostLimit, ttCache, ...rest } = body ?? {};

      // Sensible default to prevent unbounded output. User-provided
      // max_tokens / max_completion_tokens / max_output_tokens win.
      if (
        rest.max_tokens === undefined &&
        rest.max_completion_tokens === undefined &&
        rest.max_output_tokens === undefined
      ) {
        rest.max_tokens = 4096;
      }

      const headers = { ...((opts.headers as Record<string, string>) ?? {}) };
      if (typeof ttTag === 'string') headers['X-TokenTrimmer-Tag'] = ttTag;
      if (ttCostLimit !== undefined && ttCostLimit !== null) {
        const limit = Number(ttCostLimit);
        if (!Number.isFinite(limit) || limit < 0) {
          throw new Error(
            `ttCostLimit must be a non-negative finite number; got ${String(ttCostLimit)}`,
          );
        }
        headers['X-TokenTrimmer-Cost-Limit-Usd'] = String(limit);
      }
      if (ttCache !== undefined && ttCache !== null) {
        if (typeof ttCache !== 'string' || !VALID_CACHE_OVERRIDES.has(ttCache)) {
          throw new Error(
            `ttCache must be one of ${[...VALID_CACHE_OVERRIDES].join(', ')}; got ${String(ttCache)}`,
          );
        }
        headers['X-TokenTrimmer-Cache'] = ttCache;
      }
      const callOpts = { ...opts, headers };

      // Streaming: the cost headers describe the whole response, which isn't
      // complete until the stream is drained. Return the SDK Stream untouched;
      // do not call withResponse() or attach .tt.
      if (rest.stream === true) {
        // Localized cast to avoid fighting overload resolution with loose Record args.
        return (originalCreate as (b: unknown, o: unknown) => Promise<unknown>)(rest, callOpts);
      }

      const { data, response } = await (originalCreate as (b: unknown, o: unknown) => { withResponse(): Promise<{ data: unknown; response: Response }> })(rest, callOpts).withResponse();
      (data as { tt?: TokenTrimmerMeta }).tt = parseMeta(response.headers);
      return data;
    };

    // Localized cast: see comment above. This is the only `any` in the wrap.
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    this.chat.completions.create = wrapped as any;
  }
}

/**
 * Response shape augmented with `.tt`. Re-exported so app code can type-assert
 * against it without re-deriving from the OpenAI SDK's internal types.
 */
export type WithTokenTrimmerMeta<T> = T & { tt: TokenTrimmerMeta };
