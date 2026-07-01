/**
 * OpenTelemetry semantic-convention constants for TokenTrimmer cost/savings.
 *
 * The gateway already stamps these exact attribute keys onto its own request
 * spans (see the Rust `tt_telemetry::gen_ai` module,
 * `crates/telemetry/src/gen_ai.rs`). This module mirrors that vocabulary on the
 * client side — and is a **byte-for-byte** port of the Python SDK's
 * `tokentrimmer/semconv.py` (shipped in #268) — so a span recorded by a
 * framework integration (e.g. `./vercel`, the Vercel AI SDK adapter) uses
 * **identical** keys to the gateway span AND to the Python LangChain callback. A
 * distributed trace then carries one consistent set of `gen_ai.*` /
 * `tokentrimmer.*` attributes end to end, across languages.
 *
 * This module has **no third-party dependencies** — it is plain string constants
 * plus a pure `TokenTrimmerMeta -> attributes` mapping. It is therefore always
 * importable, even when neither the optional `ai` (Vercel AI SDK) nor
 * `@opentelemetry/api` peer dependency is installed. Recording the returned
 * object onto a live span is the integration layer's job (`./vercel`), and the
 * only place OpenTelemetry is actually needed.
 *
 * ## Attribute vocabulary
 *
 * `gen_ai.*` keys follow the OpenTelemetry
 * [GenAI semantic conventions](https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-spans/):
 *
 * - `gen_ai.system` — the GenAI provider (`openai`, `anthropic`, …).
 * - `gen_ai.provider.name` — the newer semconv spelling of `gen_ai.system`; we
 *   emit **both** so a dashboard keyed on either resolves.
 * - `gen_ai.operation.name` — the operation (`chat`, `embeddings`).
 * - `gen_ai.request.model` — the model the caller asked for.
 * - `gen_ai.response.model` — the model that actually served the request
 *   (differs from the request model after routing / cross-model failover).
 * - `gen_ai.usage.input_tokens` / `gen_ai.usage.output_tokens` — token counts.
 *
 * `tokentrimmer.*` keys mirror the `x-tokentrimmer-*` response headers and are
 * TokenTrimmer-specific (not part of the upstream semconv):
 *
 * - `tokentrimmer.cost_usd` — what the provider actually bills.
 * - `tokentrimmer.baseline_cost_usd` — cost with no TokenTrimmer optimisation.
 * - `tokentrimmer.saved_usd` — TokenTrimmer-attributed savings (the headline).
 * - `tokentrimmer.cache` — cache outcome (`hit-l1`, `hit-l2`, `miss`, …).
 * - `tokentrimmer.route` — the matched route name, when routing applied.
 * - `tokentrimmer.trace_id` — the gateway trace id (`x-tokentrimmer-trace-id`),
 *   a client-side-only correlation key back to the gateway's own span.
 *
 * @module
 */

// Type-only import: erased at compile time, so this module keeps its zero
// runtime dependencies (it never pulls in `openai` via `./index`).
import type { TokenTrimmerMeta } from './index.js';

// --- gen_ai.* (OpenTelemetry GenAI semantic conventions) --------------------

/** `gen_ai.system` — the GenAI provider identifier. */
export const GEN_AI_SYSTEM = 'gen_ai.system';
/** `gen_ai.provider.name` — the newer semconv spelling of {@link GEN_AI_SYSTEM}. */
export const GEN_AI_PROVIDER_NAME = 'gen_ai.provider.name';
/** `gen_ai.operation.name` — the GenAI operation being performed. */
export const GEN_AI_OPERATION_NAME = 'gen_ai.operation.name';
/** `gen_ai.request.model` — the model the request was made to. */
export const GEN_AI_REQUEST_MODEL = 'gen_ai.request.model';
/** `gen_ai.response.model` — the model that generated the response. */
export const GEN_AI_RESPONSE_MODEL = 'gen_ai.response.model';
/** `gen_ai.usage.input_tokens` — prompt tokens. */
export const GEN_AI_USAGE_INPUT_TOKENS = 'gen_ai.usage.input_tokens';
/** `gen_ai.usage.output_tokens` — completion tokens. */
export const GEN_AI_USAGE_OUTPUT_TOKENS = 'gen_ai.usage.output_tokens';

// --- tokentrimmer.* (TokenTrimmer-specific cost/savings) --------------------

