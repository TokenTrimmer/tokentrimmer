// Negative: JS tool loop guarded by a budget.
function runAgent(task) {
  let spend = 0;
  while (true) {
    if (spend > BUDGET) break;
    const output = handle_tool_call(next());
    spend += output.costUsd;
  }
}
