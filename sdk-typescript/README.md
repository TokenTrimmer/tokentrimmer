# @tokentrimmer/client

Thin Node SDK over the official OpenAI client — routes through the TokenTrimmer Gateway, surfaces cost + cache metadata.

```bash
npm install @tokentrimmer/client
```

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

The class is an `openai.OpenAI` subclass — every other method (`embeddings`, `models`, streaming, tools, vision) works unchanged.

## Self-hosted Gateway

```ts
const client = new TokenTrimmer({
  apiKey: 'sk-...',                              // your provider key, pass-through
  baseURL: 'http://localhost:8080/v1',           // your self-hosted Gateway
});
```

## License

Apache 2.0.
