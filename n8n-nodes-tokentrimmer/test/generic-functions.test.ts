import { describe, expect, it } from 'vitest';

import {
	buildMessages,
	buildRequestBody,
	buildTtHeaders,
	chatCompletionsUrl,
	normalizeBaseUrl,
	parseCostInfo,
} from '../nodes/TokenTrimmerChat/GenericFunctions';

describe('parseCostInfo', () => {
	// Header values straight from docs/04-gateway-api-reference.md §6.2 examples.
	const fullHeaders = {
		'x-tokentrimmer-trace-id': '5f3a1c...',
		'x-tokentrimmer-latency-ms': '412',
		'x-tokentrimmer-provider': 'anthropic',
		'x-tokentrimmer-model-used': 'claude-3-5-haiku-20241022',
		'x-tokentrimmer-cache': 'hit-l1',
		'x-tokentrimmer-cost-usd': '0.0034',
		'x-tokentrimmer-baseline-cost-usd': '0.0218',
		'x-tokentrimmer-saved-usd': '0.0184',
		'x-tokentrimmer-provider-cache-saved-usd': '0.0009',
	};

	it('parses the full §6.2 header set and computes savingsPct', () => {
		const info = parseCostInfo(fullHeaders);
		expect(info.costUsd).toBe(0.0034);
		expect(info.baselineCostUsd).toBe(0.0218);
		expect(info.savedUsd).toBe(0.0184);
		expect(info.providerCacheSavedUsd).toBe(0.0009);
		expect(info.modelUsed).toBe('claude-3-5-haiku-20241022');
		expect(info.provider).toBe('anthropic');
		expect(info.cache).toBe('hit-l1');
		expect(info.traceId).toBe('5f3a1c...');
		// savingsPct is COMPUTED (saved / baseline * 100) — there is no gateway header for it.
		expect(info.savingsPct).not.toBeNull();
		expect(info.savingsPct as number).toBeCloseTo(84.4, 1);
	});

	it('returns all-null on absent headers (early-4xx shape)', () => {
		const info = parseCostInfo({});
		expect(info.costUsd).toBeNull();
		expect(info.baselineCostUsd).toBeNull();
		expect(info.savedUsd).toBeNull();
		expect(info.savingsPct).toBeNull();
		expect(info.providerCacheSavedUsd).toBeNull();
		expect(info.modelUsed).toBeNull();
		expect(info.provider).toBeNull();
		expect(info.cache).toBeNull();
		expect(info.traceId).toBeNull();
	});

	it('leaves savingsPct null when baseline is missing', () => {
		const info = parseCostInfo({ 'x-tokentrimmer-saved-usd': '0.0184' });
		expect(info.savedUsd).toBe(0.0184);
		expect(info.savingsPct).toBeNull();
	});

	it('leaves savingsPct null when baseline is zero (no division by zero)', () => {
		const info = parseCostInfo({
			'x-tokentrimmer-saved-usd': '0',
			'x-tokentrimmer-baseline-cost-usd': '0',
		});
		expect(info.baselineCostUsd).toBe(0);
		expect(info.savingsPct).toBeNull();
	});

	it('nulls an unparseable numeric field without affecting the others', () => {
		const info = parseCostInfo({
			...fullHeaders,
			'x-tokentrimmer-cost-usd': 'abc',
		});
		expect(info.costUsd).toBeNull();
		expect(info.baselineCostUsd).toBe(0.0218);
		expect(info.savedUsd).toBe(0.0184);
		expect(info.savingsPct as number).toBeCloseTo(84.4, 1);
	});

	it('handles mixed-case header keys', () => {
		const info = parseCostInfo({
			'X-TokenTrimmer-Cost-Usd': '0.0034',
			'X-TOKENTRIMMER-MODEL-USED': 'gpt-4o-mini',
		});
		expect(info.costUsd).toBe(0.0034);
		expect(info.modelUsed).toBe('gpt-4o-mini');
	});

	it('tolerates string[] header values by taking the first element', () => {
		const info = parseCostInfo({
			'x-tokentrimmer-saved-usd': ['0.0184', '9.9'],
			'x-tokentrimmer-baseline-cost-usd': ['0.0218'],
			'x-tokentrimmer-cache': ['hit-l2'],
		});
		expect(info.savedUsd).toBe(0.0184);
		expect(info.baselineCostUsd).toBe(0.0218);
		expect(info.cache).toBe('hit-l2');
		expect(info.savingsPct as number).toBeCloseTo(84.4, 1);
	});
});

