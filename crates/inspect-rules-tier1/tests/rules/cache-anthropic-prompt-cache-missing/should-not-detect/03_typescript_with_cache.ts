// Negative: long system in TS but cache_control is present.
import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic();
const LONG = "a".repeat(5000);

const response = await client.messages.create({
  model: "claude-3-5-sonnet-20241022",
  max_tokens: 1024,
  system: [
    {
      type: "text",
      text: LONG,
      cache_control: { type: "ephemeral" },
    },
  ],
  messages: [{ role: "user", content: "Query" }],
});
