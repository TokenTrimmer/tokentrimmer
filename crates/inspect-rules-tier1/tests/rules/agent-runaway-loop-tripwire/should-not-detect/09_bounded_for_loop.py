"""Negative: bounded for loop dispatching tools (not a runaway pattern)."""
for i in range(10):
    tool_call = agent.get_action()
    execute_tool(tool_call)
