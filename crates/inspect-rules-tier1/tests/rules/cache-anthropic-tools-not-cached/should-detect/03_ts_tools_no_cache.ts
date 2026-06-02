import Anthropic from "@anthropic-ai/sdk";
const client = new Anthropic();
const resp = await client.messages.create({
  model: "claude-sonnet-4-6",
  max_tokens: 1024,
  tools: [{ name: "lookup", description: "Look things up", input_schema: { type: "object" } }],
  messages: [{ role: "user", content: "Look this up." }],
});
