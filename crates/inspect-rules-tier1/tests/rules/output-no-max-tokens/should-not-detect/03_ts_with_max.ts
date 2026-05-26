// Negative: TypeScript call WITH max_tokens.
import OpenAI from "openai";

const client = new OpenAI();
const response = await client.chat.completions.create({
  model: "gpt-4o",
  max_tokens: 256,
  messages: [{ role: "user", content: "Hello" }],
});
