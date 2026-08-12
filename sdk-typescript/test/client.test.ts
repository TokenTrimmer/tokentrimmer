import type { ChatCompletionChunk } from 'openai/resources/chat/completions';
import { afterEach, describe, expect, it } from 'vitest';
import { TokenTrimmer, type StreamCost } from '../src/index.js';

const TT_HEADERS = {
  'x-tokentrimmer-trace-id': 'trace-1',
  'x-tokentrimmer-provider': 'anthropic',
  'x-tokentrimmer-model-used': 'claude-haiku-4-5',
  'x-tokentrimmer-cost-usd': '0.0034',
  'x-tokentrimmer-baseline-cost-usd': '0.02',
  'x-tokentrimmer-saved-usd': '0.0166',
  'x-tokentrimmer-cache': 'miss',
};

// A real-gateway-shaped SSE body: a normal OpenAI chunk stream, then the
// terminal `tokentrimmer.usage` event (the gateway appends this after the
// OpenAI stream — see docs/04-gateway-api-reference.md and
// crates/core/src/routes/sse.rs::usage_event), then `[DONE]`.
const GATEWAY_STREAM_SSE =
  'data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,' +
  '"model":"claude-haiku-4-5","choices":[{"index":0,"delta":{"role":"assistant"},' +
  '"finish_reason":null}]}\n\n' +
  'data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,' +
  '"model":"claude-haiku-4-5","choices":[{"index":0,"delta":{"content":"Hello"},' +
  '"finish_reason":null}]}\n\n' +
  'data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,' +
  '"model":"claude-haiku-4-5","choices":[{"index":0,"delta":{"content":" world"},' +
  '"finish_reason":null}]}\n\n' +
  'data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1,' +
  '"model":"claude-haiku-4-5","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],' +
  '"usage":{"prompt_tokens":10,"completion_tokens":20,"total_tokens":30,"cached_tokens":0}}\n\n' +
  'event: tokentrimmer.usage\n' +
  'data: {"cost_usd":0.0001,"baseline_cost_usd":0.0004,"saved_usd":0.0003,' +
  '"provider_cache_saved_usd":0.0,"input_tokens":10,"output_tokens":20,"cached_tokens":0}\n\n' +
  'data: [DONE]\n\n';

const COMPLETION_BODY = {
  id: 'chatcmpl-1',
  object: 'chat.completion',
  created: 1,
  model: 'claude-haiku-4-5',
  choices: [
    { index: 0, message: { role: 'assistant', content: 'hi' }, finish_reason: 'stop' },
  ],
  usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
};

/** A stub fetch that records the last request and returns canned data + headers. */
function stubFetch(opts: { headers?: Record<string, string>; sse?: string } = {}) {
  const calls: Array<{ url: string; init: RequestInit }> = [];
  const fetchImpl = async (url: string | URL | Request, init: RequestInit = {}) => {
    calls.push({ url: String(url), init });
    if (opts.sse !== undefined) {
      return new Response(opts.sse, {
        status: 200,
        headers: { 'content-type': 'text/event-stream' },
      });
    }
    return new Response(JSON.stringify(COMPLETION_BODY), {
      status: 200,
      headers: { 'content-type': 'application/json', ...(opts.headers ?? TT_HEADERS) },
    });
  };
  return { calls, fetchImpl: fetchImpl as unknown as typeof fetch };
}

function client(fetchImpl: typeof fetch) {
  return new TokenTrimmer({ apiKey: 'tt_test_x', baseURL: 'http://gw.test/v1', fetch: fetchImpl });
}

