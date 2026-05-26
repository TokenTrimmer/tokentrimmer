"""Negative: while True with max_iterations cap."""
import openai

client = openai.OpenAI()
MAX_ITERATIONS = 10

def run_agent(task: str):
    messages = [{"role": "user", "content": task}]
    iteration = 0
    while True:
        if iteration >= MAX_ITERATIONS:
            break
        iteration += 1
        response = client.chat.completions.create(
            model="gpt-4o", messages=messages, tools=TOOLS
        )
        if response.choices[0].finish_reason == "stop":
            break
        tool_call = response.choices[0].message.tool_calls[0]
        messages.append({"role": "tool", "content": execute_tool(tool_call)})
