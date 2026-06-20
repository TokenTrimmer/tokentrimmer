import { describe, expect, it } from 'vitest';
import { TokenTrimmer } from '../src/index.js';

// These mirror the Rust `tt-client` agent driver tests
// (crates/client/src/agent.rs) and the Python `test_agent.py`: a multi-turn run
// that executes a client tool then resumes to a final answer with aggregated
// cost; a resume-cap test; an executor-error-fed-back test; and a
// terminal-on-first-response test. The gateway is mocked via a stub `fetch`.
//
// Endpoint shape (crates/client/src/agent.rs):
//   POST /v1/agent/runs                       { model, messages, tools?, max_turns?, stream }
//   POST /v1/agent/runs/{id}/tool_outputs     { tool_outputs: [{ tool_call_id, output }] }
// A `Run` body = { id, status, messages, turns, usage{...,cost_usd}, ... }. NO
// x-tokentrimmer-* headers; aggregate cost is `run.usage.cost_usd`.

const RUN_ID = '11111111-1111-1111-1111-111111111111';

function requiresActionRun() {
  return {
    id: RUN_ID,
    status: 'requires_action',
    turns: 1,
    usage: { prompt_tokens: 10, completion_tokens: 4, cost_usd: 0.0002 },
    messages: [
      { role: 'user', content: 'do it' },
      {
        role: 'assistant',
        content: null,
        tool_calls: [
          { id: 'call_1', type: 'function', function: { name: 'do_thing', arguments: '{"x":1}' } },
        ],
      },
    ],
  };
}

function completedRunAfterResume() {
  return {
    id: RUN_ID,
    status: 'completed',
    turns: 2,
    usage: { prompt_tokens: 22, completion_tokens: 9, cost_usd: 0.0005 },
    messages: [
      { role: 'user', content: 'do it' },
      {
        role: 'assistant',
        content: null,
        tool_calls: [
          { id: 'call_1', type: 'function', function: { name: 'do_thing', arguments: '{"x":1}' } },
        ],
      },
      { role: 'tool', tool_call_id: 'call_1', content: 'did the thing' },
      { role: 'assistant', content: 'All done.' },
    ],
  };
}

interface RecordedCall {
  url: string;
  init: RequestInit;
  body: unknown;
}

/**
 * A stub `fetch` routing by path: requests to `.../tool_outputs` get the resume
 * response, everything else (the create) gets the create response. Each entry is
 * a function so a route can return different bodies per call (for the cap test).
 */
function stubFetch(routes: { create: () => unknown; resume: () => unknown }) {
  const calls: RecordedCall[] = [];
  const fetchImpl = async (url: string | URL | Request, init: RequestInit = {}) => {
    const u = String(url);
    const body = init.body ? JSON.parse(init.body as string) : undefined;
    calls.push({ url: u, init, body });
    const payload = u.endsWith('/tool_outputs') ? routes.resume() : routes.create();
    if (payload instanceof Response) return payload;
    return new Response(JSON.stringify(payload), {
      status: 200,
      headers: { 'content-type': 'application/json' },
    });
  };
  return { calls, fetchImpl: fetchImpl as unknown as typeof fetch };
}

function client(fetchImpl: typeof fetch) {
  // `maxRetries: 0` so a mocked 5xx surfaces immediately (no real backoff delay)
  // — the driver inherits the SDK's retry policy in production unchanged.
  return new TokenTrimmer({
    apiKey: 'tt_test_x',
    baseURL: 'http://gw.test/v1',
    fetch: fetchImpl,
    maxRetries: 0,
  });
}

const TOOLS = [
  {
    type: 'function' as const,
    function: { name: 'do_thing', description: 'does', parameters: { type: 'object' } },
  },
];

