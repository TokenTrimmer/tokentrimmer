import { describe, expect, it } from 'vitest';

import { TokenTrimmerApi } from '../credentials/TokenTrimmerApi.credentials';
import { normalizeBaseUrl } from '../nodes/TokenTrimmerChat/GenericFunctions';

const credential = new TokenTrimmerApi();

/**
 * The credential test request is declarative (ICredentialTestRequest), so it
 * cannot call normalizeBaseUrl — it carries an n8n expression instead. These
 * tests evaluate that expression's inner JavaScript directly (the expression
 * uses only standard String methods, which n8n's expression engine evaluates
 * with plain-JS semantics) and assert it stays in lockstep with
 * normalizeBaseUrl for every URL shape the node forgives.
 */
/**
 * Only `$credentials.baseUrl` followed by chained `.trim()` /
 * `.replace(/regex/flags, "")` calls — the exact grammar the credential
 * expression is allowed to use. Anything else fails the test BEFORE being
 * evaluated, so the expression can never drift into syntax that plain-JS
 * evaluation would model unfaithfully (or that should not be executed).
 */
const ALLOWED_EXPRESSION =
	/^\s*\$credentials\.baseUrl(?:\.trim\(\)|\.replace\(\/(?:\\.|[^/\\])+\/[a-z]*,\s*""\))*\s*$/;

function evaluateBaseUrlExpression(baseUrl: string): unknown {
	const raw = credential.test.request.baseURL as string;
	expect(raw.startsWith('={{')).toBe(true);
	expect(raw.endsWith('}}')).toBe(true);
	const inner = raw.slice('={{'.length, -'}}'.length);
	// Evaluating OUR OWN committed constant (no external input), and only
	// after the allowlist above pinned its shape.
	expect(inner).toMatch(ALLOWED_EXPRESSION);
	// eslint-disable-next-line @typescript-eslint/no-implied-eval
	return new Function('$credentials', `return (${inner});`)({ baseUrl });
}

describe('TokenTrimmerApi credential test request', () => {
	it('probes GET /v1/models against the normalized origin', () => {
		expect(credential.test.request.url).toBe('/v1/models');
	});

	it.each([
		'https://api.tokentrimmer.com',
		'https://api.tokentrimmer.com/',
		'https://api.tokentrimmer.com/v1',
		'https://api.tokentrimmer.com/v1/',
		'https://api.tokentrimmer.com/V1',
		'http://localhost:8080',
		'http://localhost:8080/v1',
		'http://host.docker.internal:8080/v1/',
		'  http://localhost:8080/v1  ',
	])('baseURL expression matches normalizeBaseUrl for %j', (input) => {
		expect(evaluateBaseUrlExpression(input)).toBe(normalizeBaseUrl(input));
	});

	it('strips only ONE trailing /v1, same as the node', () => {
		const input = 'https://api.tokentrimmer.com/v1/v1';
		expect(evaluateBaseUrlExpression(input)).toBe(normalizeBaseUrl(input));
		expect(evaluateBaseUrlExpression(input)).toBe('https://api.tokentrimmer.com/v1');
	});

	it('does not touch a /v1 that is not a trailing path segment', () => {
		const input = 'https://api.tokentrimmer.com/v1beta';
		expect(evaluateBaseUrlExpression(input)).toBe('https://api.tokentrimmer.com/v1beta');
		expect(normalizeBaseUrl(input)).toBe('https://api.tokentrimmer.com/v1beta');
	});
});

describe('TokenTrimmerApi credential shape', () => {
	it('masks the API key field', () => {
		const apiKey = credential.properties.find((p) => p.name === 'apiKey');
		expect(apiKey?.typeOptions?.password).toBe(true);
	});

	it('authenticates with a Bearer Authorization header', () => {
		expect(credential.authenticate.properties.headers?.Authorization).toBe(
			'=Bearer {{$credentials.apiKey}}',
		);
	});
});
