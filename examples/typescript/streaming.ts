import { TokenTrimmer } from '@tokentrimmer/client';
import type { Stream } from 'openai/streaming';
import type { ChatCompletionChunk } from 'openai/resources';

const client = new TokenTrimmer({ apiKey: process.env.TOKENTRIMMER_API_KEY! });

const stream = (await client.chat.completions.create({
  model: 'claude-haiku-4-5',
  messages: [{ role: 'user', content: 'Count to five.' }],
  stream: true,
  stream_options: { include_usage: true },
} as never)) as unknown as Stream<ChatCompletionChunk>;

for await (const chunk of stream) {
  const delta = chunk.choices[0]?.delta?.content;
  if (delta) process.stdout.write(delta);
  if (chunk.usage) console.log(`\nusage:`, chunk.usage);
}
