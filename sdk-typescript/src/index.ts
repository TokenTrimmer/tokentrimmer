/**
 * TokenTrimmer Node SDK.
 *
 * Thin wrapper around the official `openai` Node SDK that:
 *   1. Defaults `baseURL` to the hosted Gateway.
 *   2. Lifts convenience fields on chat-completion calls into request headers:
 *      `ttTag` → `X-TokenTrimmer-Tag` (per-feature cost attribution),
 *      `ttCostLimit` → `X-TokenTrimmer-Cost-Limit-Usd` (gateway rejects with 402
 *      if the estimated cost exceeds it), and `ttCache` → `X-TokenTrimmer-Cache`
 *      (one of `bypass` / `force-write` / `read-only` / `disabled`).
 *   3. Attaches a `.tt` accessor to each non-streaming response carrying parsed
 *      `X-TokenTrimmer-*` headers (cost, baseline, saved, cache, provider,
 *      modelUsed, traceId).
 *   4. For streaming calls, strips the Gateway's terminal `tokentrimmer.usage`
 *      SSE frame (which the OpenAI parser would otherwise turn into a malformed
 *      chunk) and surfaces its cost on the returned stream's `.tt` once drained.
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
 *
 * // Streaming: read per-request cost off `stream.tt` after draining.
 * const stream = await client.chat.completions.create({
 *   model: 'claude-haiku-4-5',
 *   messages: [{ role: 'user', content: 'Hello' }],
 *   stream: true,
 * });
 * for await (const chunk of stream) process.stdout.write(chunk.choices[0]?.delta?.content ?? '');
 * console.log(`\ncost $${stream.tt?.costUsd?.toFixed(4)}`);
 * ```
 */

import OpenAI from 'openai';
import type { ClientOptions } from 'openai';
import type { APIPromise } from 'openai/core/api-promise';
import { Stream } from 'openai/core/streaming';
import type {
  ChatCompletion,
  ChatCompletionChunk,
  ChatCompletionCreateParamsBase,
  ChatCompletionCreateParamsNonStreaming,
  ChatCompletionCreateParamsStreaming,
} from 'openai/resources/chat/completions';

export const DEFAULT_BASE_URL = 'https://api.tokentrimmer.com/v1';

/**
 * Resolve the API key from `TOKENTRIMMER_API_KEY` when the caller passed none.
 *
 * Returns `undefined` if the env var is unset OR there is no `process` (a
 * browser/edge runtime), so the `typeof process` guard means this never throws
 * outside Node. When it returns `undefined`, the base OpenAI constructor's own
 * default falls back to `OPENAI_API_KEY` — giving the precedence chain
 * `options.apiKey` > `TOKENTRIMMER_API_KEY` > `OPENAI_API_KEY`.
 */
function resolveApiKey(): string | undefined {
  if (typeof process === 'undefined' || process.env == null) return undefined;
  return process.env.TOKENTRIMMER_API_KEY;
}

// The per-request options type, derived from the inherited `create` overload so
// we don't depend on an unexported `internal/*` subpath of the openai package.
type RequestOptions = NonNullable<Parameters<OpenAI['chat']['completions']['create']>[1]>;

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

/**
 * Cost/usage from the Gateway's terminal `tokentrimmer.usage` SSE frame, the
 * streaming counterpart to {@link TokenTrimmerMeta}. Surfaced on a streaming
 * response as `stream.tt` once the stream has been drained (the cost can't ride
 * on headers because it isn't known until the whole response is generated).
 * Field shape mirrors the Gateway frame (crates/core/src/routes/sse.rs).
 */
export interface StreamCost {
  costUsd: number;
  baselineCostUsd: number;
  savedUsd: number;
  providerCacheSavedUsd: number;
  inputTokens: number;
  outputTokens: number;
  cachedTokens: number;
}

/** The raw JSON payload of the `tokentrimmer.usage` frame (snake_case wire). */
interface UsagePayload {
  cost_usd?: number;
  baseline_cost_usd?: number;
  saved_usd?: number;
  provider_cache_saved_usd?: number;
  input_tokens?: number;
  output_tokens?: number;
  cached_tokens?: number;
}

