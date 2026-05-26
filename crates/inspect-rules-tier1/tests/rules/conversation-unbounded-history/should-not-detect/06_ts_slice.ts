// Negative: TypeScript with .slice(-N) for windowing.
import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic();
const msgs: Anthropic.MessageParam[] = [];

async function chat(text: string) {
  msgs.push({ role: "user", content: text });
  const windowed = msgs.slice(-10);
  const r = await client.messages.create({
    model: "claude-3-5-sonnet-20241022",
    max_tokens: 500,
    messages: windowed,
  });
  msgs.push({ role: "assistant", content: (r.content[0] as any).text });
}
