import { TokenTrimmer } from '@tokentrimmer/client';

const client = new TokenTrimmer({
  apiKey: process.env.TOKENTRIMMER_API_KEY ?? 'tt_test_local',
  baseURL: process.env.TOKENTRIMMER_BASE_URL ?? 'http://localhost:8080/v1',
});

const res = await client.chat.completions.create({
  model: 'claude-haiku-4-5',
  messages: [{ role: 'user', content: 'Ping' }],
  max_tokens: 256,
  ttCache: 'bypass',
});

console.log(res.choices[0]?.message.content);
console.log(`cache ${res.tt.cache}`);
