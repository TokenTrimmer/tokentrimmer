import { describe, expect, it } from 'vitest';
import { APIPromise } from 'openai/core/api-promise';
import { APIUserAbortError } from 'openai';
import { TokenTrimmer, TokenTrimmerStream, type TokenTrimmerMeta } from '../src/index.js';

const body = { model: 'test-model', messages: [] };
const chunk = { id: 'c', object: 'chat.completion.chunk', created: 1, model: 'test-model', choices: [{ index: 0, delta: { content: 'hi' }, finish_reason: null }] };
const sse = `data: ${JSON.stringify(chunk)}\n\nevent: tokentrimmer.usage\ndata: {"cost_usd":0.01}\n\ndata: [DONE]\n\n`;

function fixture(stream = false, content?: string) {
  const calls: RequestInit[] = [];
  const response = new Response(content ?? (stream ? sse : JSON.stringify({ id: 'c', choices: [] })), {
    headers: {
      'content-type': stream ? 'text/event-stream' : 'application/json',
      'x-request-id': 'req-123',
      'x-tokentrimmer-trace-id': 'trace-123',
      'x-tokentrimmer-cost-usd': '0.01',
    },
  });
  const client = new TokenTrimmer({
    apiKey: 'tt_test_local', baseURL: 'http://local.invalid/v1', maxRetries: 0,
    fetch: async (_url, init) => { calls.push(init ?? {}); return response; },
  });
  return { client, response, calls };
}

describe('explicit output caps', () => {
  it.each([0, -1, 1.5, NaN, Infinity, Number.MAX_SAFE_INTEGER + 1])('rejects invalid default %s at construction', (defaultMaxTokens) => {
    expect(() => new TokenTrimmer({ apiKey: 'tt_test_local', defaultMaxTokens })).toThrow('defaultMaxTokens');
  });

  it.each([{ max_tokens: null }, { max_completion_tokens: null }, { max_tokens: 17 }, { max_completion_tokens: 23 }])('preserves explicit limits %j without mutating caller input', async (limit) => {
    let sent: Record<string, unknown> = {};
    const client = new TokenTrimmer({
      apiKey: 'tt_test_local', defaultMaxTokens: 123,
      fetch: async (_url, init) => {
        sent = JSON.parse(init!.body as string) as Record<string, unknown>;
        return new Response('{"id":"c","choices":[]}', { headers: { 'content-type': 'application/json' } });
      },
    });
    const input = Object.freeze({ ...body, ...limit });
    await client.chat.completions.create(input);
    expect(sent).toEqual(input);
  });
});

describe('inherited response helpers', () => {
  it('retains APIPromise, metadata, request ID and a single cached parse', async () => {
    const { client, response, calls } = fixture();
    const pending = client.chat.completions.create({ ...body, ttTag: 'test' });
    expect(pending).toBeInstanceOf(APIPromise);
    const [plain, detailed, again] = await Promise.all([pending, pending.withResponse(), pending.withResponse()]);
    const meta: TokenTrimmerMeta = detailed.data.tt;
    expect(meta.traceId).toBe('trace-123');
    expect(detailed.request_id).toBe('req-123');
    expect(detailed.data).toHaveProperty('_request_id', 'req-123');
    expect(detailed.data).toBe(plain);
    expect(again.data).toBe(plain);
    expect(detailed.response).toBe(response);
    expect(calls).toHaveLength(1);
    expect(new Headers(calls[0]!.headers).get('x-tokentrimmer-tag')).toBe('test');
  });

  it('asResponse does not eagerly parse or consume even a non-JSON body', async () => {
    const { client, response, calls } = fixture(false, 'not json');
    const raw = await client.chat.completions.create(body).asResponse();
    expect(raw).toBe(response);
    expect(raw.bodyUsed).toBe(false);
    expect(await raw.text()).toBe('not json');
    expect(calls).toHaveLength(1);
  });

  it('withResponse returns the augmented stream; usage remains unknown until drained', async () => {
    const { client, response, calls } = fixture(true);
    const pending = client.chat.completions.create({ ...body, stream: true, ttCache: 'disabled' });
    const detailed = await pending.withResponse();
    expect(detailed.data).toBeInstanceOf(TokenTrimmerStream);
    expect(detailed.data).toBe(await pending);
    expect(detailed.response).toBe(response);
    expect(detailed.request_id).toBe('req-123');
    expect(detailed.data.tt).toBeNull();
    const chunks = [];
    for await (const c of detailed.data) chunks.push(c);
    expect(chunks).toEqual([chunk]);
    expect(detailed.data.tt?.costUsd).toBe(0.01);
    expect(calls).toHaveLength(1);
  });

  it('asResponse leaves the raw SSE body and terminal usage frame untouched', async () => {
    const { client } = fixture(true);
    const raw = await client.chat.completions.create({ ...body, stream: true }).asResponse();
    expect(raw.bodyUsed).toBe(false);
    expect(await raw.text()).toBe(sse);
  });

  it('supports variable streaming flags with tt params and typed helpers', async () => {
    const { client } = fixture();
    const stream: boolean = Boolean(0);
    const { data } = await client.chat.completions.create({ ...body, stream, ttTag: 'variable' }).withResponse();
    if (!(data instanceof TokenTrimmerStream)) expect(data.tt.costUsd).toBe(0.01);
  });

  it('keeps validation failures asynchronous on every consumption path without I/O', async () => {
    const { client, calls } = fixture();
    for (const helper of ['asResponse', 'withResponse'] as const) {
      const pending = client.chat.completions.create({ ...body, ttCostLimit: -1 });
      expect(pending).toBeInstanceOf(APIPromise);
      await expect(pending[helper]()).rejects.toThrow('ttCostLimit');
      await expect(pending).rejects.toThrow('ttCostLimit');
    }
    expect(calls).toHaveLength(0);
  });

  it('preserves pre-aborted requests and does not send or retry', async () => {
    const { client, calls } = fixture();
    const controller = new AbortController();
    controller.abort();
    const pending = client.chat.completions.create(body, { signal: controller.signal });
    await expect(pending.withResponse()).rejects.toBeInstanceOf(APIUserAbortError);
    expect(calls).toHaveLength(0);
  });

  it('aborts the underlying stream when iteration is cancelled; no invented terminal cost', async () => {
    let cancelled = false;
    const client = new TokenTrimmer({
      apiKey: 'tt_test_local', baseURL: 'http://local.invalid/v1', maxRetries: 0,
      fetch: async () => new Response(new ReadableStream({
        start(controller) { controller.enqueue(new TextEncoder().encode(`data: ${JSON.stringify(chunk)}\n\n`)); },
        cancel() { cancelled = true; },
      }), { headers: { 'content-type': 'text/event-stream' } }),
    });
    const { data } = await client.chat.completions.create({ ...body, stream: true }).withResponse();
    for await (const c of data) { expect(c).toEqual(chunk); break; }
    expect(data.controller.signal.aborted).toBe(true);
    expect(cancelled).toBe(true);
    expect(data.tt).toBeNull();
  });
});
