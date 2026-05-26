// Negative: gpt-4o-mini for categorisation — small model, no issue.
import OpenAI from "openai";

const client = new OpenAI();
const r = await client.chat.completions.create({
  model: "gpt-4o-mini",
  messages: [{ role: "user", content: "categorize this: finance news" }],
});