describe('TokenTrimmer TS SDK', () => {
  it('attaches parsed .tt metadata on a non-streaming response', async () => {
    const { fetchImpl } = stubFetch();
    const res = await client(fetchImpl).chat.completions.create({
      model: 'claude-haiku-4-5',
      messages: [{ role: 'user', content: 'hi' }],
    });
    expect(res.tt.traceId).toBe('trace-1');
    expect(res.tt.costUsd).toBe(0.0034);
    expect(res.tt.cache).toBe('miss');
  });

  it('parses a non-numeric cost header to null', async () => {
    const { fetchImpl } = stubFetch({ headers: { 'x-tokentrimmer-cost-usd': 'nope' } });
    const res = await client(fetchImpl).chat.completions.create({
      model: 'm',
      messages: [{ role: 'user', content: 'hi' }],
    });
    expect(res.tt.costUsd).toBeNull();
  });

  it('injects max_tokens=4096 when absent and respects an explicit value', async () => {
    const a = stubFetch();
    await client(a.fetchImpl).chat.completions.create({
      model: 'm',
      messages: [{ role: 'user', content: 'hi' }],
    });
    expect(JSON.parse(a.calls.at(-1)!.init.body as string).max_tokens).toBe(4096);

    const b = stubFetch();
    await client(b.fetchImpl).chat.completions.create({
      model: 'm',
      messages: [{ role: 'user', content: 'hi' }],
      max_tokens: 128,
    });
    expect(JSON.parse(b.calls.at(-1)!.init.body as string).max_tokens).toBe(128);
  });

  it('lifts ttTag / ttCostLimit / ttCache into request headers', async () => {
    const { calls, fetchImpl } = stubFetch();
    await client(fetchImpl).chat.completions.create({
      model: 'm',
      messages: [{ role: 'user', content: 'hi' }],
      ttTag: 'feature=chat',
      ttCostLimit: 0.05,
      ttCache: 'bypass',
    });
    const h = new Headers(calls.at(-1)!.init.headers as HeadersInit);
    expect(h.get('x-tokentrimmer-tag')).toBe('feature=chat');
    expect(h.get('x-tokentrimmer-cost-limit-usd')).toBe('0.05');
    expect(h.get('x-tokentrimmer-cache')).toBe('bypass');
  });

  it('throws on an invalid ttCache before sending', async () => {
    const { calls, fetchImpl } = stubFetch();
    await expect(
      client(fetchImpl).chat.completions.create({
        model: 'm',
        messages: [{ role: 'user', content: 'hi' }],
        // A response cache value, not a valid request override. Cast narrowly to
        // the field's type to exercise the runtime guard a JS caller would hit.
        ttCache: 'hit-l1' as 'bypass',
      }),
    ).rejects.toThrow();
    expect(calls.length).toBe(0);
  });

  it('throws on a non-finite or non-positive ttCostLimit before sending', async () => {
    const { calls, fetchImpl } = stubFetch();
    for (const bad of [Infinity, NaN, 0, -1]) {
      await expect(
        client(fetchImpl).chat.completions.create({
          model: 'm',
          messages: [{ role: 'user', content: 'hi' }],
          ttCostLimit: bad,
        }),
      ).rejects.toThrow();
    }
    expect(calls.length).toBe(0);
  });

  it('returns the stream (not a parsed body) for stream:true', async () => {
    const sse =
      'data: {"id":"c","object":"chat.completion.chunk","created":1,"model":"m",' +
      '"choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}\n\n' +
      'data: [DONE]\n\n';
    const { fetchImpl } = stubFetch({ sse });
    const stream = await client(fetchImpl).chat.completions.create({
      model: 'm',
      messages: [{ role: 'user', content: 'hi' }],
      stream: true,
    });
    expect(typeof stream[Symbol.asyncIterator]).toBe('function');
    const chunks: ChatCompletionChunk[] = [];
    for await (const c of stream) chunks.push(c);
    expect(chunks.length).toBeGreaterThanOrEqual(1);
    // No tokentrimmer.usage frame in this stream, so no cost is surfaced.
    expect(stream.tt).toBeNull();
  });

  it('strips the tokentrimmer.usage frame and yields only clean chunks', async () => {
    const { fetchImpl } = stubFetch({ sse: GATEWAY_STREAM_SSE });
    const stream = await client(fetchImpl).chat.completions.create({
      model: 'm',
      messages: [{ role: 'user', content: 'hi' }],
      stream: true,
    });
    const chunks: ChatCompletionChunk[] = [];
    for await (const c of stream) chunks.push(c);
    // Every yielded chunk is a well-formed chat.completion.chunk with choices.
    // Regression guard: the usage frame, passed to the OpenAI parser, yields a
    // chunk with no `choices`, which crashes naive `chunk.choices[0]` access.
    for (const c of chunks) {
      expect(c.object).toBe('chat.completion.chunk');
      expect(Array.isArray(c.choices)).toBe(true);
      expect(c.choices.length).toBeGreaterThanOrEqual(1);
    }
    const text = chunks.map((c) => c.choices[0]?.delta?.content ?? '').join('');
    expect(text).toBe('Hello world');
  });

  it('forwards tee() and strips the usage frame on both branches', async () => {
    const { fetchImpl } = stubFetch({ sse: GATEWAY_STREAM_SSE });
    const stream = await client(fetchImpl).chat.completions.create({
      model: 'm',
      messages: [{ role: 'user', content: 'hi' }],
      stream: true,
    });
    // tee() must exist on the wrapper (regression: surface reduction dropped it).
    expect(typeof stream.tee).toBe('function');
    const [a, b] = stream.tee();
    const drain = async (s: AsyncIterable<ChatCompletionChunk>) => {
      const chunks: ChatCompletionChunk[] = [];
      for await (const c of s) chunks.push(c);
      return chunks;
    };
    const [left, right] = await Promise.all([drain(a), drain(b)]);
    for (const chunks of [left, right]) {
      // Every chunk on each branch is a clean chat.completion.chunk — the usage
      // frame was stripped before it reached the tee'd streams.
      for (const c of chunks) {
        expect(c.object).toBe('chat.completion.chunk');
        expect(c.choices.length).toBeGreaterThanOrEqual(1);
      }
      expect(chunks.map((c) => c.choices[0]?.delta?.content ?? '').join('')).toBe('Hello world');
    }
    // The shared request yields the usage frame once, surfaced on the wrapper.
    expect(stream.tt).not.toBeNull();
    expect(stream.tt!.costUsd).toBe(0.0001);
  });

  it('forwards toReadableStream() with the usage frame stripped', async () => {
    const { fetchImpl } = stubFetch({ sse: GATEWAY_STREAM_SSE });
    const stream = await client(fetchImpl).chat.completions.create({
      model: 'm',
      messages: [{ role: 'user', content: 'hi' }],
      stream: true,
    });
    // toReadableStream() must exist on the wrapper (regression guard).
    expect(typeof stream.toReadableStream).toBe('function');
    const rs = stream.toReadableStream();
    const reader = (rs as ReadableStream<Uint8Array>).getReader();
    const decoder = new TextDecoder();
    let text = '';
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      text += decoder.decode(value, { stream: true });
    }
    text += decoder.decode();
    // Each newline-separated line is a JSON-stringified clean chunk; no usage
    // frame (which has no `choices`) leaked into the ReadableStream.
    const lines = text.split('\n').filter((l) => l.length > 0);
    let content = '';
    for (const line of lines) {
      const obj = JSON.parse(line) as ChatCompletionChunk & { cost_usd?: number };
      expect(obj.cost_usd).toBeUndefined();
      expect(obj.object).toBe('chat.completion.chunk');
      content += obj.choices[0]?.delta?.content ?? '';
    }
    expect(content).toBe('Hello world');
    expect(stream.tt!.costUsd).toBe(0.0001);
  });

  it('surfaces the stripped usage payload on stream.tt', async () => {
    const { fetchImpl } = stubFetch({ sse: GATEWAY_STREAM_SSE });
    const stream = await client(fetchImpl).chat.completions.create({
      model: 'm',
      messages: [{ role: 'user', content: 'hi' }],
      stream: true,
    });
    // Cost isn't known until the stream is drained.
    expect(stream.tt).toBeNull();
    for await (const _c of stream) void _c;
    const cost: StreamCost | null = stream.tt;
    expect(cost).not.toBeNull();
    expect(cost!.costUsd).toBe(0.0001);
    expect(cost!.baselineCostUsd).toBe(0.0004);
    expect(cost!.savedUsd).toBe(0.0003);
    expect(cost!.providerCacheSavedUsd).toBe(0.0);
    expect(cost!.inputTokens).toBe(10);
    expect(cost!.outputTokens).toBe(20);
    expect(cost!.cachedTokens).toBe(0);
  });
});

