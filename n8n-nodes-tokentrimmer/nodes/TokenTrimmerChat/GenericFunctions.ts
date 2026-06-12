/**
 * Pure, side-effect-free helpers for the TokenTrimmer Chat node.
 *
 * Everything here is unit-tested without any n8n runtime mocking
 * (test/generic-functions.test.ts). The node's execute() wires these
 * together around `helpers.httpRequestWithAuthentication`.
 *
 * Header names are ground truth from docs/04-gateway-api-reference.md:
 * - request (§6.1): `X-TokenTrimmer-Tag`, `X-TokenTrimmer-Cost-Limit-Usd`,
 *   `X-TokenTrimmer-Cache`
 * - response (§6.2): the lowercase `x-tokentrimmer-*` family parsed by
 *   `parseCostInfo`. There is NO savings-pct header — `savingsPct` is
 *   computed here as `savedUsd / baselineCostUsd * 100`.
 */

/** One OpenAI-shaped chat message. `content` may be a string or an array of parts (§3.6). */
export interface ChatMessage {
	role: string;
	content?: unknown;
	[key: string]: unknown;
}

/** Per-call cost/savings parsed from the gateway's `x-tokentrimmer-*` response headers. */
export interface CostInfo {
	costUsd: number | null;
	baselineCostUsd: number | null;
	savedUsd: number | null;
	/** Computed: `savedUsd / baselineCostUsd * 100`; null unless both present and baseline > 0. */
	savingsPct: number | null;
	providerCacheSavedUsd: number | null;
	modelUsed: string | null;
	provider: string | null;
	cache: string | null;
	traceId: string | null;
}

const VALID_CACHE_OVERRIDES = new Set<string>(['bypass', 'force-write', 'read-only', 'disabled']);

/**
 * Normalize a credential base URL into a bare gateway origin.
 *
 * Trims whitespace, strips trailing slashes, and forgives the common
 * misconfiguration of pasting an SDK-style base URL by stripping ONE
 * trailing `/v1`. Throws on an empty result or a non-http(s) scheme.
 */
export function normalizeBaseUrl(raw: string): string {
	let url = raw.trim();
	url = url.replace(/\/+$/, '');
	if (url.toLowerCase().endsWith('/v1')) {
		url = url.slice(0, -'/v1'.length);
	}
	url = url.replace(/\/+$/, '');
	if (url === '') {
		throw new Error('Base URL must not be empty');
	}
	if (!/^https?:\/\/.+/i.test(url)) {
		throw new Error(
			`Base URL must start with http:// or https:// (got "${raw.trim()}"). ` +
				'Use the gateway origin without /v1, e.g. https://api.tokentrimmer.com or http://localhost:8080.',
		);
	}
	return url;
}

/** Full chat-completions endpoint for a credential base URL. */
export function chatCompletionsUrl(baseUrl: string): string {
	return `${normalizeBaseUrl(baseUrl)}/v1/chat/completions`;
}

/**
 * Build the OpenAI `messages` array from either a plain prompt
 * (+ optional system prompt) or raw messages JSON.
 *
 * `messagesJson` is usually a JSON string, but n8n hands json-type
 * parameters back as real values when a pure expression (e.g.
 * `={{ $json.messages }}`) resolves to an actual array — both shapes are
 * accepted and validated identically.
 */
export function buildMessages(input: {
	mode: 'prompt' | 'messagesJson';
	prompt?: string;
	systemPrompt?: string;
	messagesJson?: unknown;
}): ChatMessage[] {
	if (input.mode === 'prompt') {
		const prompt = input.prompt ?? '';
		if (prompt.trim() === '') {
			throw new Error('Prompt must not be empty');
		}
		const messages: ChatMessage[] = [];
		if (input.systemPrompt !== undefined && input.systemPrompt.trim() !== '') {
			messages.push({ role: 'system', content: input.systemPrompt });
		}
		messages.push({ role: 'user', content: prompt });
		return messages;
	}

	let parsed: unknown;
	if (typeof input.messagesJson === 'string') {
		try {
			parsed = JSON.parse(input.messagesJson);
		} catch (error) {
			throw new Error(
				`Messages must be valid JSON: ${error instanceof Error ? error.message : String(error)}`,
			);
		}
	} else {
		// An expression already resolved to a real value — validate as-is.
		parsed = input.messagesJson;
	}
	if (!Array.isArray(parsed)) {
		throw new Error('Messages must be a JSON array of {role, content} objects');
	}
	for (const [index, element] of parsed.entries()) {
		if (
			typeof element !== 'object' ||
			element === null ||
			typeof (element as ChatMessage).role !== 'string'
		) {
			throw new Error(`Message at index ${index} must be an object with a string "role"`);
		}
	}
	// Content is passed through untouched — string or array-of-parts are both legal (§3.6).
	return parsed as ChatMessage[];
}

