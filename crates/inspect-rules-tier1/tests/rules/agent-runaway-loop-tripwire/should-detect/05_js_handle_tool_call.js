// Positive: JS while (true) handleToolCall loop, no guard.
function runAgent(task) {
  const messages = [task];
  while (true) {
    const action = next(messages);
    const output = handle_tool_call(action);
    messages.push(output);
  }
}
