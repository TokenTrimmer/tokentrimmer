// Negative: TypeScript OpenAI call with short system.
import OpenAI from "openai";

const client = new OpenAI();
const response = await client.chat.completions.create({
  model: "gpt-4o",
  messages: [
    { role: "system", content: "You are helpful." },
    { role: "user", content: "Hi there" },
  ],
});
