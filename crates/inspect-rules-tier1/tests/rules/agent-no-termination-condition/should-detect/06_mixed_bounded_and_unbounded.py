"""Positive: file has both a bounded loop AND an unbounded loop.

The bounded loop with max_iterations should not suppress the unbounded loop.
The rule must report the second loop (line ~25) only, not suppress it because
of max_iterations in the first function.
"""
import anthropic

client = anthropic.Anthropic()

# Define some tool_calls for agent pattern matching
TOOLS = [
    {"name": "search", "description": "Search for information"}
]


def safe_bounded_agent(task: str, max_iterations: int = 5):
    """This function has a bounded loop with max_iterations — should NOT fire."""
    messages = [{"role": "user", "content": task}]
    for iteration in range(max_iterations):
        response = client.messages.create(
            model="claude-3-5-sonnet-20241022",
            max_tokens=1024,
            tools=TOOLS,
            messages=messages,
        )
        if response.stop_reason == "end_turn":
            return response
        # Process tool_call if present
        if response.tool_calls:
            tool_call = response.tool_calls[0]
            result = execute_tool(tool_call)
            messages.append({"role": "tool", "content": result})


def dangerous_unbounded_agent(task: str):
    """This function has an unbounded loop — should DEFINITELY fire."""
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
        # Process tool_call if present
        if response.tool_calls:
            tool_call = response.tool_calls[0]
            result = execute_tool(tool_call)
            messages.append({"role": "tool", "content": result})
