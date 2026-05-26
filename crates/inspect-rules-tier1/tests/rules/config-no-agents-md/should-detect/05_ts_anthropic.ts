// Positive: TypeScript Anthropic import without agents file.
import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic();
const r = await client.messages.create({
  model: "claude-3-5-sonnet-20241022",
  max_tokens: 256,
  messages: [{ role: "user", content: "Hi" }],
});
