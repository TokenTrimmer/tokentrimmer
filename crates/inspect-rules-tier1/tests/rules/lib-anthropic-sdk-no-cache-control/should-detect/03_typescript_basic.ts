// Positive: TypeScript Anthropic SDK, no prompt-cache annotation anywhere in file.
import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic();

async function chat(message: string) {
  return await client.messages.create({
    model: "claude-3-5-sonnet-20241022",
    max_tokens: 1024,
    system: "You are a helpful assistant.",
    messages: [{ role: "user", content: message }],
  });
}
