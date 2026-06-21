import { describe, expect, it } from 'vitest';
import { TokenTrimmer } from '../src/index.js';

// `TokenTrimmer extends OpenAI`, so `client.files.*` and `client.batches.*` are
// the NATIVE OpenAI typed resources — we don't reimplement them. Because
// `baseURL` points at the TokenTrimmer Gateway and its `/v1/files` + `/v1/batches`
// endpoints are OpenAI-compatible, those inherited calls route through
// TokenTrimmer transparently.
//
// These tests PIN that: the stub `fetch` returns the Gateway's ACTUAL shapes
// (`crates/core/src/routes/batches.rs::upload_file` for the FileObject, and
// `crates/core/src/batch_store.rs::BatchJob::to_openai_json` for a Batch), and
// each test asserts the inherited OpenAI surface (a) hit the expected
// `http://gw.test/v1/...` path+method and (b) parsed the response. Mirrors
// `agent.test.ts` / the Python `test_batch.py`.

const BATCH_ID = 'batch_abc123';

/** The complete OpenAI FileObject the Gateway's POST /v1/files returns. */
function fileObject() {
  return {
    id: 'file-input123',
    object: 'file',
    bytes: 42,
    created_at: 1_718_000_000,
    filename: 'batch.jsonl',
    purpose: 'batch',
    status: 'processed',
  };
}

/** A Gateway Batch body == `BatchJob::to_openai_json` for a created batch. */
function batch(status = 'validating') {
  return {
    id: BATCH_ID,
    object: 'batch',
    status,
    endpoint: '/v1/chat/completions',
    input_file_id: 'file-input123',
    completion_window: '24h',
    created_at: 1_718_000_000,
    request_counts: { total: 2, completed: 0, failed: 0 },
  };
}

interface RecordedCall {
  url: string;
  method: string;
  body: unknown;
}

/**
 * A stub `fetch` routing by path+method to the matching Gateway shape. Records
 * each call so a test can assert the URL/method the inherited OpenAI surface hit.
 */
function stubFetch() {
  const calls: RecordedCall[] = [];
  const fetchImpl = async (url: string | URL | Request, init: RequestInit = {}) => {
    const u = String(url);
    const method = (init.method ?? 'GET').toUpperCase();
    // A multipart file upload has a non-string body; JSON bodies parse.
    let body: unknown;
    if (typeof init.body === 'string') {
      try {
        body = JSON.parse(init.body);
      } catch {
        body = init.body;
      }
    }
    calls.push({ url: u, method, body });

    const path = new URL(u).pathname;
    let payload: unknown;
    if (path === '/v1/files' && method === 'POST') {
      payload = fileObject();
    } else if (path === '/v1/batches' && method === 'POST') {
      payload = batch('validating');
    } else if (path === '/v1/batches' && method === 'GET') {
      payload = { object: 'list', data: [batch('completed')], has_more: false };
    } else if (path === `/v1/batches/${BATCH_ID}/cancel` && method === 'POST') {
      payload = batch('cancelling');
    } else if (path === `/v1/batches/${BATCH_ID}` && method === 'GET') {
      payload = batch('in_progress');
    } else {
      return new Response('unexpected route', { status: 404 });
    }
    return new Response(JSON.stringify(payload), {
      status: 200,
      headers: { 'content-type': 'application/json' },
    });
  };
  return { calls, fetchImpl: fetchImpl as unknown as typeof fetch };
}

function client(fetchImpl: typeof fetch) {
  return new TokenTrimmer({
    apiKey: 'tt_test_x',
    baseURL: 'http://gw.test/v1',
    fetch: fetchImpl,
    maxRetries: 0,
  });
}

describe('TokenTrimmer inherited Batch surface', () => {
  it('files.create routes through the gateway and parses the FileObject', async () => {
    const { calls, fetchImpl } = stubFetch();
    const f = await client(fetchImpl).files.create({
      file: new File([new Blob(['{"custom_id":"req-1"}\n'])], 'batch.jsonl'),
      purpose: 'batch',
    });
    // Hit POST http://gw.test/v1/files.
    const last = calls.at(-1)!;
    expect(last.url).toBe('http://gw.test/v1/files');
    expect(last.method).toBe('POST');
    // The complete FileObject parsed into the OpenAI typed model.
    expect(f.id).toBe('file-input123');
    expect(f.object).toBe('file');
    expect(f.purpose).toBe('batch');
    expect(f.status).toBe('processed');
    expect(f.bytes).toBe(42);
    expect(f.filename).toBe('batch.jsonl');
  });

  it('batches.create routes through the gateway and parses the Batch', async () => {
    const { calls, fetchImpl } = stubFetch();
    const b = await client(fetchImpl).batches.create({
      input_file_id: 'file-input123',
      endpoint: '/v1/chat/completions',
      completion_window: '24h',
    });
    const last = calls.at(-1)!;
    expect(last.url).toBe('http://gw.test/v1/batches');
    expect(last.method).toBe('POST');
    // The create params rode in the request body.
    expect(last.body).toMatchObject({
      input_file_id: 'file-input123',
      endpoint: '/v1/chat/completions',
      completion_window: '24h',
    });
    expect(b.id).toBe(BATCH_ID);
    expect(b.object).toBe('batch');
    expect(b.status).toBe('validating');
    expect(b.endpoint).toBe('/v1/chat/completions');
    expect(b.input_file_id).toBe('file-input123');
  });

  it('batches.retrieve routes through the gateway and parses', async () => {
    const { calls, fetchImpl } = stubFetch();
    const b = await client(fetchImpl).batches.retrieve(BATCH_ID);
    const last = calls.at(-1)!;
    expect(last.url).toBe(`http://gw.test/v1/batches/${BATCH_ID}`);
    expect(last.method).toBe('GET');
    expect(b.id).toBe(BATCH_ID);
    expect(b.status).toBe('in_progress');
    expect(b.object).toBe('batch');
  });

  it('batches.list routes through the gateway and parses the first item', async () => {
    const { calls, fetchImpl } = stubFetch();
    const page = await client(fetchImpl).batches.list();
    const last = calls.at(-1)!;
    expect(last.url).toMatch(/^http:\/\/gw\.test\/v1\/batches(\?.*)?$/);
    expect(last.method).toBe('GET');
    // The list envelope's first item parsed into a Batch.
    expect(page.data[0]!.id).toBe(BATCH_ID);
    expect(page.data[0]!.status).toBe('completed');
    // Iteration over the page yields the same parsed Batch.
    const ids: string[] = [];
    for await (const item of page) ids.push(item.id);
    expect(ids).toEqual([BATCH_ID]);
  });

  it('batches.cancel routes through the gateway and parses', async () => {
    const { calls, fetchImpl } = stubFetch();
    const b = await client(fetchImpl).batches.cancel(BATCH_ID);
    const last = calls.at(-1)!;
    expect(last.url).toBe(`http://gw.test/v1/batches/${BATCH_ID}/cancel`);
    expect(last.method).toBe('POST');
    expect(b.id).toBe(BATCH_ID);
    expect(b.status).toBe('cancelling');
  });
});
