"""Negative: agent loop with timeout — counts as termination condition."""
import anthropic
import time

client = anthropic.Anthropic()

def run_with_timeout(task: str, timeout: int = 60):
    messages = [{"role": "user", "content": task}]
    start = time.time()
    while True:
        if time.time() - start > timeout:
            break
        response = client.messages.create(
            model="claude-3-5-sonnet-20241022",
            max_tokens=1024,
            tools=TOOLS,
            messages=messages,
        )
        if response.stop_reason == "end_turn":
            return response
