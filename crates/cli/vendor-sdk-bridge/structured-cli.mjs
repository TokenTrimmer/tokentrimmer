import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const MCP_TOKEN_ENV = "TT_TOKENTRIMMER_MCP_TOKEN";
const SYSTEM_PROMPT =
  "You are running inside TokenTrimmer. Only the supplied tokentrimmer MCP tools are available for repository actions. Respect their denials and return a final answer when the task is complete.";
const UNAUTHORIZED_CODEX_ITEMS = new Set([
  "command_execution",
  "file_change",
  "web_search",
]);

function toml(value) {
  return JSON.stringify(value);
}

function normalizedUsage(usage = {}) {
  return {
    inputTokens: Number(usage.input_tokens ?? 0),
    cachedInputTokens: Number(
      usage.cached_input_tokens ?? usage.cache_read_input_tokens ?? 0,
    ),
    cacheWriteInputTokens: Number(usage.cache_write_input_tokens ?? 0),
    outputTokens: Number(usage.output_tokens ?? 0),
    reasoningOutputTokens: Number(usage.reasoning_output_tokens ?? 0),
  };
}

function boundedDiagnostic(value) {
  return String(value ?? "")
    .slice(0, 8192)
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/g, "");
}

function parseJsonLines(stdout, vendor) {
  const events = [];
  for (const [index, line] of stdout.split(/\r?\n/u).entries()) {
    if (line.trim() === "") continue;
    let event;
    try {
      event = JSON.parse(line);
    } catch (error) {
      throw new Error(
        `${vendor} structured CLI emitted invalid JSONL at line ${index + 1}: ${error.message}`,
      );
    }
    if (event === null || typeof event !== "object" || Array.isArray(event)) {
      throw new Error(`${vendor} structured CLI emitted a non-object event`);
    }
    events.push(event);
  }
  if (events.length === 0) {
    throw new Error(`${vendor} structured CLI emitted no structured events`);
  }
  return events;
}

export function codexCliPrompt(request) {
  return [
    "TokenTrimmer execution contract:",
    `- Complete the requested task in at most ${request.maxTurns} agent turns.`,
    "- Use only the tokentrimmer MCP tools for repository inspection or mutation.",
    "- Shell, built-in patching, web search, and direct network access are unavailable.",
    "- Stop on any tool denial; do not bypass policy or access paths outside the supplied workspace.",
    "",
    request.prompt,
  ].join("\n");
}

export function buildCodexCliArgs(request) {
  const overrides = [
    ["approval_policy", "never"],
    ["features.shell_tool", false],
    ["features.unified_exec", false],
    ["web_search", "disabled"],
    ["tools.web_search", false],
    ["mcp_servers.tokentrimmer.url", request.mcp.url],
    ["mcp_servers.tokentrimmer.bearer_token_env_var", MCP_TOKEN_ENV],
    ["mcp_servers.tokentrimmer.enabled", true],
    ["mcp_servers.tokentrimmer.required", true],
    ["mcp_servers.tokentrimmer.enabled_tools", request.mcp.tools],
    ["mcp_servers.tokentrimmer.default_tools_approval_mode", "approve"],
    ["mcp_servers.tokentrimmer.startup_timeout_sec", 10],
    ["mcp_servers.tokentrimmer.tool_timeout_sec", 60],
  ];
  const args = [
    "exec",
    "--json",
    "--color",
    "never",
    "--strict-config",
    "--ignore-user-config",
    "--ignore-rules",
    "--skip-git-repo-check",
    "--sandbox",
    "read-only",
    "--cd",
    request.cwd,
  ];
  if (request.model !== undefined) args.push("--model", request.model);
  for (const [key, value] of overrides) {
    args.push("--config", `${key}=${toml(value)}`);
  }
  if (request.sessionId !== undefined) {
    args.push("resume", request.sessionId, "-");
  } else {
    args.push("-");
  }
  return args;
}

