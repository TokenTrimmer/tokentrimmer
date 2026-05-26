// Positive: TypeScript conversation with messages.push() — no pruning.
import OpenAI from "openai";

const client = new OpenAI();
const messages: OpenAI.Chat.ChatCompletionMessageParam[] = [];

async function chat(userInput: string): Promise<string> {
  messages.push({ role: "user", content: userInput });
  const response = await client.chat.completions.create({
    model: "gpt-4o",
    messages,
  });
  const reply = response.choices[0].message.content ?? "";
  messages.push({ role: "assistant", content: reply });
  return reply;
}