/** `tokentrimmer.cost_usd` — what the provider actually bills (USD). */
export const TT_COST_USD = 'tokentrimmer.cost_usd';
/** `tokentrimmer.baseline_cost_usd` — cost without TokenTrimmer (USD). */
export const TT_BASELINE_COST_USD = 'tokentrimmer.baseline_cost_usd';
/** `tokentrimmer.saved_usd` — TokenTrimmer-attributed savings (USD). */
export const TT_SAVED_USD = 'tokentrimmer.saved_usd';
/** `tokentrimmer.cache` — cache outcome (`hit-l1`, `hit-l2`, `miss`, …). */
export const TT_CACHE = 'tokentrimmer.cache';
/** `tokentrimmer.route` — the matched route name (when routing applied). */
export const TT_ROUTE = 'tokentrimmer.route';
/**
 * `tokentrimmer.trace_id` — the gateway trace id, for correlation back to the
 * gateway's own span. Client-side-only (the gateway span already carries its own
 * OTel trace/span ids, so it does not re-emit this attribute).
 */
export const TT_TRACE_ID = 'tokentrimmer.trace_id';

/** OTel attribute value types produced by this module (string or number). */
export type AttributeValue = string | number;
/** A span-attribute object ready for `span.setAttributes(...)`. */
export type Attributes = Record<string, AttributeValue>;

/** Well-known `gen_ai.system` values keyed by TokenTrimmer provider id. */
const GEN_AI_SYSTEM_MAP: Readonly<Record<string, string>> = {
  openai: 'openai',
  anthropic: 'anthropic',
  // The semconv well-known value for Google Gemini is `gcp.gemini`.
  gemini: 'gcp.gemini',
  groq: 'groq',
  mistral: 'mistral_ai',
};

/**
 * Map a TokenTrimmer provider id to the OTel `gen_ai.system` value.
 *
 * Mirrors the gateway's Rust `gen_ai_system` mapping (and the Python SDK's
 * `gen_ai_system`) so the client and gateway agree on the well-known enum value.
 * The semconv defines fixed values for common providers; unregistered ids
 * (aggregators, OpenAI-compat shims, local runtimes, the synthetic
 * `cache`/`sandbox` pseudo-providers on cache hits) pass through verbatim — the
 * spec permits custom values and a stable string is more useful in a dashboard
 * than a dropped attribute.
 */
export function genAiSystem(providerId: string): string {
  return GEN_AI_SYSTEM_MAP[providerId] ?? providerId;
}

/**
 * Map a parsed {@link TokenTrimmerMeta} to a span-attribute object.
 *
 * The returned object uses the semconv keys defined in this module and is ready
 * to hand to `span.setAttributes(...)`. Only fields the gateway actually
 * populated (non-`null` on `meta`) appear — the mapping is **additive**,
 * mirroring the gateway span's own behaviour and the Python
 * `cost_info_to_attributes`, so a self-hosted deployment that emits no cost
 * headers yields an empty object rather than a span littered with nulls.
 *
 * Note that token counts (`gen_ai.usage.*`) and the requested model
 * (`gen_ai.request.model`) are **not** carried on the cost headers, so they are
 * not produced here; an integration that has them from the framework's own
 * result object (e.g. the Vercel adapter's `result.usage`) records them
 * separately.
 */
export function costInfoToAttributes(meta: TokenTrimmerMeta): Attributes {
  const attrs: Attributes = {};

  if (meta.provider != null) {
    const system = genAiSystem(meta.provider);
    attrs[GEN_AI_SYSTEM] = system;
    // Emit the newer semconv spelling too so either-keyed dashboards resolve.
    attrs[GEN_AI_PROVIDER_NAME] = system;
  }
  if (meta.modelUsed != null) attrs[GEN_AI_RESPONSE_MODEL] = meta.modelUsed;

  if (meta.costUsd != null) attrs[TT_COST_USD] = meta.costUsd;
  if (meta.baselineCostUsd != null) attrs[TT_BASELINE_COST_USD] = meta.baselineCostUsd;
  if (meta.savedUsd != null) attrs[TT_SAVED_USD] = meta.savedUsd;
  if (meta.cache != null) attrs[TT_CACHE] = meta.cache;
  if (meta.route != null) attrs[TT_ROUTE] = meta.route;
  if (meta.traceId != null) attrs[TT_TRACE_ID] = meta.traceId;

  return attrs;
}
