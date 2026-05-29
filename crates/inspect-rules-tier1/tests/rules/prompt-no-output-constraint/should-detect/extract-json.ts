import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic();
const response = await client.messages.create({
  model: "claude-3-haiku-20240307",
  messages: [
    {
      role: "user",
      content: "Extract the person name from this text: John Smith went to the store."
    }
  ]
});
