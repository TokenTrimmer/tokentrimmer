import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic();
const message = await client.messages.create({
  model: "claude-3-5-haiku-20241022",
  max_tokens: 1024,
  messages: [{ role: "user", content: "Hello" }]
});
