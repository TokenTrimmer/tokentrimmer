import assert from 'node:assert/strict';
import { test } from 'node:test';
import { TokenTrimmer, TokenTrimmerStream } from '@tokentrimmer/client';
import { APIUserAbortError } from 'openai';
import { APIPromise } from 'openai/core/api-promise';

const body = { model: 'test-model', messages: [] };
const chunk = { id: 'c', object: 'chat.completion.chunk', created: 1, model: 'test-model', choices: [{ index: 0, delta: { content: 'hi' }, finish_reason: null }] };
const sse = `data: ${JSON.stringify(chunk)}\n\nevent: tokentrimmer.usage\ndata: {"cost_usd":0.01}\n\ndata: [DONE]\n\n`;
function fixture(options = {}, responseBody = '{"id":"c","choices":[]}', streaming = false) {
  const calls = [];
  const response = new Response(responseBody, { headers: {
    'content-type': streaming ? 'text/event-stream' : 'application/json',
    'x-tokentrimmer-cost-usd': '0.01', 'x-request-id': 'req-1',
  } });
  const client = new TokenTrimmer({
    apiKey: 'tt_test_local', baseURL: 'http://local.invalid/v1', maxRetries: 0,
    fetch: async (_url, init) => { calls.push(init); return response; }, ...options,
  });
  return { client, calls, response };
}

test('packed helpers preserve lazy parsing, metadata, request ID and single I/O', async () => {
  const { client, calls } = fixture();
  const pending = client.chat.completions.create({ ...body, ttTag: 'tag', ttCostLimit: 0.05, ttCache: 'disabled' });
  assert.ok(pending instanceof APIPromise);
  const raw = await pending.asResponse();
  assert.equal(raw.bodyUsed, false);
  const { data, response, request_id } = await pending.withResponse();
  assert.equal(response, raw);
  assert.equal(data, await pending);
  assert.equal(data.tt.costUsd, 0.01);
  assert.equal(request_id, 'req-1');
  assert.equal(data._request_id, 'req-1');
  assert.equal(calls.length, 1);
  const headers = new Headers(calls[0].headers);
  assert.equal(headers.get('x-tokentrimmer-tag'), 'tag');
  assert.equal(headers.get('x-tokentrimmer-cost-limit-usd'), '0.05');
  assert.equal(headers.get('x-tokentrimmer-cache'), 'disabled');
  assert.deepEqual(JSON.parse(calls[0].body), body);
});

test('packed limits are opt-in, explicit limits and null win, input is immutable', async () => {
  for (const [options, limit, expected] of [
    [{}, {}, {}], [{ defaultMaxTokens: 32 }, {}, { max_tokens: 32 }],
    [{ defaultMaxTokens: 32 }, { max_tokens: 8 }, { max_tokens: 8 }],
    [{ defaultMaxTokens: 32 }, { max_completion_tokens: 16 }, { max_completion_tokens: 16 }],
    [{ defaultMaxTokens: 32 }, { max_tokens: null }, { max_tokens: null }],
  ]) {
    const { client, calls } = fixture(options);
    const input = Object.freeze({ ...body, ...limit });
    await client.chat.completions.create(input);
    assert.deepEqual(JSON.parse(calls[0].body), { ...body, ...expected });
  }
  for (const defaultMaxTokens of [0, -1, NaN, Infinity, 0.5]) {
    assert.throws(() => fixture({ defaultMaxTokens }), /defaultMaxTokens/);
  }
});

test('packed streaming helper strips usage and retains final cost', async () => {
  const { client } = fixture({}, sse, true);
  const { data, request_id } = await client.chat.completions.create({ ...body, stream: true }).withResponse();
  assert.ok(data instanceof TokenTrimmerStream);
  assert.equal(request_id, 'req-1');
  assert.equal(data.tt, null);
  const chunks = [];
  for await (const c of data) chunks.push(c);
  assert.deepEqual(chunks, [chunk]);
  assert.equal(data.tt.costUsd, 0.01);
});

test('packed raw helper does not parse JSON or strip SSE', async () => {
  for (const [wire, stream] of [['not-json', false], [sse, true]]) {
    const { client } = fixture({}, wire, stream);
    const raw = await client.chat.completions.create({ ...body, stream }).asResponse();
    assert.equal(raw.bodyUsed, false);
    assert.equal(await raw.text(), wire);
  }
});

test('packed helpers preserve validation and pre-abort errors with zero I/O', async () => {
  const { client, calls } = fixture();
  await assert.rejects(client.chat.completions.create({ ...body, ttCostLimit: -1 }).asResponse(), /ttCostLimit/);
  const controller = new AbortController();
  controller.abort();
  await assert.rejects(client.chat.completions.create(body, { signal: controller.signal }).withResponse(), APIUserAbortError);
  assert.equal(calls.length, 0);
});

test('packed in-flight abort propagates through helpers without retry', async () => {
  let started;
  const ready = new Promise((resolve) => { started = resolve; });
  let calls = 0;
  const { client } = fixture({ fetch: async (_url, init) => {
    calls++;
    return new Promise((_resolve, reject) => {
      init.signal.addEventListener('abort', () => reject(new DOMException('Aborted', 'AbortError')), { once: true });
      started();
    });
  } });
  const controller = new AbortController();
  const result = assert.rejects(client.chat.completions.create(body, { signal: controller.signal }).withResponse(), APIUserAbortError);
  await ready;
  controller.abort();
  await result;
  assert.equal(calls, 1);
});

test('packed stream cancellation closes transport without fabricated cost', async () => {
  let closed = false;
  const { client } = fixture({ fetch: async () => new Response(new ReadableStream({
    start(controller) { controller.enqueue(new TextEncoder().encode(`data: ${JSON.stringify(chunk)}\n\n`)); },
    cancel() { closed = true; },
  }), { headers: { 'content-type': 'text/event-stream' } }) });
  const stream = await client.chat.completions.create({ ...body, stream: true });
  for await (const _ of stream) break;
  assert.ok(stream.controller.signal.aborted);
  assert.ok(closed);
  assert.equal(stream.tt, null);
});
