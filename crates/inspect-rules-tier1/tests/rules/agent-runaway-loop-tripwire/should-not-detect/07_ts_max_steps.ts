// Negative: TS tool loop with a turn budget.
function agentLoop(task: string) {
  const context = [task];
  while (true) {
    if (context.turn_count >= context.max_turns) break;
    const output = dispatch_tool(decide(context));
    context.push(output);
  }
}
