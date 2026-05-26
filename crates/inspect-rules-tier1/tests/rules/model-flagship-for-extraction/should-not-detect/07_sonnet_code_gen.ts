// Negative: Claude Sonnet for code generation — not a data task.
import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic();
await client.messages.create({
  model: "claude-3-5-sonnet-20241022",
  max_tokens: 2048,
  messages: [{ role: "user", content: "Write a REST API in Express.js." }],
});
