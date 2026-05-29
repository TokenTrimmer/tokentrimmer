import OpenAI from "openai";
const client = new OpenAI();
const resp = await client.chat.completions.create({
  model: "gpt-4o",
  maxTokens: 256,
  messages: [{ role: "user", content: "no n" }],
});
