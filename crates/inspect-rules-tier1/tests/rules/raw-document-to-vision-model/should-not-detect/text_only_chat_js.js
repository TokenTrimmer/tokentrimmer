const OpenAI = require("openai");

const client = new OpenAI();

async function ask(question) {
  const res = await client.chat.completions.create({
    model: "gpt-4o-mini",
    messages: [{ role: "user", content: question }],
  });
  return res.choices[0].message.content;
}

module.exports = { ask };
