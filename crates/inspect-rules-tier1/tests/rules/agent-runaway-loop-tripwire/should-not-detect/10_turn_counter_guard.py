"""Negative: turn_count guard in the tool loop."""
while True:
    if turn_count > 20:
        break
    tool_call = model(history)
    call_tool(tool_call)
    turn_count += 1
