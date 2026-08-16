import assert from "node:assert/strict";
import test from "node:test";

import {
  buildClaudeOptions,
  buildCodexConfig,
  buildCodexThreadOptions,
  runClaude,
  runCodex,
  validateRequest,
} from "./bridge.mjs";
import {
  buildClaudeCliArgs,
  buildClaudeMcpConfig,
  buildCodexCliArgs,
  codexCliPrompt,
  parseClaudeCliOutput,
  parseCodexCliOutput,
} from "./structured-cli.mjs";

function request(overrides = {}) {
  return {
    runner: "codex-sdk",
    prompt: "Fix the bug",
    cwd: "/tmp/workspace",
    maxTurns: 8,
    maxBudgetUsd: 1.25,
    maxOutputBytes: 1_000_000,
    mcp: {
      url: "http://127.0.0.1:43123/mcp",
      token: "0123456789abcdef0123456789abcdef",
      tools: ["read_repo", "write_repo"],
    },
    ...overrides,
  };
}

test("validates the loopback-only broker boundary", () => {
  assert.equal(validateRequest(request()).runner, "codex-sdk");
  assert.throws(
    () =>
      validateRequest(
        request({
          mcp: {
            ...request().mcp,
            url: "https://broker.example.com/mcp",
          },
        }),
      ),
    /http:\/\/127\.0\.0\.1/,
  );
  assert.throws(() => validateRequest(request({ cwd: "relative" })), /absolute path/);
  assert.throws(
    () =>
      validateRequest(
        request({
          mcp: {
            ...request().mcp,
            tools: ["read_repo", "read_repo"],
          },
        }),
      ),
    /duplicate MCP tool name/,
  );
});

test("Codex config disables built-ins and requires the scoped MCP broker", () => {
  const req = request();
  const config = buildCodexConfig(req);
  const thread = buildCodexThreadOptions(req);

  assert.deepEqual(config.features, { shell_tool: false, unified_exec: false });
  assert.equal(config.web_search, "disabled");
  assert.equal(config.tools.web_search, false);
  assert.deepEqual(config.mcp_servers.tokentrimmer.enabled_tools, req.mcp.tools);
  assert.equal(config.mcp_servers.tokentrimmer.required, true);
  assert.equal(config.mcp_servers.tokentrimmer.default_tools_approval_mode, "approve");
  assert.equal(
    config.mcp_servers.tokentrimmer.http_headers.Authorization,
    `Bearer ${req.mcp.token}`,
  );
  assert.equal(thread.sandboxMode, "read-only");
  assert.equal(thread.networkAccessEnabled, false);
  assert.equal(thread.approvalPolicy, "never");
});

test("Claude exposes exactly the scoped MCP tools without loading user settings", () => {
  const req = request({
    runner: "claude-agent-sdk",
    model: "claude-sonnet-4-5",
    sessionId: "session-123",
    executablePath: "/opt/bin/claude",
  });
  const options = buildClaudeOptions(req, { PATH: "/usr/bin" });

  assert.deepEqual(options.tools, [
    "mcp__tokentrimmer__read_repo",
    "mcp__tokentrimmer__write_repo",
  ]);
  assert.deepEqual(options.allowedTools, options.tools);
  assert.deepEqual(options.settingSources, []);
  assert.equal(options.strictMcpConfig, true);
  assert.equal(options.permissionMode, "dontAsk");
  assert.equal(options.resume, "session-123");
  assert.equal(options.pathToClaudeCodeExecutable, "/opt/bin/claude");
  assert.equal(options.env.CLAUDE_AGENT_SDK_CLIENT_APP, "tokentrimmer");
  assert.equal(options.mcpServers.tokentrimmer.headers.Authorization, `Bearer ${req.mcp.token}`);
});

test("Codex adapter consumes official structured events", async () => {
  const captured = {};
  class Codex {
    constructor(options) {
      captured.codex = options;
    }

    startThread(options) {
      captured.thread = options;
      return {
        async runStreamed(prompt) {
          captured.prompt = prompt;
          return {
            events: (async function* () {
              yield { type: "thread.started", thread_id: "codex-thread" };
              yield {
                type: "item.completed",
                item: { type: "mcp_tool_call", status: "completed" },
              };
              yield {
                type: "item.completed",
                item: { type: "agent_message", text: "done" },
              };
              yield {
                type: "turn.completed",
                usage: { input_tokens: 12, cached_input_tokens: 4, output_tokens: 3 },
              };
            })(),
          };
        },
      };
    }
  }

  const outcome = await runCodex(request(), async () => ({ Codex }));
  assert.equal(outcome.ok, true);
  assert.equal(outcome.sessionId, "codex-thread");
  assert.equal(outcome.response, "done");
  assert.equal(outcome.toolCalls, 1);
  assert.equal(outcome.usage.inputTokens, 12);
  assert.equal(captured.thread.sandboxMode, "read-only");
  assert.match(captured.prompt, /Use only the tokentrimmer MCP tools/);
});

test("Claude adapter consumes the terminal SDK result without reclassifying cost", async () => {
  let captured;
  async function* query(input) {
    captured = input;
    yield {
      type: "assistant",
      message: { content: [{ type: "tool_use", name: "mcp__tokentrimmer__read_repo" }] },
    };
    yield {
      type: "result",
      subtype: "success",
      is_error: false,
      session_id: "claude-session",
      result: "done",
      usage: { input_tokens: 20, cache_read_input_tokens: 5, output_tokens: 4 },
      total_cost_usd: 0.02,
    };
  }

  const req = request({ runner: "claude-agent-sdk" });
  const outcome = await runClaude(req, async () => ({ query }));
  assert.equal(outcome.ok, true);
  assert.equal(outcome.sessionId, "claude-session");
  assert.equal(outcome.totalCostUsd, 0.02);
  assert.equal(outcome.toolCalls, 1);
  assert.equal(outcome.usage.cachedInputTokens, 5);
  assert.deepEqual(captured.options.tools, [
    "mcp__tokentrimmer__read_repo",
    "mcp__tokentrimmer__write_repo",
  ]);
});

