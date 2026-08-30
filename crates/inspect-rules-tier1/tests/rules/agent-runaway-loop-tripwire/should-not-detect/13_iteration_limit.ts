// Negative: TS iteration limit on the tool loop.
while (true) {
  if (iteration >= MAX_ITERATION_LIMIT) break;
  const out = call_tool(nextStep());
  iteration++;
}
