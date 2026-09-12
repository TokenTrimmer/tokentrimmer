// Compile only: resolves declarations from the installed tarball, not src/.
import OpenAI from 'openai';
import { APIPromise } from 'openai/core/api-promise';
import { TokenTrimmer, type ChatCompletionWithMeta, type TokenTrimmerStream } from '@tokentrimmer/client';

const client = new TokenTrimmer({ apiKey: 'tt_test_local' });
const base: OpenAI = client;
void base;
const pending: APIPromise<ChatCompletionWithMeta> = client.chat.completions.create({ model: 'test-model', messages: [], ttTag: 'test' });
const raw: Promise<Response> = pending.asResponse();
const data: ChatCompletionWithMeta = (await pending.withResponse()).data;
const stream: TokenTrimmerStream = (await client.chat.completions.create({ model: 'test-model', messages: [], stream: true, ttCostLimit: 0.1 }).withResponse()).data;
const flag: boolean = Boolean(0);
const variable: APIPromise<ChatCompletionWithMeta | TokenTrimmerStream> = client.chat.completions.create({ model: 'test-model', messages: [], stream: flag, ttCache: 'disabled' });
void [raw, data.tt, stream.tt, variable];