describe('TokenTrimmer agent loop', () => {
  it('executes a client tool then resumes to completion with aggregated cost', async () => {
    const { calls, fetchImpl } = stubFetch({
      create: requiresActionRun,
      resume: completedRunAfterResume,
    });
    const seen: { name?: string; args?: string } = {};
    const out = await client(fetchImpl).agent.run({
      model: 'gpt-4o-mini',
      messages: [
        { role: 'system', content: 'be helpful' },
        { role: 'user', content: 'do it' },
      ],
      tools: TOOLS,
      executor: (name, args) => {
        seen.name = name;
        seen.args = args;
        return 'did the thing';
      },
    });

    // The executor saw the model's tool call.
    expect(seen.name).toBe('do_thing');
    expect(seen.args).toBe('{"x":1}');
    expect(out.resumeRounds).toBe(1);
    expect(out.run.status).toBe('completed');
    expect(out.text).toBe('All done.');
    // Aggregate cost is read from the terminal run body, not headers.
    expect(out.usage.costUsd).toBe(0.0005);
    expect(out.usage.promptTokens).toBe(22);
    // create + 1 resume.
    expect(calls.length).toBe(2);
    expect(calls[0]!.url).toBe('http://gw.test/v1/agent/runs');
    expect(calls[1]!.url).toBe(`http://gw.test/v1/agent/runs/${RUN_ID}/tool_outputs`);
    // The resume body carries the tool output keyed by the pending call id.
    expect(calls[1]!.body).toEqual({
      tool_outputs: [{ tool_call_id: 'call_1', output: 'did the thing' }],
    });
  });

  it('returns immediately when the first response is terminal (no resume)', async () => {
    const { calls, fetchImpl } = stubFetch({
      create: () => ({
        id: 'abc',
        status: 'completed',
        turns: 3,
        usage: { prompt_tokens: 30, completion_tokens: 12, cost_usd: 0.0009 },
        messages: [
          { role: 'user', content: 'hi' },
          { role: 'assistant', content: 'Answer.' },
        ],
      }),
      resume: () => {
        throw new Error('resume must not be called');
      },
    });
    const out = await client(fetchImpl).agent.run({
      model: 'gpt-4o-mini',
      messages: [{ role: 'user', content: 'hi' }],
      executor: () => {
        throw new Error('executor must not run when nothing pauses');
      },
    });
    expect(out.resumeRounds).toBe(0);
    expect(out.text).toBe('Answer.');
    expect(out.usage.costUsd).toBe(0.0009);
    expect(calls.length).toBe(1);
  });

  it('stops at the resume cap when the run keeps pausing', async () => {
    // Every create/resume returns the SAME requires_action run. With
    // maxResumeRounds=2 the driver makes the create + exactly 2 resumes.
    const { calls, fetchImpl } = stubFetch({
      create: requiresActionRun,
      resume: requiresActionRun,
    });
    const out = await client(fetchImpl).agent.run({
      model: 'gpt-4o-mini',
      messages: [{ role: 'user', content: 'loop' }],
      tools: TOOLS,
      executor: () => 'again',
      maxResumeRounds: 2,
    });
    expect(out.resumeRounds).toBe(2);
    expect(out.run.status).toBe('requires_action');
    // 1 create + 2 resumes.
    expect(calls.length).toBe(3);
  });

  it('feeds an executor error back as the tool output (never aborts)', async () => {
    const { calls, fetchImpl } = stubFetch({
      create: requiresActionRun,
      resume: completedRunAfterResume,
    });
    const out = await client(fetchImpl).agent.run({
      model: 'gpt-4o-mini',
      messages: [{ role: 'user', content: 'go' }],
      tools: TOOLS,
      executor: () => {
        throw new Error('tool exploded');
      },
    });
    expect(out.run.status).toBe('completed');
    const resumeBody = calls.find((c) => c.url.endsWith('/tool_outputs'))!.body as {
      tool_outputs: Array<{ output: string }>;
    };
    const parsed = JSON.parse(resumeBody.tool_outputs[0]!.output) as { error: string };
    expect(parsed.error).toContain('exploded');
  });

  it('awaits an async executor', async () => {
    const { fetchImpl } = stubFetch({
      create: requiresActionRun,
      resume: completedRunAfterResume,
    });
    const out = await client(fetchImpl).agent.run({
      model: 'gpt-4o-mini',
      messages: [{ role: 'user', content: 'go' }],
      tools: TOOLS,
      executor: async (_name, _args) => {
        await Promise.resolve();
        return 'did the thing';
      },
    });
    expect(out.run.status).toBe('completed');
    expect(out.text).toBe('All done.');
  });

  it('forwards ttTag and maxTurns on the create request', async () => {
    const { calls, fetchImpl } = stubFetch({
      create: () => ({
        id: 'z',
        status: 'completed',
        turns: 1,
        usage: { prompt_tokens: 1, completion_tokens: 1, cost_usd: 0 },
        messages: [{ role: 'assistant', content: 'ok' }],
      }),
      resume: () => ({}),
    });
    await client(fetchImpl).agent.run({
      model: 'gpt-4o-mini',
      messages: [{ role: 'user', content: 'hi' }],
      executor: () => 'x',
      maxTurns: 4,
      ttTag: 'proj-x',
    });
    const create = calls[0]!;
    const h = new Headers(create.init.headers as HeadersInit);
    expect(h.get('x-tokentrimmer-tag')).toBe('proj-x');
    expect(create.body).toMatchObject({ model: 'gpt-4o-mini', max_turns: 4, stream: false });
  });

  it('throws on an empty model before any request', async () => {
    const { calls, fetchImpl } = stubFetch({ create: () => ({}), resume: () => ({}) });
    await expect(
      client(fetchImpl).agent.run({
        model: '',
        messages: [{ role: 'user', content: 'hi' }],
        executor: () => 'x',
      }),
    ).rejects.toThrow();
    expect(calls.length).toBe(0);
  });

  it('propagates a gateway error on create', async () => {
    const { fetchImpl } = stubFetch({
      create: () => new Response('boom', { status: 500 }),
      resume: completedRunAfterResume,
    });
    await expect(
      client(fetchImpl).agent.run({
        model: 'gpt-4o-mini',
        messages: [{ role: 'user', content: 'go' }],
        executor: () => 'x',
      }),
    ).rejects.toThrow();
  });

  it('propagates a gateway error on resume', async () => {
    const { fetchImpl } = stubFetch({
      create: requiresActionRun,
      resume: () => new Response('store unavailable', { status: 503 }),
    });
    await expect(
      client(fetchImpl).agent.run({
        model: 'gpt-4o-mini',
        messages: [{ role: 'user', content: 'go' }],
        tools: TOOLS,
        executor: () => 'x',
      }),
    ).rejects.toThrow();
  });
});
