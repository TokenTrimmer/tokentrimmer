import { TokenTrimmer, type WithTokenTrimmerMeta } from '@tokentrimmer/client';
import type { ChatCompletion } from 'openai/resources';

const client = new TokenTrimmer({ apiKey: process.env.TOKENTRIMMER_API_KEY! });

const res = (await client.chat.completions.create({
  model: 'claude-haiku-4-5',
  messages: [{ role: 'user', content: 'Say hello in five words.' }],
  ttTag: 'example=cost-attribution',
  ttCostLimit: 0.05,
} as never)) as WithTokenTrimmerMeta<ChatCompletion>;

console.log(res.choices[0]?.message.content);
console.log(`cost     $${res.tt.costUsd}`);
console.log(`saved    $${res.tt.savedUsd}`);
console.log(`cache    ${res.tt.cache}`);