/** TokenTrimmer convenience fields accepted on `create`, lifted into headers. */
export interface TokenTrimmerExtraParams {
  /** Free-form cost-attribution tag → `X-TokenTrimmer-Tag`. */
  ttTag?: string;
  /** Reject (402) if estimated cost exceeds this → `X-TokenTrimmer-Cost-Limit-Usd`. */
  ttCostLimit?: number;
  /** Cache override → `X-TokenTrimmer-Cache` (`bypass`/`force-write`/`read-only`/`disabled`). */
  ttCache?: 'bypass' | 'force-write' | 'read-only' | 'disabled';
}

/** Non-streaming response augmented with the parsed `.tt` metadata. */
export type ChatCompletionWithMeta = ChatCompletion & { tt: TokenTrimmerMeta };

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
const VALID_CACHE_OVERRIDES = new Set<string>(['bypass', 'force-write', 'read-only', 'disabled']);

/**
 * Normalize the caller's request-options `headers` (whose type is a broad
 * `HeadersLike` union) into a `Headers` we can extend with the `X-TokenTrimmer-*`
 * entries. Preserves any caller-supplied headers; drops null/undefined values.
 */
function toHeaders(src: unknown): Headers {
  const out = new Headers();
  if (src == null) return out;
  if (src instanceof Headers) {
    src.forEach((value, key) => out.set(key, value));
    return out;
  }
  const setPair = (key: unknown, value: unknown): void => {
    if (typeof key === 'string' && value != null) out.set(key, String(value));
  };
  if (Array.isArray(src)) {
    for (const pair of src) if (Array.isArray(pair)) setPair(pair[0], pair[1]);
    return out;
  }
  if (typeof src === 'object') {
    for (const [key, value] of Object.entries(src as Record<string, unknown>)) {
      setPair(key, Array.isArray(value) ? value[0] : value);
    }
  }
  return out;
}

/**
 * A streaming chat-completion response that strips the Gateway's terminal
 * `tokentrimmer.usage` frame and surfaces its cost on {@link tt}.
 *
 * The underlying OpenAI `Stream` parses every SSE frame's `data` as JSON and
 * yields it; the Gateway's usage frame has no `choices`, so left unfiltered it
 * reaches the caller as a malformed chunk that crashes `chunk.choices[0]`. This
 * wrapper detects and removes that frame, parsing its payload into
 * {@link StreamCost}. Iterate with `for await … of`; read cost off `.tt` after
 * the stream is drained (`null` until then, or if no usage frame was emitted).
 *
 * Mirrors the OpenAI `Stream` surface beyond iteration: {@link controller},
 * {@link tee} and {@link toReadableStream} are all forwarded, and every path
 * applies the same usage-frame stripping (so consumers of `tee()` /
 * `toReadableStream()` see clean chunks too, not the raw frame).
 */
export class TokenTrimmerStream implements AsyncIterable<ChatCompletionChunk> {
  /** Streaming cost; `null` until the terminal usage frame is consumed. */
  public tt: StreamCost | null = null;

  constructor(private readonly inner: Stream<ChatCompletionChunk>) {}

  /** Cancel the underlying request (forwards to the OpenAI stream controller). */
  get controller(): AbortController {
    return this.inner.controller;
  }

  async *[Symbol.asyncIterator](): AsyncIterator<ChatCompletionChunk> {
    for await (const chunk of this.inner) {
      const usage = asUsagePayload(chunk);
      if (usage) {
        this.tt = toStreamCost(usage);
        continue; // strip: never hand the usage frame to the caller
      }
      yield chunk;
    }
  }

  /**
   * Splits the stream into two independently-readable streams, mirroring the
   * OpenAI `Stream.tee()`. Both branches draw from THIS wrapper's stripping
   * iterator, so the `tokentrimmer.usage` frame is removed on either side and
   * its cost lands on this object's {@link tt} once a branch is drained (the
   * underlying request is shared, so a single usage frame is seen once).
   */
  tee(): [Stream<ChatCompletionChunk>, Stream<ChatCompletionChunk>] {
    return this.asStream().tee();
  }

