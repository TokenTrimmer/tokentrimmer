// Positive: TypeScript OpenAI call without max_tokens.
import OpenAI from "openai";

const client = new OpenAI();
const response = await client.chat.completions.create({
  model: "gpt-4o",
  messages: [{ role: "user", content: "Describe the history of computing." }],
});
