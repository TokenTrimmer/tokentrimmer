const OpenAI = require("openai");

const client = new OpenAI();
client.chat.completions.create({
  model: "gpt-3.5-turbo-0301",
  messages: [{ role: "user", content: "test" }]
});
