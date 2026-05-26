// Negative: Anthropic SDK imported but only used to build the client object.
import Anthropic from "@anthropic-ai/sdk";

export function createClient(): Anthropic {
  return new Anthropic({ apiKey: process.env.ANTHROPIC_API_KEY });
}
