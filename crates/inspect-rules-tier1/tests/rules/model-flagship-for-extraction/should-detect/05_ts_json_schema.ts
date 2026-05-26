// Positive: GPT-4o for JSON schema based structured output.
import OpenAI from "openai";

const openai = new OpenAI();

async function parseDocument(text: string) {
  return openai.chat.completions.create({
    model: "gpt-4o",
    messages: [
      { role: "system", content: "Extract fields according to json schema." },
      { role: "user", content: text },
    ],
  });
}
