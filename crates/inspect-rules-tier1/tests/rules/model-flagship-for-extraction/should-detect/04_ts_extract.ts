// Positive: Claude Sonnet for entity extraction in TypeScript.
import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic();

async function extractEntities(text: string) {
  return await client.messages.create({
    model: "claude-3-5-sonnet-20241022",
    max_tokens: 256,
    system: "You extract named entity lists from text in JSON format.",
    messages: [{ role: "user", content: text }],
  });
}
