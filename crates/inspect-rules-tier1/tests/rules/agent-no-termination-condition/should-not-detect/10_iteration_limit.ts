// Negative: iteration_limit constant used — termination guard present.
import Anthropic from "@anthropic-ai/sdk";

const client = new Anthropic();
const iteration_limit = 25;

async function agent(task: string) {
  const messages: Anthropic.MessageParam[] = [{ role: "user", content: task }];
  let count = 0;
  while (true) {
    count++;
    if (count > iteration_limit) break;
    const response = await client.messages.create({
      model: "claude-3-5-sonnet-20241022",
      max_tokens: 1024,
      tools: TOOLS,
      messages,
    });
    if (response.stop_reason === "end_turn") break;
  }
}
