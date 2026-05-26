// Negative: TypeScript Anthropic WITH max_tokens.
import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic();
const r = await client.messages.create({
  model: "claude-3-5-sonnet-20241022",
  max_tokens: 500,
  messages: [{ role: "user", content: "What is the capital of France?" }],
});
