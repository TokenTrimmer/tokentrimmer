// Negative: utility module, no LLM calls.
export function sentimentScore(text: string): number {
  const positiveWords = ["good", "great", "excellent"];
  return positiveWords.filter(w => text.includes(w)).length;
}
