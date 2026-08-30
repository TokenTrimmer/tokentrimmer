"""Negative: tool dispatch exists but there is no infinite loop."""
def process(task: str):
    tool_call = parse(task)
    return execute_tool(tool_call)
