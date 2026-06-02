import OpenAI from "openai";
const client = new OpenAI();
const resp = await client.chat.completions.create({
  model: "gpt-4o",
  tools: [{ type: "function", function: { name: "f" } }],
  messages: [{ role: "user", content: "hi" }],
});
