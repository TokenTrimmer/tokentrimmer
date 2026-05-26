// Negative: cache_control is present in TypeScript.
import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic();
const r = await client.messages.create({
  model: "claude-3-5-sonnet-20241022",
  max_tokens: 512,
  system: [{ type: "text", text: "You are helpful.", cache_control: { type: "ephemeral" } }],
  messages: [{ role: "user", content: "Hi" }],
});
