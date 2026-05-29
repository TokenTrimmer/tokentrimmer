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
 *   model: 'claude-sonnet-4-6',
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
  /** `'hit-l1' | 'hit-l2' | 'miss' | 'none' | 'sandbox' | 'bypass'` */
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

export class TokenTrimmer extends OpenAI {
  constructor(options: ClientOptions = {}) {
    super({
      ...options,
      baseURL: options.baseURL ?? DEFAULT_BASE_URL,
    });

    // Wrap chat.completions.create to lift `ttTag` into the request header
    // and attach `.tt` metadata to the parsed response.
    const originalCreate = this.chat.completions.create.bind(this.chat.completions);
    // The OpenAI SDK uses overload-heavy signatures; we type-erase locally.
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    this.chat.completions.create = (async (body: any, opts: any = {}) => {
      const { ttTag, ...rest } = body ?? {};
      // Sensible default to prevent unbounded output. User-provided
      // max_tokens / max_completion_tokens / max_output_tokens win.
      if (
        rest.max_tokens === undefined &&
        rest.max_completion_tokens === undefined &&
        rest.max_output_tokens === undefined
      ) {
        rest.max_tokens = 4096;
      }
      const headers = { ...(opts?.headers ?? {}) } as Record<string, string>;
      if (typeof ttTag === 'string') {
        headers['X-TokenTrimmer-Tag'] = ttTag;
      }
      // Ask the OpenAI SDK to return raw response too, so we can read headers.
      const { data, response } = await originalCreate(rest, {
        ...opts,
        headers,
      }).withResponse();
      const meta = parseMeta(response.headers);
      // Attach .tt to the parsed body. Pydantic-equivalent strictness isn't a
      // concern in JS — plain assignment works.
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
      (data as any).tt = meta;
      return data;
      // eslint-disable-next-line @typescript-eslint/no-explicit-any
    }) as any;
  }
}

/**
 * Response shape augmented with `.tt`. Re-exported so app code can type-assert
 * against it without re-deriving from the OpenAI SDK's internal types.
 */
export type WithTokenTrimmerMeta<T> = T & { tt: TokenTrimmerMeta };
