// Negative: only OpenAI, not Anthropic.
import OpenAI from "openai";

const client = new OpenAI();
const r = await client.chat.completions.create({
  model: "gpt-4o",
  messages: [{ role: "user", content: "Test" }],
});