describe('TokenTrimmer API-key resolution', () => {
  // Precedence: explicit `apiKey` option > TOKENTRIMMER_API_KEY > OPENAI_API_KEY.
  const saved = {
    tt: process.env.TOKENTRIMMER_API_KEY,
    openai: process.env.OPENAI_API_KEY,
  };

  afterEach(() => {
    if (saved.tt === undefined) delete process.env.TOKENTRIMMER_API_KEY;
    else process.env.TOKENTRIMMER_API_KEY = saved.tt;
    if (saved.openai === undefined) delete process.env.OPENAI_API_KEY;
    else process.env.OPENAI_API_KEY = saved.openai;
  });

  it('explicit apiKey wins over TOKENTRIMMER_API_KEY', () => {
    process.env.TOKENTRIMMER_API_KEY = 'tt_from_env';
    const c = new TokenTrimmer({ apiKey: 'tt_explicit' });
    expect(c.apiKey).toBe('tt_explicit');
  });

  it('uses TOKENTRIMMER_API_KEY when no apiKey is passed', () => {
    process.env.TOKENTRIMMER_API_KEY = 'tt_from_env';
    delete process.env.OPENAI_API_KEY;
    const c = new TokenTrimmer();
    expect(c.apiKey).toBe('tt_from_env');
  });

  it('prefers TOKENTRIMMER_API_KEY over OPENAI_API_KEY', () => {
    process.env.TOKENTRIMMER_API_KEY = 'tt_from_env';
    process.env.OPENAI_API_KEY = 'tt_from_openai_env';
    const c = new TokenTrimmer();
    expect(c.apiKey).toBe('tt_from_env');
  });

  it('falls back to OPENAI_API_KEY when TOKENTRIMMER_API_KEY is unset', () => {
    delete process.env.TOKENTRIMMER_API_KEY;
    process.env.OPENAI_API_KEY = 'tt_from_openai_env';
    const c = new TokenTrimmer();
    expect(c.apiKey).toBe('tt_from_openai_env');
  });

  it('does not throw reading env in a browser-like env (no process)', () => {
    // Simulate a browser/edge runtime where `process` is undefined: the env
    // lookup must be guarded so the constructor never throws on `process.env`.
    const realProcess = globalThis.process;
    // @ts-expect-error - deliberately removing the Node `process` global.
    delete globalThis.process;
    try {
      // An explicit apiKey is supplied (the browser path has no env to read);
      // the assertion is that constructing it does not throw a ReferenceError.
      expect(() => new TokenTrimmer({ apiKey: 'tt_browser' })).not.toThrow();
    } finally {
      globalThis.process = realProcess;
    }
  });
});
