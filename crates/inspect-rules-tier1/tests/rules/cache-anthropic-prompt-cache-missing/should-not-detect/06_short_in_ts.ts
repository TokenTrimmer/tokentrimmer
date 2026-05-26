// Negative: short system prompt in TypeScript.
import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic();
const response = await client.messages.create({
  model: "claude-3-haiku-20240307",
  max_tokens: 256,
  system: "You are a helpful assistant.",
  messages: [{ role: "user", content: "What is 2+2?" }],
});
