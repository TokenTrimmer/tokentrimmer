"""Positive: while True agent loop with tool_call, no iteration cap."""
import openai

client = openai.OpenAI()

def run_agent(task: str):
    messages = [{"role": "user", "content": task}]
    while True:
        response = client.chat.completions.create(
            model="gpt-4o",
            messages=messages,
            tools=TOOLS,
        )
        choice = response.choices[0]
        if choice.finish_reason == "stop":
            break
        tool_call = choice.message.tool_calls[0]
        result = execute_tool(tool_call)
        messages.append({"role": "tool", "content": result})
