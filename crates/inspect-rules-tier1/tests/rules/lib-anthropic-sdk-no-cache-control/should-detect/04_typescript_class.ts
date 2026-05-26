// Positive: class-based TypeScript usage, missing prompt-cache annotation.
import { Anthropic } from "@anthropic-ai/sdk";

export class LLMService {
  private readonly anthropic = new Anthropic();

  async complete(prompt: string): Promise<string> {
    const response = await this.anthropic.messages.create({
      model: "claude-3-5-sonnet-20241022",
      max_tokens: 2000,
      messages: [{ role: "user", content: prompt }],
    });
    return (response.content[0] as { text: string }).text;
  }
}
