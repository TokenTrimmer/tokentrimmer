// Negative: TypeScript with splice pruning.
import OpenAI from "openai";

const client = new OpenAI();
const messages: any[] = [];

async function chat(input: string) {
  messages.push({ role: "user", content: input });
  // Keep only last 30 messages
  if (messages.length > 30) {
    messages.splice(0, messages.length - 30);
  }
  return client.chat.completions.create({ model: "gpt-4o", messages });
}
