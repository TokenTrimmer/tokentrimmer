"""Positive: while True loop dispatching tools with no turn budget."""
import openai

client = openai.OpenAI()

def run_agent(task: str):
    messages = [{"role": "user", "content": task}]
    while True:
        response = client.chat.completions.create(model="gpt-4o", messages=messages)
        tool_call = response.choices[0].message.tool_calls[0]
        result = execute_tool(tool_call)
        messages.append({"role": "tool", "content": result})
