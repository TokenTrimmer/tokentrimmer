const OpenAI = require("openai");
const client = new OpenAI();
const resp = await client.chat.completions.create({
  model: "gpt-4o",
  n: 2,
  messages: [{ role: "user", content: "two" }],
});
