import OpenAI from "openai";
const client = new OpenAI();
const resp = await client.chat.completions.create({
  model: "o3-mini",
  reasoning_effort: "low",
  messages: [{ role: "user", content: "reason cheaply" }],
});
