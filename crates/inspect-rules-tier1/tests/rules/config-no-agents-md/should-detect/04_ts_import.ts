// Positive: TypeScript OpenAI import without agents file.
import OpenAI from "openai";

const client = new OpenAI();
const r = await client.chat.completions.create({
  model: "gpt-4o",
  max_tokens: 200,
  messages: [{ role: "user", content: "Hello" }],
});
