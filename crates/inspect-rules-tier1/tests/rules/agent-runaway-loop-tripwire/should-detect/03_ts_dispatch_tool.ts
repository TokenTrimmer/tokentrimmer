// Positive: TypeScript while (true) dispatch_tool loop, no budget.
function agentLoop(task: string) {
  const context = [task];
  while (true) {
    const action = decide(context);
    const output = dispatch_tool(action);
    context.push(output);
  }
}
