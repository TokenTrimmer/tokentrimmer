// Negative: utility file with no LLM calls.
export function formatDate(d: Date): string {
  return d.toISOString().slice(0, 10);
}
