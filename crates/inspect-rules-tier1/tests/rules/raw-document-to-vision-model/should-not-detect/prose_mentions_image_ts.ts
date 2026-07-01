import OpenAI from "openai";

const client = new OpenAI();

// The prompt talks *about* images, but no image is actually sent to the model.
const r = await client.chat.completions.create({
  model: "gpt-4o",
  messages: [
    {
      role: "user",
      content:
        "Write alt text guidelines for our image gallery. Keep each description under 120 characters.",
    },
  ],
});

console.log(r);
