"""Negative: loop bounded by max_iterations."""
while True:
    if iteration < max_iterations:
        dispatch_tool(next_action())
        iteration += 1
    else:
        break
