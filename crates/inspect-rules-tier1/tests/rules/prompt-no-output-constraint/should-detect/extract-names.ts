import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic();
const response = await client.messages.create({
  model: "claude-3-opus-20240229",
  messages: [
    {
      role: "user",
      content: "Extract all person names from: Alice and Bob went to the park with Charlie."
    }
  ]
});
