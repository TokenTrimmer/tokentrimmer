// Negative: config loading utility, no LLM.
import fs from "fs";
import path from "path";

export function loadEnv(file: string): Record<string, string> {
  const content = fs.readFileSync(path.resolve(file), "utf-8");
  return Object.fromEntries(content.split("\n").map(l => l.split("=")));
}