test("structured CLI fallback arguments preserve the broker-only boundary", () => {
  const codex = request({ runner: "codex-cli", model: "gpt-5.4-mini" });
  const codexArgs = buildCodexCliArgs(codex);
  assert.equal(validateRequest(codex).runner, "codex-cli");
  assert.deepEqual(codexArgs.slice(0, 3), ["exec", "--json", "--color"]);
  assert.ok(codexArgs.includes("--ignore-user-config"));
  assert.ok(codexArgs.includes("--ignore-rules"));
  assert.ok(codexArgs.includes("features.shell_tool=false"));
  assert.ok(codexArgs.includes("features.unified_exec=false"));
  assert.ok(codexArgs.includes("mcp_servers.tokentrimmer.required=true"));
  assert.ok(
    codexArgs.includes(
      'mcp_servers.tokentrimmer.bearer_token_env_var="TT_TOKENTRIMMER_MCP_TOKEN"',
    ),
  );
  assert.equal(codexArgs.join("\n").includes(codex.mcp.token), false);

  assert.match(codexCliPrompt(codex), /Use only the tokentrimmer MCP tools/);
  const claude = request({
    runner: "claude-cli",
    model: "claude-sonnet-4-5",
    sessionId: "session-123",
  });
  const claudeArgs = buildClaudeCliArgs(claude, "/tmp/mcp.json");
  assert.equal(validateRequest(claude).runner, "claude-cli");
  assert.ok(claudeArgs.includes("--setting-sources"));
  assert.ok(claudeArgs.includes("--strict-mcp-config"));
  assert.ok(claudeArgs.includes("--disable-slash-commands"));
  assert.ok(claudeArgs.includes("dontAsk"));
  assert.ok(claudeArgs.includes("mcp__tokentrimmer__read_repo,mcp__tokentrimmer__write_repo"));
  assert.equal(claudeArgs.join("\n").includes(claude.mcp.token), false);
  assert.equal(
    buildClaudeMcpConfig(claude).mcpServers.tokentrimmer.headers.Authorization,
    `Bearer ${claude.mcp.token}`,
  );
});

test("Codex CLI fallback consumes JSONL and fails closed on built-in tools", () => {
  const req = request({ runner: "codex-cli" });
  const success = parseCodexCliOutput(req, {
    code: 0,
    stderr: "",
    stdout: [
      JSON.stringify({ type: "thread.started", thread_id: "codex-cli-session" }),
      JSON.stringify({
        type: "item.completed",
        item: {
          type: "mcp_tool_call",
          server: "tokentrimmer",
          tool: "read_repo",
          error: null,
        },
      }),
      JSON.stringify({
        type: "item.completed",
        item: { type: "agent_message", text: "done" },
      }),
      JSON.stringify({
        type: "turn.completed",
        usage: { input_tokens: 12, cached_input_tokens: 5, output_tokens: 3 },
      }),
    ].join("\n"),
  });
  assert.equal(success.ok, true);
  assert.equal(success.sessionId, "codex-cli-session");
  assert.equal(success.toolCalls, 1);
  assert.equal(success.usage.cachedInputTokens, 5);

  const denied = parseCodexCliOutput(req, {
    code: 0,
    stderr: "",
    stdout: [
      JSON.stringify({
        type: "item.completed",
        item: { type: "command_execution", status: "completed" },
      }),
      JSON.stringify({ type: "turn.completed", usage: {} }),
    ].join("\n"),
  });
  assert.equal(denied.ok, false);
  assert.match(denied.error, /unauthorized built-in tool/);
});

test("Claude CLI fallback consumes stream-json and rejects foreign tools", () => {
  const req = request({ runner: "claude-cli" });
  const success = parseClaudeCliOutput(req, {
    code: 0,
    stderr: "",
    stdout: [
      JSON.stringify({
        type: "assistant",
        message: {
          content: [{ type: "tool_use", name: "mcp__tokentrimmer__write_repo" }],
        },
      }),
      JSON.stringify({
        type: "result",
        subtype: "success",
        is_error: false,
        session_id: "claude-cli-session",
        result: "done",
        usage: { input_tokens: 20, cache_read_input_tokens: 7, output_tokens: 4 },
        total_cost_usd: 0.02,
      }),
    ].join("\n"),
  });
  assert.equal(success.ok, true);
  assert.equal(success.sessionId, "claude-cli-session");
  assert.equal(success.toolCalls, 1);
  assert.equal(success.totalCostUsd, 0.02);

  const denied = parseClaudeCliOutput(req, {
    code: 0,
    stderr: "",
    stdout: [
      JSON.stringify({
        type: "assistant",
        message: { content: [{ type: "tool_use", name: "Bash" }] },
      }),
      JSON.stringify({
        type: "result",
        subtype: "success",
        is_error: false,
        session_id: "claude-cli-session",
        result: "done",
        usage: {},
      }),
    ].join("\n"),
  });
  assert.equal(denied.ok, false);
  assert.match(denied.error, /unauthorized tool Bash/);
});
