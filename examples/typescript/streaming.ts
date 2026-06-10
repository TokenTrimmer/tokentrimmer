import { TokenTrimmer } from '@tokentrimmer/client';

const client = new TokenTrimmer({ apiKey: process.env.TOKENTRIMMER_API_KEY! });

const stream = await client.chat.completions.create({
  model: 'claude-haiku-4-5',
  messages: [{ role: 'user', content: 'Count to five.' }],
  max_tokens: 256,
  stream: true,
  stream_options: { include_usage: true },
});

for await (const chunk of stream) {
  const delta = chunk.choices[0]?.delta?.content;
  if (delta) process.stdout.write(delta);
  if (chunk.usage) console.log(`\nusage:`, chunk.usage);
}

// Per-request cost is on `stream.tt` once the stream is fully drained.
console.log(`\ncost  $${stream.tt?.costUsd.toFixed(4)}`);
console.log(`saved $${stream.tt?.savedUsd.toFixed(4)}`);