export function buildClaudeMcpConfig(request) {
  return {
    mcpServers: {
      tokentrimmer: {
        type: "http",
        url: request.mcp.url,
        headers: {
          Authorization: `Bearer ${request.mcp.token}`,
        },
      },
    },
  };
}

export function buildClaudeCliArgs(request, mcpConfigPath) {
  const tools = request.mcp.tools.map((name) => `mcp__tokentrimmer__${name}`);
  const args = [
    "--print",
    "--output-format",
    "stream-json",
    "--verbose",
    "--setting-sources",
    "",
    "--strict-mcp-config",
    "--mcp-config",
    mcpConfigPath,
    "--tools",
    "",
    "--allowedTools",
    tools.join(","),
    "--permission-mode",
    "dontAsk",
    "--disable-slash-commands",
    "--system-prompt",
    SYSTEM_PROMPT,
    "--max-turns",
    String(request.maxTurns),
  ];
  if (request.model !== undefined) args.push("--model", request.model);
  if (request.maxBudgetUsd !== undefined) {
    args.push("--max-budget-usd", String(request.maxBudgetUsd));
  }
  if (request.sessionId !== undefined) args.push("--resume", request.sessionId);
  return args;
}

export function parseCodexCliOutput(request, execution) {
  const events = parseJsonLines(execution.stdout, "Codex");
  const allowedTools = new Set(request.mcp.tools);
  let sessionId = request.sessionId ?? null;
  let response = "";
  let usage = null;
  let failure = null;
  let terminal = false;
  let toolCalls = 0;

  for (const event of events) {
    if (event.type === "thread.started" && typeof event.thread_id === "string") {
      sessionId = event.thread_id;
    }
    if (event.type === "item.completed" && event.item?.type === "agent_message") {
      if (typeof event.item.text === "string") response = event.item.text;
    }
    if (event.type === "item.completed" && event.item?.type === "mcp_tool_call") {
      toolCalls += 1;
      if (
        event.item.server !== "tokentrimmer" ||
        typeof event.item.tool !== "string" ||
        !allowedTools.has(event.item.tool)
      ) {
        failure = `Codex invoked unauthorized MCP tool ${String(event.item.server)}/${String(event.item.tool)}`;
      } else if (event.item.error !== null && event.item.error !== undefined) {
        failure = `MCP tokentrimmer/${event.item.tool}: ${boundedDiagnostic(
          event.item.error.message ?? event.item.error,
        )}`;
      }
    }
    if (event.type === "item.completed" && UNAUTHORIZED_CODEX_ITEMS.has(event.item?.type)) {
      failure = `Codex emitted unauthorized built-in tool event ${event.item.type}`;
    }
    if (event.type === "turn.completed") {
      terminal = true;
      usage = normalizedUsage(event.usage);
    }
    if (event.type === "turn.failed") {
      terminal = true;
      failure = boundedDiagnostic(event.error?.message ?? "Codex turn failed");
    }
    if (event.type === "error") {
      failure = boundedDiagnostic(event.message ?? "Codex emitted an error");
    }
  }

  if (!terminal && failure === null) failure = "Codex CLI ended without a terminal turn event";
  if (execution.code !== 0 && failure === null) {
    failure = `Codex CLI exited with ${execution.code}: ${boundedDiagnostic(execution.stderr)}`;
  }
  return {
    ok: failure === null,
    runner: request.runner,
    sessionId,
    response,
    usage,
    totalCostUsd: null,
    toolCalls,
    ...(failure === null ? {} : { error: failure }),
  };
}

