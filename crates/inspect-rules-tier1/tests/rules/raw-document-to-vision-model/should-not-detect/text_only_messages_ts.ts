import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic();

const msg = await client.messages.create({
  model: "claude-3-5-sonnet",
  max_tokens: 512,
  messages: [{ role: "user", content: "Draft a polite follow-up email." }],
});

console.log(msg);
