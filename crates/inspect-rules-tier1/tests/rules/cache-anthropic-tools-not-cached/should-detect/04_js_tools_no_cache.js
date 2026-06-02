const Anthropic = require("@anthropic-ai/sdk");
const client = new Anthropic();
const resp = await client.messages.create({
  model: "claude-sonnet-4-6",
  max_tokens: 1024,
  tools: [{ name: "fetch_data", description: "Fetch data", input_schema: { type: "object" } }],
  messages: [{ role: "user", content: "Fetch it." }],
});
