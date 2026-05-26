// Negative: uses maxTokens (camelCase variant) — covered by pattern.
import { Anthropic } from "@anthropic-ai/sdk";

const client = new Anthropic();
const response = await client.messages.create({
  model: "claude-3-5-sonnet-20241022",
  maxTokens: 256,
  messages: [{ role: "user", content: "Hi" }],
});
