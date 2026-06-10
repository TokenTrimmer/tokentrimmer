import { TokenTrimmer } from '@tokentrimmer/client';

const client = new TokenTrimmer({ apiKey: process.env.TOKENTRIMMER_API_KEY! });

const res = await client.chat.completions.create({
  model: 'claude-haiku-4-5',
  messages: [{ role: 'user', content: 'Say hello in five words.' }],
  max_tokens: 256,
  ttTag: 'example=cost-attribution',
  ttCostLimit: 0.05,
});

console.log(res.choices[0]?.message.content);
console.log(`cost     $${res.tt.costUsd}`);
console.log(`saved    $${res.tt.savedUsd}`);
console.log(`cache    ${res.tt.cache}`);
