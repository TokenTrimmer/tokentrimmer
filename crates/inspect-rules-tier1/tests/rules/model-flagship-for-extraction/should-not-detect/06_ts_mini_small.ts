// Negative: gpt-4o-mini for identifying names — small model, appropriate.
import OpenAI from "openai";

const client = new OpenAI();
const r = await client.chat.completions.create({
  model: "gpt-4o-mini",
  messages: [{ role: "user", content: "Who are the people mentioned here? Alice, Bob, Carol" }],
});
