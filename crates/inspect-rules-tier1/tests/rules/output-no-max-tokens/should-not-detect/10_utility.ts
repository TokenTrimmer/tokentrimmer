// Negative: utility file.
export const MAX_TOKENS = 1024;
export function getLimit(type: string): number {
  return MAX_TOKENS;
}
