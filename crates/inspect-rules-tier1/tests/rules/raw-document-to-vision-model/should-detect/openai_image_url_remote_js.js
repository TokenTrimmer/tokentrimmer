const OpenAI = require("openai");

const client = new OpenAI();

async function run() {
  const res = await client.chat.completions.create({
    model: "gpt-4o-mini",
    messages: [
      {
        role: "user",
        content: [
          { type: "text", text: "Read the total from this invoice." },
          {
            type: "image_url",
            image_url: { url: "https://cdn.example.com/invoices/2026-06.png" },
          },
        ],
      },
    ],
  });
  return res;
}

module.exports = { run };
