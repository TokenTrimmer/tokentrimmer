const OpenAI = require("openai");
const client = new OpenAI();

async function reply(userText) {
  const resp = await client.chat.completions.create({
    model: "gpt-4o",
    max_tokens: 400,
    messages: [
      { role: "system", content: "You are a helpful support agent." },
      { role: "user", content: userText },
    ],
  });
  return resp.choices[0].message.content;
}
module.exports = { reply };
