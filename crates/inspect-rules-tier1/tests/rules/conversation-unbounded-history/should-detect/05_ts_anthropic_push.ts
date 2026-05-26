// Positive: Anthropic TypeScript with chat_history.push() and no pruning.
import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic();
const chat_history: Anthropic.MessageParam[] = [];

export async function converse(input: string): Promise<string> {
  chat_history.push({ role: "user", content: input });
  const response = await client.messages.create({
    model: "claude-3-5-sonnet-20241022",
    max_tokens: 1024,
    messages: chat_history,
  });
  const text = (response.content[0] as Anthropic.TextBlock).text;
  chat_history.push({ role: "assistant", content: text });
  return text;
}
