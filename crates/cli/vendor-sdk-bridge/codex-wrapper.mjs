#!/usr/bin/env node

import { spawn } from "node:child_process";

const realCodex = process.env.TT_CODEX_REAL_PATH;
if (!realCodex) {
  process.stderr.write("TT_CODEX_REAL_PATH is required\n");
  process.exit(2);
}
const args = process.argv.slice(2);
if (args[0] !== "exec") {
  process.stderr.write("TokenTrimmer Codex wrapper accepts only the exec subcommand\n");
  process.exit(2);
}
const env = { ...process.env };
delete env.TT_CODEX_REAL_PATH;
const child = spawn(
  realCodex,
  ["exec", "--ignore-user-config", "--ignore-rules", ...args.slice(1)],
  { env, stdio: "inherit", windowsHide: true },
);
for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
  process.on(signal, () => child.kill(signal));
}
child.on("error", (error) => {
  process.stderr.write(`Failed to start pinned Codex runtime: ${error.message}\n`);
  process.exitCode = 1;
});
child.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
  } else {
    process.exitCode = code ?? 1;
  }
});
