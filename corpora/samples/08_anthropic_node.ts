import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic();

export async function review(diff: string) {
  return client.messages.create({
    model: "claude-sonnet-4-6",
    max_tokens: 1024,
    system: [{ type: "text", text: "You are a senior code reviewer.",
               cache_control: { type: "ephemeral" } }],
    messages: [{ role: "user", content: `Review this diff:\n${diff}` }],
  });
}
