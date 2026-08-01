# @tokentrimmer/client

Thin Node SDK over the official OpenAI client — routes through the TokenTrimmer Gateway, surfaces cost + cache metadata.

> **Not yet on npm** — published packages land at launch. Until then (npm cannot
> install a git subdirectory directly), build from a clone and install the local path:

```bash
git clone https://github.com/TokenTrimmer/tokentrimmer.git
(cd tokentrimmer/sdk-typescript && npm install && npm run build)
npm install ./tokentrimmer/sdk-typescript openai
```

> **`openai` is a peer dependency** — you install it alongside this SDK (not bundled
> inside it). This avoids having two separate copies of the `openai` client in your
> dependency tree when your app already depends on `openai` directly, which would
> break subclassing and cause subtle type mismatches.
> Once on npm, install both together: `npm i @tokentrimmer/client openai`.

## Try it in 30 seconds — no account, no provider key, $0

A `tt_test_*` **sandbox key** short-circuits inside the Gateway to a deterministic
synthetic response: it never contacts a provider, never verifies against a key
store, and costs nothing — ideal for wiring up an integration before you have an
account. Start a local Gateway (one `docker run`, no provider keys), then call it:

```bash
docker run -p 8080:8080 \
  -e TT_BIND_ADDR=0.0.0.0 -e TT_ALLOW_UNAUTHENTICATED_PUBLIC_BIND=1 \
  ghcr.io/tokentrimmer/tt-cli:latest
```

```ts
import { TokenTrimmer } from '@tokentrimmer/client';

// Sandbox: any tt_test_ token works — no account, no provider key, $0.
const client = new TokenTrimmer({ apiKey: 'tt_test_demo', baseURL: 'http://localhost:8080/v1' });

const response = await client.chat.completions.create({
  model: 'claude-sonnet-4-6',
  messages: [{ role: 'user', content: 'Hello' }],
});
console.log(response.choices[0].message.content);
// → [sandbox] TokenTrimmer test response for model=claude-sonnet-4-6
console.log(`cost $${response.tt.costUsd?.toFixed(4)}  cache ${response.tt.cache}`);
// → cost $0.0000  cache sandbox   (no provider was called)
```

## Real usage

Point at a live Gateway with a verified `tt_live_*` key for real routing, cost, and
cache metadata.

