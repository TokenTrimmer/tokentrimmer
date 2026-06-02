const OpenAI = require("openai");
const client = new OpenAI();
const resp = await client.chat.completions.create({
  model: "o1",
  messages: [{ role: "user", content: "think" }],
});
