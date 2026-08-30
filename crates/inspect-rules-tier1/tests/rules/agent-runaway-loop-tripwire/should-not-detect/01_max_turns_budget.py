"""Negative: tool loop guarded by a max_turns budget."""
def agent(task: str):
    context = [task]
    while True:
        if context["turn_count"] >= context["max_turns"]:
            break
        tool_call = plan(context)
        output = execute_tool(tool_call)
        context.append(output)
