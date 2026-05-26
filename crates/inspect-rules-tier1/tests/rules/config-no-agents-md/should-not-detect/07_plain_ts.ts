// Negative: plain TypeScript, no LLM imports.
type User = { name: string; age: number };

function formatUser(u: User): string {
  return `${u.name} (${u.age})`;
}