  /**
   * Converts this stream to a newline-separated `ReadableStream` of JSON
   * stringified chunks, mirroring the OpenAI `Stream.toReadableStream()`. The
   * `tokentrimmer.usage` frame is stripped (it never reaches the ReadableStream)
   * and its cost is surfaced on {@link tt} once the ReadableStream is drained.
   */
  toReadableStream(): ReturnType<Stream<ChatCompletionChunk>['toReadableStream']> {
    return this.asStream().toReadableStream();
  }

  /**
   * Wrap this stripping iterator in a fresh OpenAI `Stream` so we can reuse its
   * `tee()` / `toReadableStream()` implementations (and exact return types)
   * while keeping the usage-frame filtering intact on every consumption path.
   */
  private asStream(): Stream<ChatCompletionChunk> {
    return new Stream<ChatCompletionChunk>(() => this[Symbol.asyncIterator](), this.controller);
  }
}

/**
 * Identify the Gateway usage frame among yielded stream items. A real chunk has
 * `object === 'chat.completion.chunk'`; the usage frame doesn't and carries
 * `cost_usd`. Returns the typed payload when matched, else `null`.
 */
function asUsagePayload(chunk: unknown): UsagePayload | null {
  if (typeof chunk !== 'object' || chunk === null) return null;
  const rec = chunk as Record<string, unknown>;
  if (rec.object === 'chat.completion.chunk') return null;
  if (typeof rec.cost_usd !== 'number') return null;
  return rec as UsagePayload;
}

function num(v: number | undefined): number {
  return typeof v === 'number' ? v : 0;
}

function toStreamCost(p: UsagePayload): StreamCost {
  return {
    costUsd: num(p.cost_usd),
    baselineCostUsd: num(p.baseline_cost_usd),
    savedUsd: num(p.saved_usd),
    providerCacheSavedUsd: num(p.provider_cache_saved_usd),
    inputTokens: num(p.input_tokens),
    outputTokens: num(p.output_tokens),
    cachedTokens: num(p.cached_tokens),
  };
}

/**
 * The `create` method as exposed on the TokenTrimmer client. The TokenTrimmer
 * overloads are declared FIRST so they win resolution: non-streaming resolves to
 * {@link ChatCompletionWithMeta} (with `.tt`), streaming to a
 * {@link TokenTrimmerStream}, and both accept the `tt*` convenience params. The
 * original OpenAI overloads are re-declared after as a fallback for other shapes.
 * (Interface overload order is declaration order — unlike a `&` intersection,
 * which would let the broad base overload capture the call first.)
 */
interface TokenTrimmerCreate {
  // TokenTrimmer overloads (must come first):
  (
    body: ChatCompletionCreateParamsNonStreaming & TokenTrimmerExtraParams,
    options?: RequestOptions,
  ): Promise<ChatCompletionWithMeta>;
  (
    body: ChatCompletionCreateParamsStreaming & TokenTrimmerExtraParams,
    options?: RequestOptions,
  ): Promise<TokenTrimmerStream>;
  // Inherited OpenAI overloads (fallback):
  (body: ChatCompletionCreateParamsNonStreaming, options?: RequestOptions): APIPromise<ChatCompletion>;
  (
    body: ChatCompletionCreateParamsStreaming,
    options?: RequestOptions,
  ): APIPromise<Stream<ChatCompletionChunk>>;
  (
    body: ChatCompletionCreateParamsBase,
    options?: RequestOptions,
  ): APIPromise<Stream<ChatCompletionChunk> | ChatCompletion>;
}

/**
 * The `chat.completions` resource with TokenTrimmer's typed {@link TokenTrimmerCreate}.
 * `Omit`s the base `create` (so our overloads win resolution as interface-own
 * members) while keeping every other member of the resource.
 */
export interface TokenTrimmerCompletions extends Omit<OpenAI.Chat.Completions, 'create'> {
  create: TokenTrimmerCreate;
}

