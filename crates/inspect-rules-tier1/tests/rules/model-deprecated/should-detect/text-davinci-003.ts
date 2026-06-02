import OpenAI from "openai";

const client = new OpenAI();
const response = await client.completions.create({
  model: "text-davinci-003",
  prompt: "Say hello"
});
