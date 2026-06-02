"""A tool-using agent loop with a hard iteration cap."""
from openai import OpenAI

client = OpenAI()

def run_agent(goal: str, max_iterations: int = 8):
    history = [{"role": "user", "content": goal}]
    for step in range(max_iterations):
        resp = client.chat.completions.create(
            model="gpt-4o",
            max_tokens=512,
            messages=history,
            tools=TOOLS,
        )
        msg = resp.choices[0].message
        if not msg.tool_calls:
            return msg.content
        history.append(msg)
        # Keep a bounded sliding window so context cost stays flat.
        history = history[-20:]
    return "stopped: max_iterations reached"

TOOLS = [{"type": "function", "function": {"name": "noop"}}]
