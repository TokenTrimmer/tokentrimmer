const OpenAI = require("openai");

const client = new OpenAI();
client.chat.completions.create({
  model: "gpt-4-turbo-2024-04-09",
  messages: [{ role: "user", content: "test" }]
});
