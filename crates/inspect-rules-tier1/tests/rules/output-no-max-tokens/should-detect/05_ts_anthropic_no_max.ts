// Positive: Anthropic TypeScript without max_tokens.
import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic();
const msg = await client.messages.create({
  model: "claude-3-5-sonnet-20241022",
  messages: [{ role: "user", content: "Write a novel." }],
});
