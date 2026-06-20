# @tokentrimmer/client

Thin Node SDK over the official OpenAI client — routes through the TokenTrimmer Gateway, surfaces cost + cache metadata.

> **Not yet on npm** — published packages land at launch. Until then (npm cannot
> install a git subdirectory directly), build from a clone and install the local path:

```bash
git clone https://github.com/TokenTrimmer/tokentrimmer.git
(cd tokentrimmer/sdk-typescript && npm install && npm run build)
npm install ./tokentrimmer/sdk-typescript
```

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

The class is an `openai.OpenAI` subclass — every other method (`embeddings`, `models`, tools, vision) works unchanged.

### `openai` version compatibility

The wrapper depends on `openai@"^5.0.0 || ^6.0.0"` and its full test suite passes
against both majors (CI locks and tests the v6 line). Because the class subclasses
`openai.OpenAI`, your app's `openai` version is effectively coupled to this range:
either major dedupes cleanly with an app that pins `^5` or `^6`. If you pin `openai`
yourself, prefer `^6` — that is the resolution this package is locked and tested
against day-to-day.

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