export function parseClaudeCliOutput(request, execution) {
  const events = parseJsonLines(execution.stdout, "Claude");
  const allowedTools = new Set(
    request.mcp.tools.map((name) => `mcp__tokentrimmer__${name}`),
  );
  let result = null;
  let failure = null;
  let toolCalls = 0;

  for (const event of events) {
    if (event.type === "assistant") {
      const content = Array.isArray(event.message?.content) ? event.message.content : [];
      for (const block of content) {
        if (block?.type !== "tool_use") continue;
        toolCalls += 1;
        if (typeof block.name !== "string" || !allowedTools.has(block.name)) {
          failure = `Claude invoked unauthorized tool ${String(block?.name)}`;
        }
      }
    }
    if (event.type === "result") result = event;
  }

  if (result === null) failure ??= "Claude CLI ended without a result event";
  if (execution.code !== 0 && failure === null) {
    failure = `Claude CLI exited with ${execution.code}: ${boundedDiagnostic(execution.stderr)}`;
  }
  const succeeded = result?.subtype === "success" && result?.is_error === false;
  if (!succeeded && failure === null) {
    failure = Array.isArray(result?.errors)
      ? result.errors.map(boundedDiagnostic).join("; ")
      : `Claude CLI stopped with ${String(result?.subtype ?? "unknown")}`;
  }
  return {
    ok: failure === null,
    runner: request.runner,
    sessionId: typeof result?.session_id === "string" ? result.session_id : null,
    response: succeeded && typeof result?.result === "string" ? result.result : "",
    usage: result === null ? null : normalizedUsage(result.usage),
    totalCostUsd: Number.isFinite(result?.total_cost_usd) ? result.total_cost_usd : null,
    toolCalls,
    ...(failure === null ? {} : { error: failure }),
  };
}

export function runCliProcess(
  executable,
  args,
  { cwd, env, input, maxOutputBytes },
  spawnProcess = spawn,
) {
  return new Promise((resolve, reject) => {
    const child = spawnProcess(executable, args, {
      cwd,
      env,
      stdio: ["pipe", "pipe", "pipe"],
      windowsHide: true,
    });
    const stdout = [];
    const stderr = [];
    let observedBytes = 0;
    let settled = false;

    const rejectOnce = (error) => {
      if (settled) return;
      settled = true;
      child.kill("SIGKILL");
      reject(error);
    };
    const collect = (target, chunk) => {
      observedBytes += chunk.length;
      if (observedBytes > maxOutputBytes) {
        rejectOnce(
          new Error(`vendor structured CLI output exceeded ${maxOutputBytes} bytes`),
        );
        return;
      }
      target.push(chunk);
    };

    child.once("error", rejectOnce);
    child.stdout.on("data", (chunk) => collect(stdout, chunk));
    child.stderr.on("data", (chunk) => collect(stderr, chunk));
    child.once("close", (code, signal) => {
      if (settled) return;
      settled = true;
      resolve({
        code: code ?? (signal === null ? 1 : 128),
        signal,
        stdout: Buffer.concat(stdout).toString("utf8"),
        stderr: Buffer.concat(stderr).toString("utf8"),
      });
    });
    child.stdin.once("error", rejectOnce);
    child.stdin.end(input);
  });
}

export async function runCodexCli(request, executable, executor = runCliProcess) {
  const execution = await executor(executable, buildCodexCliArgs(request), {
    cwd: request.cwd,
    env: { ...process.env, [MCP_TOKEN_ENV]: request.mcp.token },
    input: `${codexCliPrompt(request)}\n`,
    maxOutputBytes: request.maxOutputBytes,
  });
  return parseCodexCliOutput(request, execution);
}

export async function runClaudeCli(request, executable, executor = runCliProcess) {
  const directory = mkdtempSync(join(tmpdir(), "tokentrimmer-claude-mcp-"));
  const configPath = join(directory, "mcp.json");
  try {
    writeFileSync(configPath, JSON.stringify(buildClaudeMcpConfig(request)), {
      encoding: "utf8",
      mode: 0o600,
      flag: "wx",
    });
    const execution = await executor(executable, buildClaudeCliArgs(request, configPath), {
      cwd: request.cwd,
      env: { ...process.env, CLAUDE_AGENT_SDK_CLIENT_APP: "tokentrimmer" },
      input: `${request.prompt}\n`,
      maxOutputBytes: request.maxOutputBytes,
    });
    return parseClaudeCliOutput(request, execution);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}
