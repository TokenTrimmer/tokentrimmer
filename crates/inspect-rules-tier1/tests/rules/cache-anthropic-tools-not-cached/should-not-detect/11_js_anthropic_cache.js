const Anthropic = require("@anthropic-ai/sdk");
const client = new Anthropic();
const resp = await client.messages.create({
  model: "claude-sonnet-4-6",
  max_tokens: 512,
  tools: [{ name: "t", description: "d", input_schema: {},
            cache_control: { type: "ephemeral" } }],
  messages: [{ role: "user", content: "x" }],
});