/** The `chat` resource carrying {@link TokenTrimmerCompletions}. */
export interface TokenTrimmerChat extends Omit<OpenAI.Chat, 'completions'> {
  completions: TokenTrimmerCompletions;
}

export class TokenTrimmer extends OpenAI {
  /**
   * Narrowed type for the inherited `chat` resource: `chat.completions.create`
   * exposes the `.tt`-augmented return types and `tt*` params. `declare` refines
   * the inherited property's type without emitting a field (the base
   * constructor still creates the real resource). The `& OpenAI.Chat`
   * intersection re-supplies the base's protected members (so the override stays
   * assignable to the inherited property); `TokenTrimmerChat`'s own `create`
   * overloads still win resolution.
   */
  declare chat: TokenTrimmerChat & OpenAI.Chat;

  constructor(options: ClientOptions = {}) {
    super({
      ...options,
      apiKey: options.apiKey ?? resolveApiKey(),
      baseURL: options.baseURL ?? DEFAULT_BASE_URL,
    });

    // Capture the original OpenAI `create` (an `APIPromise`-returning, overloaded
    // method) before we replace it. The `chat` field is type-narrowed to the
    // TokenTrimmer overloads, so we read the base method type back off
    // `OpenAI.Chat.Completions` here.
    const completions = this.chat.completions as unknown as OpenAI.Chat.Completions;
    const originalCreate = completions.create.bind(completions);

    const wrapped = async (
      body: ChatCompletionCreateParamsBase & TokenTrimmerExtraParams,
      options: RequestOptions = {},
    ): Promise<ChatCompletionWithMeta | TokenTrimmerStream> => {
      const { ttTag, ttCostLimit, ttCache, ...rest } = body;
      const params: ChatCompletionCreateParamsBase = rest;

      // Sensible default to prevent unbounded output. A user-provided
      // max_tokens / max_completion_tokens wins.
      if (params.max_tokens == null && params.max_completion_tokens == null) {
        params.max_tokens = 4096;
      }

      const headers = toHeaders(options.headers);
      if (ttTag !== undefined) headers.set('X-TokenTrimmer-Tag', ttTag);
      if (ttCostLimit !== undefined) {
        if (!Number.isFinite(ttCostLimit) || ttCostLimit < 0) {
          throw new Error(
            `ttCostLimit must be a non-negative finite number; got ${String(ttCostLimit)}`,
          );
        }
        headers.set('X-TokenTrimmer-Cost-Limit-Usd', String(ttCostLimit));
      }
      if (ttCache !== undefined) {
        if (!VALID_CACHE_OVERRIDES.has(ttCache)) {
          throw new Error(
            `ttCache must be one of ${[...VALID_CACHE_OVERRIDES].join(', ')}; got ${String(ttCache)}`,
          );
        }
        headers.set('X-TokenTrimmer-Cache', ttCache);
      }
      const callOpts: RequestOptions = { ...options, headers };

      // Streaming: strip the terminal `tokentrimmer.usage` frame so chunk
      // iteration stays clean, and surface its cost on the returned stream's
      // `.tt` once drained.
      if (params.stream === true) {
        const stream = await originalCreate(params as ChatCompletionCreateParamsStreaming, callOpts);
        return new TokenTrimmerStream(stream);
      }

      const { data, response } = await originalCreate(
        params as ChatCompletionCreateParamsNonStreaming,
        callOpts,
      ).withResponse();
      const withMeta = data as ChatCompletionWithMeta;
      withMeta.tt = parseMeta(response.headers);
      return withMeta;
    };

    // The OpenAI SDK's `create` is a heavily-overloaded method; replacing it
    // with a single concrete implementation requires a localized cast at this
    // assignment boundary. Callers see the typed `TokenTrimmerCompletions`
    // overloads via the narrowed `chat` field, so no `any` leaks into user code.
    this.chat.completions.create = wrapped as unknown as typeof this.chat.completions.create;
  }
}

/**
 * Response shape augmented with `.tt`. Re-exported so app code can type-assert
 * against it without re-deriving from the OpenAI SDK's internal types.
 */
export type WithTokenTrimmerMeta<T> = T & { tt: TokenTrimmerMeta };