> **Hosted gateway launching soon** *(as of 2026-06-10)* — `new TokenTrimmer({ apiKey })`
> defaults to `https://api.tokentrimmer.com`, which is not live yet. Self-host with
> Docker today and pass `baseURL: 'http://localhost:8080/v1'` (see "Self-hosted
> Gateway" below).

```ts
import { TokenTrimmer } from '@tokentrimmer/client';

const client = new TokenTrimmer({ apiKey: 'tt_live_...' });

const response = await client.chat.completions.create({
  model: 'claude-sonnet-4-6',                      // any model your Gateway routes
  messages: [{ role: 'user', content: 'Hello' }],
  ttTag: 'feature=chat-support',                    // optional: cost attribution
});

console.log(response.choices[0].message.content);

// Cost + cache metadata is on `.tt`:
console.log(`cost  $${response.tt.costUsd?.toFixed(4)}`);
console.log(`saved $${response.tt.savedUsd?.toFixed(4)}`);
console.log(`cache ${response.tt.cache}`);
console.log(`trace ${response.tt.traceId}`);
```

The class is an `openai.OpenAI` subclass — inherited methods (`embeddings`,
`models`, tools, vision) work unchanged. For TokenTrimmer's responder-scoped,
runtime-validated metadata extensions, use `client.gateway`:

```ts
// Anonymous catalog metadata. No configured bearer is sent.
const catalog = await client.gateway.models();
console.log(catalog.data[0]?.tokentrimmer.max_input_tokens);

// Requires a tt_live_* key; evidence from one responding gateway process.
const capabilities = await client.gateway.capabilities();
console.log(capabilities.features.fusion.limits.member_models_max.value);

// Local responder preflight only: no provider request, tokenization, or
// credential-validity/readiness claim.
const preflight = await client.gateway.preflight({
  schema_version: 1,
  model: 'gpt-4o-mini',
  provider: null,
  required_capabilities: ['text', 'tools', 'streaming'],
  declared_input_tokens: 12_000,
  requested_max_output_tokens: 4_096,
});
console.log(preflight.actions);
console.log(preflight.catalog_cost);

// One responder and generated-at marker for 1–9 ordered declarations.
const batch = await client.gateway.preflightBatch({
  schema_version: 1,
  requests: [preflight.request],
});
console.log(batch.documents);
```

These operations use Rust-generated wire types, no automatic redirects, one
five-second timeout, streamed body ceilings (256 KiB models / 64 KiB
capabilities and preflight), strict no-store/nosniff and semantic validation,
and model snapshot-digest recomputation. They do not prove credentials,
provider health, model/modality readiness, live pricing, request acceptance,
fleet convergence, a quote/reservation, enforced budget, settlement, invoice,
or later execution. Successful reason messages are bounded responder copy; use
the stable reason codes and actual request result for machine decisions. The
batch removes cross-process drift, but composite stores and runtime
configuration are still not one transaction.

### `openai` version compatibility

`openai` is a **peer dependency** (`^6.45.0`). Install it alongside this package —
your copy is the one that gets used, with no risk of a duplicate instance in the tree.
Because `TokenTrimmer` subclasses `openai.OpenAI`, sharing a single copy is required
for correct subclassing and TypeScript types. The package is developed and tested
against the `^6` line; `^5` is not supported.

### Streaming

Streaming works as usual; per-request cost is on the stream's `.tt` once it's drained (the Gateway's terminal usage frame is stripped, so chunk iteration is clean):

```ts
const stream = await client.chat.completions.create({
  model: 'claude-sonnet-4-6',
  messages: [{ role: 'user', content: 'Hello' }],
  stream: true,
});

for await (const chunk of stream) {
  process.stdout.write(chunk.choices[0]?.delta?.content ?? '');
}

// Cost is known once the stream is fully consumed:
console.log(`\ncost  $${stream.tt?.costUsd.toFixed(4)}`);
console.log(`saved $${stream.tt?.savedUsd.toFixed(4)}`);
```

### Agent loop

For multi-step tool-using runs, `client.agent.run(...)` drives the Gateway's
server-side agent loop (`POST /v1/agent/runs`). The Gateway owns the loop
(down-routing, judge-gated summarize, substep cache); the SDK just executes any
**client** tool the run pauses on (via your `executor`) and resumes — until a
final answer. Aggregate cost spans every turn and is read from the run body
(`outcome.usage.costUsd`), not response headers.

```ts
const outcome = await client.agent.run({
  model: 'claude-sonnet-4-6',
  messages: [{ role: 'user', content: "What's the weather in Paris?" }],
  tools: [
    {
      type: 'function',
      function: {
        name: 'get_weather',
        description: 'Current weather for a city',
        parameters: { type: 'object', properties: { city: { type: 'string' } } },
      },
    },
  ],
  // `args` is the raw JSON string the model produced; return the tool result as a
  // string (sync or async). Throwing is fine — the error is fed back to the model.
  executor: async (name, args) => {
    if (name === 'get_weather') return JSON.stringify({ temp_c: 21, sky: 'clear' });
    return '{}';
  },
  maxTurns: 8,          // optional: server-side per-run turn cap
  ttTag: 'feature=agent',
});

console.log(outcome.text);                          // final assistant answer
console.log(`cost   $${outcome.usage.costUsd.toFixed(4)}`);
console.log(`rounds ${outcome.resumeRounds}`);      // client-side tool_outputs resumes made
```

Paused/resumed transcripts remain in Redis for one hour. You can explicitly
export or erase that short-lived resume state without deleting durable
billing/audit metadata:

```ts
const transcript = await client.agent.exportTranscript(outcome.run.id);
await client.agent.deleteTranscript(outcome.run.id); // idempotent
```

### Batch (50% cheaper, async)

The Gateway's `/v1/files` + `/v1/batches` endpoints are OpenAI-compatible, so the
**inherited** OpenAI `files` / `batches` resources route through TokenTrimmer
unchanged — no special methods, just the standard OpenAI batch flow. Provider
batch jobs are ~50% cheaper than synchronous calls, and TokenTrimmer's poll
worker books the realized savings server-side as each batch settles (visible in
your dashboard).

```ts
import { createReadStream } from 'node:fs';
import { setTimeout as sleep } from 'node:timers/promises';

const client = new TokenTrimmer({ apiKey: 'tt_live_...' });

// 1. Upload a JSONL of requests (one chat-completion request per line).
const file = await client.files.create({
  file: createReadStream('requests.jsonl'),
  purpose: 'batch',
});

// 2. Create the batch.
let batch = await client.batches.create({
  input_file_id: file.id,
  endpoint: '/v1/chat/completions',
  completion_window: '24h',
});

// 3. Poll until it settles (the Gateway poll worker drives status + savings).
const TERMINAL = new Set(['completed', 'failed', 'expired', 'cancelled']);
while (!TERMINAL.has(batch.status)) {
  await sleep(30_000);
  batch = await client.batches.retrieve(batch.id);
}

// 4. Download the results JSONL.
if (batch.status === 'completed' && batch.output_file_id) {
  const results = await client.files.content(batch.output_file_id);
  console.log(await results.text());
}
```

Prefer no code? The [`tt` CLI](https://github.com/TokenTrimmer/tokentrimmer)
wraps the same flow: `tt batch submit requests.jsonl`, `tt batch get <id>`,
`tt batch download <output_file_id>`.

## Framework integrations: cost/savings into OTel spans

Point the Vercel AI SDK (`ai`) at the Gateway with a
`baseURL` swap and the `x-tokentrimmer-*` cost/savings headers ride back on
`result.response.headers` — but they're invisible to the observability you
already watch. The optional `@tokentrimmer/client/vercel` adapter recovers them
and records them as OpenTelemetry span attributes using a shared semantic-
convention vocabulary (`gen_ai.*` + `tokentrimmer.{cost_usd,saved_usd,cache,route,…}`)
that is **identical** across the gateway span, this TS adapter, and the Python
SDK — so one dashboard query resolves cost end to end, in any language.

```ts
import { openai } from '@ai-sdk/openai';
import { generateText } from 'ai';
import { TokenTrimmerRunCost } from '@tokentrimmer/client/vercel';

const gateway = openai.provider({ baseURL: 'https://api.tokentrimmer.com/v1', apiKey: 'tt_live_...' });
const run = new TokenTrimmerRunCost({ maxCostUsd: 0.5 }); // optional per-run budget

const result = await generateText({
  model: gateway('claude-haiku-4-5'),
  prompt: 'Hello',
  experimental_telemetry: { isEnabled: true }, // AI SDK records the active span
});
await run.record(result); // reads TT headers, records span attrs, accumulates totals

console.log(`run cost $${run.totalCostUsd} · saved $${run.totalSavedUsd}`);
```

A run whose accumulated cost exceeds `maxCostUsd` throws `BudgetExceededError`
(a framework-level stop). For one-off recording use `recordTokenTrimmerCost(result, { span })`.
A non-gateway response (no `x-tokentrimmer-*` headers) degrades quietly: nothing
is recorded and nothing is thrown.

`ai` and `@opentelemetry/api` are **optional** peer dependencies — install them
only if you use this adapter. The base `import { TokenTrimmer } from '@tokentrimmer/client'`
never depends on them. The raw semconv constants + `costInfoToAttributes()` are
also exposed dependency-free at `@tokentrimmer/client/semconv`.

## Self-hosted Gateway

```ts
const client = new TokenTrimmer({
  apiKey: 'sk-...',                              // your provider key, pass-through
  baseURL: 'http://localhost:8080/v1',           // your self-hosted Gateway
});
```

## Releasing

Maintainers: publishing to npm is tag-triggered and documented in
[`RELEASING.md`](RELEASING.md).

## License

Apache 2.0.
