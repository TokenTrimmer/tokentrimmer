import OpenAI from "openai";
const client = new OpenAI();

async function run() {
  const r = await client.chat.completions.create({
    model: "gpt-4o",
    messages: [{ role: "user", content: `{"a":1,"b":2,"c":3}` }],
  });
  return r;
}
