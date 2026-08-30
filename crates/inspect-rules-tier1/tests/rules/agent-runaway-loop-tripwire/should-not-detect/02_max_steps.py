"""Negative: loop bounded by max_steps."""
while True:
    if step >= max_steps:
        break
    execute_tool(get_action())
    step += 1
