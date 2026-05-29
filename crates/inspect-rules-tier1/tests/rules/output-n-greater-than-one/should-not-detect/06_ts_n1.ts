import OpenAI from "openai";
const client = new OpenAI();
const resp = await client.chat.completions.create({
  model: "gpt-4o",
  n: 1,
  messages: [{ role: "user", content: "one" }],
});
