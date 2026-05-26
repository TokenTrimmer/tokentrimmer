// Negative: TypeScript with max_iterations guard.
import OpenAI from "openai";

const client = new OpenAI();
const MAX_ITERATIONS = 15;

async function runAgent(task: string) {
  const messages: any[] = [{ role: "user", content: task }];
  let iterations = 0;
  while (true) {
    if (iterations >= MAX_ITERATIONS) break;
    iterations++;
    const response = await client.chat.completions.create({
      model: "gpt-4o",
      messages,
      tools: TOOLS,
    });
    if (response.choices[0].finish_reason === "stop") break;
  }
}
