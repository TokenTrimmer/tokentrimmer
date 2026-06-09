import { describe, expect, it } from 'vitest';
import { TokenTrimmer } from '../src/index.js';

const TT_HEADERS = {
  'x-tokentrimmer-trace-id': 'trace-1',
  'x-tokentrimmer-provider': 'anthropic',
  'x-tokentrimmer-model-used': 'claude-haiku-4-5',
  'x-tokentrimmer-cost-usd': '0.0034',
  'x-tokentrimmer-baseline-cost-usd': '0.02',
  'x-tokentrimmer-saved-usd': '0.0166',
  'x-tokentrimmer-cache': 'miss',
};

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
    expect((res as any).tt.traceId).toBe('trace-1');
    expect((res as any).tt.costUsd).toBe(0.0034);
    expect((res as any).tt.cache).toBe('miss');
  });

  it('parses a non-numeric cost header to null', async () => {
    const { fetchImpl } = stubFetch({ headers: { 'x-tokentrimmer-cost-usd': 'nope' } });
    const res = await client(fetchImpl).chat.completions.create({
      model: 'm',
      messages: [{ role: 'user', content: 'hi' }],
    });
    expect((res as any).tt.costUsd).toBeNull();
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
    } as any);
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
        ttCache: 'hit-l1', // a response value, not a valid request override
      } as any),
    ).rejects.toThrow();
    expect(calls.length).toBe(0);
  });

  it('throws on a non-finite or negative ttCostLimit before sending', async () => {
    const { calls, fetchImpl } = stubFetch();
    for (const bad of [Infinity, NaN, -1]) {
      await expect(
        client(fetchImpl).chat.completions.create({
          model: 'm',
          messages: [{ role: 'user', content: 'hi' }],
          ttCostLimit: bad,
        } as any),
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
    expect((stream as any).tt).toBeUndefined();
    expect(typeof (stream as any)[Symbol.asyncIterator]).toBe('function');
    const chunks: unknown[] = [];
    for await (const c of stream as any) chunks.push(c);
    expect(chunks.length).toBeGreaterThanOrEqual(1);
  });
});
