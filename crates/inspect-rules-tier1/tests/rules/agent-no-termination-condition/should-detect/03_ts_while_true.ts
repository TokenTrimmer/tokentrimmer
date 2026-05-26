// Positive: TypeScript agent with while(true) and tool_calls.
import OpenAI from "openai";

const client = new OpenAI();

async function runAgent(task: string) {
  const messages: any[] = [{ role: "user", content: task }];
  while (true) {
    const response = await client.chat.completions.create({
      model: "gpt-4o",
      messages,
      tools: TOOLS,
    });
    const choice = response.choices[0];
    if (choice.finish_reason === "stop") break;
    const tool_calls = choice.message.tool_calls ?? [];
    for (const tc of tool_calls) {
      const result = await executeTool(tc);
      messages.push({ role: "tool", content: result, tool_call_id: tc.id });
    }
  }
}
