"""Positive: while True loop calling call_tool with no guard."""
def agent_loop(prompt: str):
    context = [prompt]
    while True:
        action = decide(context)
        output = call_tool(action)
        context.append(output)