/**
 * Build the TokenTrimmer request headers (§6.1). Keys are omitted for
 * absent options — empty headers are never sent.
 */
export function buildTtHeaders(opts: {
	tag?: string;
	costLimitUsd?: number;
	cacheOverride?: string;
}): Record<string, string> {
	const headers: Record<string, string> = {};
	if (opts.tag !== undefined && opts.tag !== '') {
		headers['X-TokenTrimmer-Tag'] = opts.tag;
	}
	if (opts.costLimitUsd !== undefined) {
		if (!Number.isFinite(opts.costLimitUsd) || opts.costLimitUsd < 0) {
			throw new Error(
				`Cost limit must be a finite number >= 0 (got ${String(opts.costLimitUsd)})`,
			);
		}
		headers['X-TokenTrimmer-Cost-Limit-Usd'] = String(opts.costLimitUsd);
	}
	if (opts.cacheOverride !== undefined && opts.cacheOverride !== '') {
		if (!VALID_CACHE_OVERRIDES.has(opts.cacheOverride)) {
			throw new Error(
				`Cache override must be one of bypass | force-write | read-only | disabled (got "${opts.cacheOverride}")`,
			);
		}
		headers['X-TokenTrimmer-Cache'] = opts.cacheOverride;
	}
	return headers;
}

/** Build the non-streaming chat-completions request body. Never sets `stream`. */
export function buildRequestBody(params: {
	model: string;
	messages: ChatMessage[];
	maxTokens?: number;
	temperature?: number;
}): object {
	const body: Record<string, unknown> = {
		model: params.model,
		messages: params.messages,
	};
	if (params.maxTokens !== undefined) {
		body.max_tokens = params.maxTokens;
	}
	if (params.temperature !== undefined) {
		body.temperature = params.temperature;
	}
	return body;
}

function headerString(value: unknown): string | null {
	const single = Array.isArray(value) ? value[0] : value;
	if (single === undefined || single === null) {
		return null;
	}
	const s = String(single);
	return s === '' ? null : s;
}

function parseFloatOrNull(s: string | null): number | null {
	if (s === null || s.trim() === '') {
		return null;
	}
	const n = Number(s);
	return Number.isFinite(n) ? n : null;
}

/**
 * Parse per-call cost/savings from the gateway's response headers (§6.2).
 *
 * Header keys are matched case-insensitively and string[] values are
 * tolerated (first element wins). Every field is nullable: cost headers
 * are absent on early-4xx responses.
 */
export function parseCostInfo(headers: Record<string, unknown>): CostInfo {
	const lower: Record<string, unknown> = {};
	for (const [key, value] of Object.entries(headers)) {
		lower[key.toLowerCase()] = value;
	}
	const str = (name: string): string | null => headerString(lower[name]);
	const num = (name: string): number | null => parseFloatOrNull(str(name));

	const costUsd = num('x-tokentrimmer-cost-usd');
	const baselineCostUsd = num('x-tokentrimmer-baseline-cost-usd');
	const savedUsd = num('x-tokentrimmer-saved-usd');
	const savingsPct =
		savedUsd !== null && baselineCostUsd !== null && baselineCostUsd > 0
			? (savedUsd / baselineCostUsd) * 100
			: null;

	return {
		costUsd,
		baselineCostUsd,
		savedUsd,
		savingsPct,
		providerCacheSavedUsd: num('x-tokentrimmer-provider-cache-saved-usd'),
		modelUsed: str('x-tokentrimmer-model-used'),
		provider: str('x-tokentrimmer-provider'),
		cache: str('x-tokentrimmer-cache'),
		traceId: str('x-tokentrimmer-trace-id'),
	};
}
