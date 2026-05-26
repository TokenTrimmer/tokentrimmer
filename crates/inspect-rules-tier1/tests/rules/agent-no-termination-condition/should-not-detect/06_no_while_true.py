"""Negative: uses tool_call but with a for-loop with fixed range."""
import openai

client = openai.OpenAI()

def run_agent(task: str, max_steps: int = 10):
    messages = [{"role": "user", "content": task}]
    for _ in range(max_steps):
        response = client.chat.completions.create(
            model="gpt-4o",
            messages=messages,
            tools=TOOLS,
        )
        if response.choices[0].finish_reason == "stop":
            break
        tool_call = response.choices[0].message.tool_calls[0]
        messages.append({"role": "tool", "content": execute_tool(tool_call)})
