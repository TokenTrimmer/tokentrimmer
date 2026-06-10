// datetime-style interpolation inside a prompt template string.
function buildSystemPrompt(clock: { now(): string }) {
  return `You are a planner. The system clock reads ${clock.now()}; plan accordingly and stay terse.`;
}
export { buildSystemPrompt };
