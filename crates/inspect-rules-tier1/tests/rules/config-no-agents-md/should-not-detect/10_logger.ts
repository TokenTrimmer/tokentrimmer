// Negative: logging utility, no LLM.
const LOG_LEVEL = process.env.LOG_LEVEL ?? "info";

export function log(level: string, msg: string): void {
  if (level >= LOG_LEVEL) console.log(`[${level}] ${msg}`);
}
