// Negative: OpenAI completion without a system message.
import OpenAI from "openai";

const client = new OpenAI();
const r = await client.chat.completions.create({
  model: "gpt-4o",
  messages: [{ role: "user", content: "Summarise this text." }],
  max_tokens: 500,
});
