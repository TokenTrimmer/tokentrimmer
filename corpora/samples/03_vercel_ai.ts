import { generateText } from "ai";
import { openai } from "@ai-sdk/openai";

export async function draft(prompt: string) {
  const { text } = await generateText({
    model: openai("gpt-4o"),
    maxTokens: 512,
    system: "You write concise marketing copy.",
    prompt,
  });
  return text;
}
