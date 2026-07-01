import OpenAI from "openai";

const client = new OpenAI();

const r = await client.responses.create({
  model: "gpt-4o",
  input: [
    {
      role: "user",
      content: [
        { type: "input_text", text: "Describe this diagram." },
        {
          type: "input_image",
          image_url: "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAA",
        },
      ],
    },
  ],
});

console.log(r);
