// Positive: TypeScript Anthropic call to classify intent.
import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic();

async function classifyIntent(userInput: string): Promise<string> {
  const response = await client.messages.create({
    model: "claude-3-5-sonnet-20241022",
    max_tokens: 20,
    system: "You classify user intent into one of: question, complaint, compliment, other.",
    messages: [{ role: "user", content: userInput }],
  });
  return (response.content[0] as { text: string }).text;
}
