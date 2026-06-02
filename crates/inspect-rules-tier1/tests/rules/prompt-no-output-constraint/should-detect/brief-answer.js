const OpenAI = require("openai");

const client = new OpenAI();
client.chat.completions.create({
  model: "gpt-4",
  messages: [
    {
      role: "user",
      content: "Concise answer: What is the capital of France?"
    }
  ]
});
