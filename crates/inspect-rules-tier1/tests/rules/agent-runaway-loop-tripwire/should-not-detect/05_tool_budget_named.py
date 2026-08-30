"""Negative: tool loop with an explicit remaining-budget check."""
while True:
    if remaining_budget_usd < 0.01:
        break
    tool_call = poll()
    execute_tool(tool_call)
