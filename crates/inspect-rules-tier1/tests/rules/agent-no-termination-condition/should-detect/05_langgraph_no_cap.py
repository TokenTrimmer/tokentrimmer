"""Positive: langgraph workflow loop with tool_use and no iteration cap."""
import langgraph
import anthropic

client = anthropic.Anthropic()

def run_graph(initial_state):
    state = initial_state
    while True:
        response = client.messages.create(
            model="claude-3-5-sonnet-20241022",
            max_tokens=2048,
            tools=TOOLS,
            messages=state["messages"],
        )
        if response.stop_reason == "end_turn":
            break
        # process tool_use responses
        for block in response.content:
            if block.type == "tool_use":
                state["messages"].append(handle_tool(block))
    return state