describe('normalizeBaseUrl', () => {
	it('strips trailing slashes', () => {
		expect(normalizeBaseUrl('https://api.tokentrimmer.com/')).toBe('https://api.tokentrimmer.com');
		expect(normalizeBaseUrl('https://api.tokentrimmer.com//')).toBe(
			'https://api.tokentrimmer.com',
		);
	});

	it('strips one trailing /v1 (and /v1/)', () => {
		expect(normalizeBaseUrl('https://api.tokentrimmer.com/v1')).toBe(
			'https://api.tokentrimmer.com',
		);
		expect(normalizeBaseUrl('https://api.tokentrimmer.com/v1/')).toBe(
			'https://api.tokentrimmer.com',
		);
	});

	it('passes a plain self-host origin through', () => {
		expect(normalizeBaseUrl('http://localhost:8080')).toBe('http://localhost:8080');
	});

	it('trims surrounding whitespace', () => {
		expect(normalizeBaseUrl('  http://localhost:8080  ')).toBe('http://localhost:8080');
	});

	it('throws on empty input', () => {
		expect(() => normalizeBaseUrl('')).toThrow();
		expect(() => normalizeBaseUrl('   ')).toThrow();
	});

	it('throws on a non-http(s) scheme', () => {
		expect(() => normalizeBaseUrl('ftp://example.com')).toThrow();
		expect(() => normalizeBaseUrl('api.tokentrimmer.com')).toThrow();
	});
});

describe('chatCompletionsUrl', () => {
	it('appends /v1/chat/completions to the origin', () => {
		expect(chatCompletionsUrl('https://api.tokentrimmer.com')).toBe(
			'https://api.tokentrimmer.com/v1/chat/completions',
		);
	});

	it('does not double up /v1 when the credential already has it', () => {
		expect(chatCompletionsUrl('https://api.tokentrimmer.com/v1/')).toBe(
			'https://api.tokentrimmer.com/v1/chat/completions',
		);
	});
});

describe('buildTtHeaders', () => {
	it('builds exactly the three documented request headers', () => {
		const headers = buildTtHeaders({
			tag: 'feature=my-workflow',
			costLimitUsd: 0.05,
			cacheOverride: 'bypass',
		});
		expect(headers).toEqual({
			'X-TokenTrimmer-Tag': 'feature=my-workflow',
			'X-TokenTrimmer-Cost-Limit-Usd': '0.05',
			'X-TokenTrimmer-Cache': 'bypass',
		});
	});

	it('omits keys for absent options (no empty headers)', () => {
		expect(buildTtHeaders({})).toEqual({});
		expect(buildTtHeaders({ tag: '' })).toEqual({});
	});

	it('throws on a negative, NaN, or Infinity cost limit', () => {
		expect(() => buildTtHeaders({ costLimitUsd: -1 })).toThrow();
		expect(() => buildTtHeaders({ costLimitUsd: Number.NaN })).toThrow();
		expect(() => buildTtHeaders({ costLimitUsd: Number.POSITIVE_INFINITY })).toThrow();
	});

	it('accepts a zero cost limit', () => {
		expect(buildTtHeaders({ costLimitUsd: 0 })).toEqual({
			'X-TokenTrimmer-Cost-Limit-Usd': '0',
		});
	});

	it('accepts every documented cache override and rejects anything else', () => {
		for (const value of ['bypass', 'force-write', 'read-only', 'disabled']) {
			expect(buildTtHeaders({ cacheOverride: value })).toEqual({
				'X-TokenTrimmer-Cache': value,
			});
		}
		expect(() => buildTtHeaders({ cacheOverride: 'hit-l1' })).toThrow();
		expect(() => buildTtHeaders({ cacheOverride: 'nope' })).toThrow();
	});
});

