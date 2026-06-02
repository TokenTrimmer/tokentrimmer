import OpenAI from "openai";
const client = new OpenAI();
const resp = await client.chat.completions.create({
  model: "o3-mini",
  messages: [{ role: "user", content: "reason" }],
});
