#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { existsSync, realpathSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { runClaudeCli, runCodexCli } from "./structured-cli.mjs";

const RUNNERS = new Set(["codex-sdk", "claude-agent-sdk", "codex-cli", "claude-cli"]);
export const SDK_COMPATIBILITY = Object.freeze({
  "@openai/codex-sdk": {
    sdkVersion: "0.147.0",
    runtime: "codex-cli",
    runtimeVersion: "0.147.0",
  },
  "@anthropic-ai/claude-agent-sdk": {
    sdkVersion: "0.3.233",
    runtime: "claude-code",
    runtimeVersion: "2.1.233",
  },
});
const TOOL_NAME = /^[A-Za-z0-9_-]{1,128}$/;
const MAX_PROMPT_BYTES = 1_048_576;

function fail(message) {
  throw new Error(`invalid vendor bridge request: ${message}`);
}

function isRecord(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

export function validateRequest(value) {
  if (!isRecord(value)) fail("request must be an object");
  if (!RUNNERS.has(value.runner)) fail("runner is unsupported");
  if (typeof value.prompt !== "string" || value.prompt.trim() === "") {
    fail("prompt must be non-empty");
  }
  if (Buffer.byteLength(value.prompt, "utf8") > MAX_PROMPT_BYTES) {
    fail("prompt exceeds 1 MiB");
  }
  if (typeof value.cwd !== "string" || !isAbsolute(value.cwd)) {
    fail("cwd must be an absolute path");
  }
  if (!Number.isInteger(value.maxTurns) || value.maxTurns < 1 || value.maxTurns > 256) {
    fail("maxTurns must be an integer from 1 through 256");
  }
  if (
    !Number.isInteger(value.maxOutputBytes) ||
    value.maxOutputBytes < 1 ||
    value.maxOutputBytes > 16 * 1024 * 1024
  ) {
    fail("maxOutputBytes must be an integer from 1 through 16777216");
  }
  if (
    value.maxBudgetUsd !== undefined &&
    (typeof value.maxBudgetUsd !== "number" ||
      !Number.isFinite(value.maxBudgetUsd) ||
      value.maxBudgetUsd <= 0)
  ) {
    fail("maxBudgetUsd must be a finite positive number");
  }
  for (const field of ["model", "sessionId"]) {
    if (
      value[field] !== undefined &&
      (typeof value[field] !== "string" || value[field].trim() === "")
    ) {
      fail(`${field} must be a non-empty string`);
    }
  }
  if (
    value.sessionId !== undefined &&
    !/^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$/u.test(value.sessionId)
  ) {
    fail("sessionId must be a safe vendor session identifier");
  }
  if (
    value.executablePath !== undefined &&
    (typeof value.executablePath !== "string" || !isAbsolute(value.executablePath))
  ) {
    fail("executablePath must be an absolute path");
  }
  if (!isRecord(value.mcp)) fail("mcp must be an object");

  let endpoint;
  try {
    endpoint = new URL(value.mcp.url);
  } catch {
    fail("mcp.url must be a URL");
  }
  if (
    endpoint.protocol !== "http:" ||
    endpoint.hostname !== "127.0.0.1" ||
    endpoint.port === "" ||
    endpoint.pathname !== "/mcp" ||
    endpoint.username !== "" ||
    endpoint.password !== "" ||
    endpoint.search !== "" ||
    endpoint.hash !== ""
  ) {
    fail("mcp.url must be an uncredentialed http://127.0.0.1:<port>/mcp endpoint");
  }
  if (typeof value.mcp.token !== "string" || value.mcp.token.length < 32) {
    fail("mcp.token must contain at least 32 characters");
  }
  if (!Array.isArray(value.mcp.tools) || value.mcp.tools.length === 0) {
    fail("mcp.tools must be a non-empty array");
  }
  const tools = new Set();
  for (const tool of value.mcp.tools) {
    if (typeof tool !== "string" || !TOOL_NAME.test(tool)) {
      fail("every MCP tool name must match [A-Za-z0-9_-]{1,128}");
    }
    if (tools.has(tool)) fail(`duplicate MCP tool name: ${tool}`);
    tools.add(tool);
  }

  return value;
}

export function buildCodexConfig(request) {
  return {
    features: {
      shell_tool: false,
      unified_exec: false,
    },
    web_search: "disabled",
    tools: {
      web_search: false,
    },
    mcp_servers: {
      tokentrimmer: {
        url: request.mcp.url,
        http_headers: {
          Authorization: `Bearer ${request.mcp.token}`,
        },
        enabled: true,
        required: true,
        enabled_tools: request.mcp.tools,
        default_tools_approval_mode: "approve",
        startup_timeout_sec: 10,
        tool_timeout_sec: 60,
      },
    },
  };
}

export function buildCodexThreadOptions(request) {
  return {
    workingDirectory: request.cwd,
    skipGitRepoCheck: true,
    ...(request.model === undefined ? {} : { model: request.model }),
    sandboxMode: "read-only",
    networkAccessEnabled: false,
    webSearchEnabled: false,
    approvalPolicy: "never",
  };
}

function claudeToolNames(request) {
  return request.mcp.tools.map((name) => `mcp__tokentrimmer__${name}`);
}

export function buildClaudeOptions(request, env = process.env) {
  const tools = claudeToolNames(request);
  return {
    cwd: request.cwd,
    ...(request.model === undefined ? {} : { model: request.model }),
    maxTurns: request.maxTurns,
    ...(request.maxBudgetUsd === undefined ? {} : { maxBudgetUsd: request.maxBudgetUsd }),
    ...(request.sessionId === undefined ? {} : { resume: request.sessionId }),
    ...(request.executablePath === undefined
      ? {}
      : { pathToClaudeCodeExecutable: request.executablePath }),
    env: {
      ...env,
      CLAUDE_AGENT_SDK_CLIENT_APP: "tokentrimmer",
    },
    settingSources: [],
    strictMcpConfig: true,
    mcpServers: {
      tokentrimmer: {
        type: "http",
        url: request.mcp.url,
        headers: {
          Authorization: `Bearer ${request.mcp.token}`,
        },
      },
    },
    tools,
    allowedTools: tools,
    permissionMode: "dontAsk",
    persistSession: true,
    systemPrompt:
      "You are running inside TokenTrimmer. Only the supplied tokentrimmer MCP tools are available for repository actions. Respect their denials and return a final answer when the task is complete.",
  };
}

function normalizedUsage(usage = {}) {
  return {
    inputTokens: Number(usage.input_tokens ?? 0),
    cachedInputTokens: Number(usage.cached_input_tokens ?? usage.cache_read_input_tokens ?? 0),
    cacheWriteInputTokens: Number(usage.cache_write_input_tokens ?? 0),
    outputTokens: Number(usage.output_tokens ?? 0),
    reasoningOutputTokens: Number(usage.reasoning_output_tokens ?? 0),
  };
}

function codexPrompt(request) {
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

function bundledCodexPath() {
  const target = {
    "darwin:arm64": ["@openai/codex-darwin-arm64", "aarch64-apple-darwin"],
    "darwin:x64": ["@openai/codex-darwin-x64", "x86_64-apple-darwin"],
    "linux:arm64": ["@openai/codex-linux-arm64", "aarch64-unknown-linux-musl"],
    "linux:x64": ["@openai/codex-linux-x64", "x86_64-unknown-linux-musl"],
    "win32:arm64": ["@openai/codex-win32-arm64", "aarch64-pc-windows-msvc"],
    "win32:x64": ["@openai/codex-win32-x64", "x86_64-pc-windows-msvc"],
  }[`${process.platform}:${process.arch}`];
  if (!target) throw new Error(`unsupported Codex platform ${process.platform}:${process.arch}`);
  const moduleRequire = createRequire(import.meta.url);
  const codexPackage = moduleRequire.resolve("@openai/codex/package.json");
  const codexRequire = createRequire(codexPackage);
  const platformPackage = codexRequire.resolve(`${target[0]}/package.json`);
  const root = join(dirname(platformPackage), "vendor", target[1]);
  const binary = process.platform === "win32" ? "codex.exe" : "codex";
  for (const candidate of [join(root, "bin", binary), join(root, "codex", binary)]) {
    if (existsSync(candidate)) return realpathSync(candidate);
  }
  throw new Error(`pinned Codex runtime is missing from ${target[0]}`);
}

function installedCodexNativePath(launcher) {
  const resolvedLauncher = realpathSync(launcher);
  if (!resolvedLauncher.endsWith(".js")) return resolvedLauncher;
  const target = {
    "darwin:arm64": ["@openai/codex-darwin-arm64", "aarch64-apple-darwin"],
    "darwin:x64": ["@openai/codex-darwin-x64", "x86_64-apple-darwin"],
    "linux:arm64": ["@openai/codex-linux-arm64", "aarch64-unknown-linux-musl"],
    "linux:x64": ["@openai/codex-linux-x64", "x86_64-unknown-linux-musl"],
    "win32:arm64": ["@openai/codex-win32-arm64", "aarch64-pc-windows-msvc"],
    "win32:x64": ["@openai/codex-win32-x64", "x86_64-pc-windows-msvc"],
  }[`${process.platform}:${process.arch}`];
  if (!target) throw new Error(`unsupported Codex platform ${process.platform}:${process.arch}`);
  const launcherRequire = createRequire(pathToFileURL(resolvedLauncher));
  const codexPackage = launcherRequire.resolve("@openai/codex/package.json");
  const codexRequire = createRequire(codexPackage);
  const platformPackage = codexRequire.resolve(`${target[0]}/package.json`);
  const root = join(dirname(platformPackage), "vendor", target[1]);
  const binary = process.platform === "win32" ? "codex.exe" : "codex";
  for (const candidate of [join(root, "bin", binary), join(root, "codex", binary)]) {
    if (existsSync(candidate)) return realpathSync(candidate);
  }
  throw new Error(`installed Codex runtime is missing from ${target[0]}`);
}

function bundledClaudePath() {
  const packageName = {
    "darwin:arm64": "@anthropic-ai/claude-agent-sdk-darwin-arm64",
    "darwin:x64": "@anthropic-ai/claude-agent-sdk-darwin-x64",
    "linux:arm64": "@anthropic-ai/claude-agent-sdk-linux-arm64",
    "linux:x64": "@anthropic-ai/claude-agent-sdk-linux-x64",
    "win32:arm64": "@anthropic-ai/claude-agent-sdk-win32-arm64",
    "win32:x64": "@anthropic-ai/claude-agent-sdk-win32-x64",
  }[`${process.platform}:${process.arch}`];
  if (!packageName) {
    throw new Error(`unsupported Claude platform ${process.platform}:${process.arch}`);
  }
  const moduleRequire = createRequire(import.meta.url);
  const packageJson = moduleRequire.resolve(`${packageName}/package.json`);
  const binary = join(dirname(packageJson), process.platform === "win32" ? "claude.exe" : "claude");
  if (!existsSync(binary)) throw new Error(`pinned Claude runtime is missing from ${packageName}`);
  return realpathSync(binary);
}

function probeRuntime(runner, explicitPath) {
  const executable =
    explicitPath ??
    (runner.startsWith("codex-")
      ? bundledCodexPath()
      : runner.startsWith("claude-")
        ? bundledClaudePath()
        : fail("probe runner is unsupported"));
  if (!isAbsolute(executable)) fail("probe executable path must be absolute");
  const version = spawnSync(executable, ["--version"], {
    encoding: "utf8",
    env: process.env,
    timeout: 10_000,
    maxBuffer: 64 * 1024,
    windowsHide: true,
  });
  if (version.error || version.status !== 0) {
    throw new Error(
      `vendor runtime version probe failed: ${version.error?.message ?? version.stderr.trim()}`,
    );
  }
  if (runner.startsWith("codex-")) {
    const auth = spawnSync(executable, ["login", "status"], {
      encoding: "utf8",
      env: process.env,
      timeout: 10_000,
      maxBuffer: 64 * 1024,
      windowsHide: true,
    });
    const authText = `${auth.stdout}\n${auth.stderr}`;
    const launcherPath = realpathSync(executable);
    return {
      launcherPath,
      executablePath: installedCodexNativePath(launcherPath),
      version: version.stdout.trim(),
      authenticated: auth.status === 0 && authText.includes("Logged in"),
      authenticationMethod: authText.includes("ChatGPT") ? "chatgpt" : "vendor-local-session",
    };
  }
  const auth = spawnSync(executable, ["auth", "status", "--json"], {
    encoding: "utf8",
    env: process.env,
    timeout: 10_000,
    maxBuffer: 64 * 1024,
    windowsHide: true,
  });
  let parsed = {};
  try {
    parsed = JSON.parse(auth.stdout);
  } catch {
    // A malformed status response is an unauthenticated, fail-closed result.
  }
  return {
    launcherPath: realpathSync(executable),
    executablePath: realpathSync(executable),
    version: version.stdout.trim(),
    authenticated: auth.status === 0 && parsed.loggedIn === true,
    authenticationMethod:
      typeof parsed.authMethod === "string" ? parsed.authMethod : "vendor-local-session",
  };
}

export async function runCodex(request, sdkLoader = () => import("@openai/codex-sdk")) {
  const { Codex } = await sdkLoader();
  if (process.platform === "win32") {
    throw new Error("the isolated Codex SDK wrapper is not yet available on Windows");
  }
  const realCodex = request.executablePath ?? bundledCodexPath();
  const wrapper = fileURLToPath(new URL("./codex-wrapper.mjs", import.meta.url));
  const codex = new Codex({
    codexPathOverride: wrapper,
    env: { ...process.env, TT_CODEX_REAL_PATH: realCodex },
    config: buildCodexConfig(request),
  });
  const threadOptions = buildCodexThreadOptions(request);
  const thread = request.sessionId
    ? codex.resumeThread(request.sessionId, threadOptions)
    : codex.startThread(threadOptions);
  const streamed = await thread.runStreamed(codexPrompt(request));

  let sessionId = request.sessionId ?? null;
  let response = "";
  let usage = null;
  let failure = null;
  let toolCalls = 0;
  try {
    for await (const event of streamed.events) {
      if (event.type === "thread.started") sessionId = event.thread_id;
      if (event.type === "item.completed") {
        if (event.item?.type === "agent_message") response = event.item.text;
        if (event.item?.type === "mcp_tool_call") {
          toolCalls += 1;
          if (event.item.error?.message) {
            failure = `MCP ${event.item.server}/${event.item.tool}: ${event.item.error.message}`;
          }
        }
      }
      if (event.type === "turn.completed") usage = normalizedUsage(event.usage);
      if (event.type === "turn.failed") failure = event.error?.message ?? "Codex turn failed";
      if (event.type === "error") failure = event.message;
    }
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error);
    failure = failure === null ? detail : `${failure}; ${detail}`;
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

export async function runClaude(request, sdkLoader = () => import("@anthropic-ai/claude-agent-sdk")) {
  const { query } = await sdkLoader();
  let result = null;
  let toolCalls = 0;
  for await (const message of query({
    prompt: request.prompt,
    options: buildClaudeOptions(request),
  })) {
    if (message.type === "assistant") {
      const content = Array.isArray(message.message?.content) ? message.message.content : [];
      toolCalls += content.filter((block) => block?.type === "tool_use").length;
    }
    if (message.type === "result") result = message;
  }
  if (result === null) throw new Error("Claude Agent SDK ended without a result message");

  const ok = result.subtype === "success" && result.is_error === false;
  return {
    ok,
    runner: request.runner,
    sessionId: result.session_id,
    response: result.subtype === "success" ? result.result : "",
    usage: normalizedUsage(result.usage),
    totalCostUsd: Number.isFinite(result.total_cost_usd) ? result.total_cost_usd : null,
    toolCalls,
    ...(ok
      ? {}
      : {
          error: Array.isArray(result.errors)
            ? result.errors.join("; ")
            : `Claude Agent SDK stopped with ${result.subtype}`,
        }),
  };
}

async function readRequest() {
  const chunks = [];
  for await (const chunk of process.stdin) chunks.push(chunk);
  const raw = Buffer.concat(chunks).toString("utf8");
  if (raw.trim() === "") fail("stdin is empty");
  try {
    return validateRequest(JSON.parse(raw));
  } catch (error) {
    if (error instanceof SyntaxError) fail(`stdin is not JSON: ${error.message}`);
    throw error;
  }
}
async function probe(runner, explicitPath) {
  if (runner === "codex-sdk") {
    await import("@openai/codex-sdk");
  } else if (runner === "claude-agent-sdk") {
    await import("@anthropic-ai/claude-agent-sdk");
  } else if (!RUNNERS.has(runner)) {
    fail("probe runner is unsupported");
  }
  return {
    ok: true,
    nodeVersion: process.versions.node,
    compatibility: SDK_COMPATIBILITY,
    runtime: probeRuntime(runner, explicitPath),
  };
}


async function main() {
  try {
    let result;
    if (process.argv[2] === "--probe") {
      result = await probe(process.argv[3], process.argv[4]);
    } else {
      const request = await readRequest();
      const executable =
        request.executablePath ??
        (request.runner.startsWith("codex-") ? bundledCodexPath() : bundledClaudePath());
      if (request.runner === "codex-sdk") {
        result = await runCodex(request);
      } else if (request.runner === "claude-agent-sdk") {
        result = await runClaude(request);
      } else if (request.runner === "codex-cli") {
        result = await runCodexCli(request, executable);
      } else {
        result = await runClaudeCli(request, executable);
      }
    }
    process.stdout.write(`${JSON.stringify(result)}\n`);
    if (!result.ok) process.exitCode = 1;
  } catch (error) {
    process.stdout.write(
      `${JSON.stringify({
        ok: false,
        error: error instanceof Error ? error.message : String(error),
      })}\n`,
    );
    process.exitCode = 1;
  }
}

const entry = process.argv[1] === undefined ? null : pathToFileURL(resolve(process.argv[1])).href;
if (entry === import.meta.url) await main();