describe('buildMessages (prompt mode)', () => {
	it('builds a single user message from a prompt', () => {
		expect(buildMessages({ mode: 'prompt', prompt: 'Hello' })).toEqual([
			{ role: 'user', content: 'Hello' },
		]);
	});

	it('puts the system prompt first when provided', () => {
		expect(
			buildMessages({ mode: 'prompt', prompt: 'Hello', systemPrompt: 'Be terse.' }),
		).toEqual([
			{ role: 'system', content: 'Be terse.' },
			{ role: 'user', content: 'Hello' },
		]);
	});

	it('throws on an empty prompt', () => {
		expect(() => buildMessages({ mode: 'prompt', prompt: '' })).toThrow();
		expect(() => buildMessages({ mode: 'prompt' })).toThrow();
	});
});

describe('buildMessages (messagesJson mode)', () => {
	it('passes a valid OpenAI messages array through untouched', () => {
		const messages = [
			{ role: 'system', content: 'Be terse.' },
			{ role: 'user', content: 'Hello' },
		];
		expect(
			buildMessages({ mode: 'messagesJson', messagesJson: JSON.stringify(messages) }),
		).toEqual(messages);
	});

	it('passes array-of-parts content through untouched (API ref §3.6)', () => {
		const messages = [
			{
				role: 'user',
				content: [
					{ type: 'text', text: "What's in this image?" },
					{ type: 'image_url', image_url: { url: 'data:image/jpeg;base64,...' } },
				],
			},
		];
		expect(
			buildMessages({ mode: 'messagesJson', messagesJson: JSON.stringify(messages) }),
		).toEqual(messages);
	});

	it('throws on invalid JSON', () => {
		expect(() => buildMessages({ mode: 'messagesJson', messagesJson: '{nope' })).toThrow();
	});

	it('throws on non-array JSON', () => {
		expect(() =>
			buildMessages({ mode: 'messagesJson', messagesJson: '{"role":"user"}' }),
		).toThrow();
	});

	it('accepts an already-parsed array (expression resolved to a real value)', () => {
		const messages = [
			{ role: 'system', content: 'Be terse.' },
			{ role: 'user', content: 'Hello' },
		];
		expect(buildMessages({ mode: 'messagesJson', messagesJson: messages })).toEqual(messages);
	});

	it('validates an already-parsed value the same as parsed JSON', () => {
		expect(() =>
			buildMessages({ mode: 'messagesJson', messagesJson: { role: 'user' } }),
		).toThrow(/array/);
		expect(() =>
			buildMessages({ mode: 'messagesJson', messagesJson: [{ content: 'hi' }] }),
		).toThrow(/string "role"/);
		expect(() => buildMessages({ mode: 'messagesJson', messagesJson: undefined })).toThrow(
			/array/,
		);
	});

	it('throws when an element is missing a string role', () => {
		expect(() =>
			buildMessages({ mode: 'messagesJson', messagesJson: '[{"content":"hi"}]' }),
		).toThrow();
		expect(() =>
			buildMessages({ mode: 'messagesJson', messagesJson: '[{"role":1,"content":"hi"}]' }),
		).toThrow();
		expect(() => buildMessages({ mode: 'messagesJson', messagesJson: '["hi"]' })).toThrow();
	});
});

describe('buildRequestBody', () => {
	const messages = [{ role: 'user', content: 'Hello' }];

	it('contains only model and messages by default', () => {
		expect(buildRequestBody({ model: 'claude-haiku-4-5', messages })).toEqual({
			model: 'claude-haiku-4-5',
			messages,
		});
	});

	it('includes max_tokens and temperature only when provided', () => {
		expect(
			buildRequestBody({ model: 'm', messages, maxTokens: 256, temperature: 0.2 }),
		).toEqual({ model: 'm', messages, max_tokens: 256, temperature: 0.2 });
	});

	it('never sets stream', () => {
		const body = buildRequestBody({ model: 'm', messages, maxTokens: 1, temperature: 0 });
		expect(Object.keys(body)).not.toContain('stream');
	});
});
