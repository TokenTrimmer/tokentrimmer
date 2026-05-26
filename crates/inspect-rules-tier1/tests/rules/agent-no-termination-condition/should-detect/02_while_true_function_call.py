"""Positive: agentic while True loop with function_call, no cap."""
import anthropic

client = anthropic.Anthropic()

def agent(task: str):
    messages = [{"role": "user", "content": task}]
    while True:
        response = client.messages.create(
            model="claude-3-5-sonnet-20241022",
            max_tokens=1024,
            tools=TOOLS,
            messages=messages,
        )
        if response.stop_reason == "end_turn":
            return response
        # Handle tool_use blocks
        for block in response.content:
            if block.type == "tool_use":
                result = call_tool(block.name, block.input)
                messages.append({"role": "user", "content": [{"type": "tool_result", "tool_use_id": block.id, "content": result}]})
