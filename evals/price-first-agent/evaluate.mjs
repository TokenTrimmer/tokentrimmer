#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  existsSync,
  mkdtempSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const publicRoot = resolve(here, "../..");

function parseArgs(argv) {
  const options = {
    corpus: join(here, "corpus.v1.json"),
    ttBin: join(publicRoot, "target/debug/tt"),
    output: null,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const value = argv[index];
    const next = argv[index + 1];
    if (value === "--corpus" && next) options.corpus = resolve(next);
    else if (value === "--tt-bin" && next) options.ttBin = resolve(next);
    else if (value === "--output" && next) options.output = resolve(next);
    else throw new Error(`unknown or incomplete argument: ${value}`);
    index += 1;
  }
  return options;
}

function sha256(value) {
  return createHash("sha256").update(value).digest("hex");
}

function writeTree(root, files) {
  for (const [path, content] of Object.entries(files)) {
    const target = join(root, path);
    mkdirSync(dirname(target), { recursive: true });
    writeFileSync(target, content, { encoding: "utf8", mode: 0o644 });
  }
}

function fileHashes(root, files) {
  return Object.fromEntries(
    Object.keys(files)
      .sort()
      .map((path) => [path, sha256(readFileSync(join(root, path)))]),
  );
}

function policyToml(corpus) {
  const models = Object.values(corpus.models).map((model) => `"${model}"`).join(", ");
  return `schema_version = 1

[filesystem]
readable_roots = ["."]
writable_roots = ["src"]
max_files = 100
max_file_bytes = 200000
max_total_read_bytes = 2000000
max_total_write_bytes = 1000000
allow_symlinks = false
excluded_paths = [".git/**", ".tokentrimmer/**", ".env*", "**/*.pem", "**/*.key"]

[process]
allowed_commands = []
max_subprocesses = 0
max_duration_seconds = 0
max_output_bytes = 2000000
allow_shell = false

[network]
default = "deny"
allowed_destinations = []
allow_redirects = false
inherit_proxy_env = false

[inference]
allowed_runners = ["codex_sdk"]
allowed_providers = ["openai"]
allowed_models = [${models}]
allowed_cost_bases = ["subscription"]

[limits]
max_api_calls = 12
max_model_turns = 12
max_retries = 0
max_wall_time_seconds = 180
max_diff_bytes = 250000
max_changed_files = 8

[budgets]
max_api_cash_micros = 0
max_subscription_marginal_cash_micros = 0
max_subscription_allocated_micros = ${corpus.allocated_plan_micros_per_run}
max_self_hosted_tco_micros = 0
subscription_quota_caps = [
  { unit = "requests", max_units = 1 },
  { unit = "tokens", max_units = 200000 },
  { unit = "tool_calls", max_units = 11 },
]
allow_unmeasured = true

[approvals]
destructive_operations = "deny"
rollback = "deny"

[validation]
required_commands = []
stop_on_regression = true
`;
}

function runCommand(executable, args, options = {}) {
  const started = performance.now();
  const result = spawnSync(executable, args, {
    cwd: options.cwd,
    input: options.input,
    encoding: "utf8",
    env: options.env ?? process.env,
    maxBuffer: options.maxBuffer ?? 4 * 1024 * 1024,
    timeout: options.timeout ?? 60_000,
    windowsHide: true,
  });
  return {
    status: result.status,
    signal: result.signal,
    error: result.error?.message ?? null,
    stdout: result.stdout ?? "",
    stderr: result.stderr ?? "",
    duration_ms: Math.round(performance.now() - started),
  };
}

function costSummary(cost) {
  const summary = {
    marginal_cash_micros: 0,
    allocated_plan_micros: 0,
    api_metered_micros: 0,
    self_hosted_tco_micros: 0,
    measured_components: 0,
    unmeasured_components: 0,
    total_components: cost.components.length,
  };
  for (const component of cost.components) {
    const value = component.cost;
    if (value.basis === "subscription") {
      summary.marginal_cash_micros += value.marginal_cash_micros;
      summary.allocated_plan_micros += value.allocated_plan_micros ?? 0;
      summary.measured_components += 1;
    } else if (value.basis === "api_metered") {
      summary.api_metered_micros += value.amount_micros;
      summary.measured_components += 1;
    } else if (value.basis === "self_hosted") {
      summary.self_hosted_tco_micros +=
        (value.energy_micros ?? 0) +
        (value.hardware_amortization_micros ?? 0) +
        (value.hosting_micros ?? 0) +
        (value.operator_micros ?? 0);
      summary.measured_components += 1;
    } else {
      summary.unmeasured_components += 1;
    }
  }
  return summary;
}

function validatePatch(task, report, sourceRoot) {
  const changedPaths = report.broker.patch.changes.map((change) => change.path).sort();
  const allowed = [...task.allowed_changed_paths].sort();
  const unauthorized = changedPaths.filter((path) => !allowed.includes(path));
  const validationRoot = mkdtempSync(join(tmpdir(), "tt-agent-eval-validate-"));
  try {
    writeTree(validationRoot, task.files);
    const applied = runCommand(
      "git",
      ["apply", "--whitespace=nowarn", "-"],
      {
        cwd: validationRoot,
        input: report.broker.patch.unified_diff,
        timeout: 30_000,
      },
    );
    const checks = [];
    if (applied.status === 0) {
      for (const command of task.validation) {
        const result = runCommand(command.executable, command.args, {
          cwd: validationRoot,
          timeout: 60_000,
        });
        checks.push({
          executable: command.executable,
          args: command.args,
          passed: result.status === 0 && result.signal === null && result.error === null,
          exit_code: result.status,
          signal: result.signal,
          duration_ms: result.duration_ms,
          stdout_sha256: sha256(result.stdout),
          stderr_sha256: sha256(result.stderr),
        });
        if (!checks.at(-1).passed) break;
      }
    }
    return {
      accepted:
        report.status === "completed" &&
        report.broker.source_checkout_modified === false &&
        unauthorized.length === 0 &&
        changedPaths.length > 0 &&
        applied.status === 0 &&
        checks.length === task.validation.length &&
        checks.every((check) => check.passed),
      changed_paths: changedPaths,
      unauthorized_paths: unauthorized,
      patch_applied: applied.status === 0,
      patch_apply_error_sha256: applied.status === 0 ? null : sha256(applied.stderr),
      checks,
      source_checkout_unchanged:
        JSON.stringify(fileHashes(sourceRoot, task.files)) ===
        JSON.stringify(Object.fromEntries(Object.keys(task.files).sort().map((path) => [path, sha256(task.files[path])]))),
    };
  } finally {
    rmSync(validationRoot, { recursive: true, force: true });
  }
}

function evaluateRun(ttBin, corpus, task, strategy, model) {
  const root = mkdtempSync(join(tmpdir(), "tt-agent-eval-run-"));
  const receipt = `${root}-receipt.json`;
  try {
    writeTree(root, task.files);
    mkdirSync(join(root, ".tokentrimmer"), { recursive: true });
    writeFileSync(join(root, ".tokentrimmer", "agent.toml"), policyToml(corpus), "utf8");
    const before = fileHashes(root, task.files);
    const invocation = runCommand(
      ttBin,
      [
        "code",
        "run",
        task.prompt,
        "--runner",
        corpus.runner,
        "--model",
        model,
        "--repository",
        root,
        "--allocated-plan-micros",
        String(corpus.allocated_plan_micros_per_run),
        "--receipt",
        receipt,
      ],
      { cwd: publicRoot, timeout: 240_000, maxBuffer: 32 * 1024 * 1024 },
    );
    if (!existsSync(receipt)) {
      throw new Error(
        `${task.id}/${strategy} produced no terminal receipt: ${invocation.error ?? invocation.stderr}`,
      );
    }
    const receiptBytes = readFileSync(receipt);
    const report = JSON.parse(receiptBytes);
    const after = fileHashes(root, task.files);
    if (JSON.stringify(before) !== JSON.stringify(after)) {
      throw new Error(`${task.id}/${strategy} modified the source checkout`);
    }
    const validation = validatePatch(task, report, root);
    const costs = costSummary(report.cost);
    return {
      task_id: task.id,
      strategy,
      model,
      accepted: validation.accepted,
      invocation_exit_code: invocation.status,
      stop_reason: report.stop_reason,
      failure: report.failure,
      duration_ms: report.duration_ms,
      input_tokens: report.usage.inputTokens,
      cached_input_tokens: report.usage.cachedInputTokens,
      output_tokens: report.usage.outputTokens,
      total_tokens: report.usage.inputTokens + report.usage.outputTokens,
      tool_actions_requested: report.actions.model_actions_requested,
      tool_actions_denied: report.actions.model_actions_denied,
      policy_denials: report.broker.policy_decisions.filter((decision) => !decision.allowed).length,
      retry_count: 0,
      denied_actions: report.broker.policy_decisions
        .filter((decision) => !decision.allowed)
        .map((decision) => ({
          tool: decision.tool,
          reason_code: decision.reason_code,
          action_sha256: decision.action_sha256,
        })),
      rollback: report.cleanup.rollback,
      receipt_reasons: report.receipt.reasons,
      costs,
      validation,
      receipt_sha256: sha256(receiptBytes),
      patch_sha256: sha256(report.broker.patch.unified_diff),
      runtime: {
        sdk_version: report.runtime.sdk_version,
        runtime_version: report.runtime.runtime_version,
        runtime_executable_sha256: report.runtime.runtime_executable_sha256,
      },
    };
  } finally {
    rmSync(root, { recursive: true, force: true });
    rmSync(receipt, { force: true });
  }
}

function percentile(values, probability) {
  if (values.length === 0) return null;
  const sorted = [...values].sort((left, right) => left - right);
  return sorted[Math.max(0, Math.ceil(probability * sorted.length) - 1)];
}

function aggregate(name, runs) {
  const accepted = runs.filter((run) => run.accepted).length;
  const totals = runs.reduce(
    (sum, run) => {
      sum.duration_ms += run.duration_ms;
      sum.total_tokens += run.total_tokens;
      sum.input_tokens += run.input_tokens;
      sum.cached_input_tokens += run.cached_input_tokens;
      sum.output_tokens += run.output_tokens;
      sum.failed_runs += run.stop_reason === "completed" ? 0 : 1;
      sum.retry_count += run.retry_count;
      sum.rollback_count += run.rollback === "not_required_source_never_mutated" ? 0 : 1;
      sum.marginal_cash_micros += run.costs.marginal_cash_micros;
      sum.allocated_plan_micros += run.costs.allocated_plan_micros;
      sum.api_metered_micros += run.costs.api_metered_micros;
      sum.self_hosted_tco_micros += run.costs.self_hosted_tco_micros;
      sum.unmeasured_components += run.costs.unmeasured_components;
      sum.total_components += run.costs.total_components;
      sum.policy_denials += run.policy_denials;
      sum.tool_actions_denied += run.tool_actions_denied;
      return sum;
    },
    {
      duration_ms: 0,
      total_tokens: 0,
      input_tokens: 0,
      cached_input_tokens: 0,
      output_tokens: 0,
      failed_runs: 0,
      retry_count: 0,
      rollback_count: 0,
      marginal_cash_micros: 0,
      allocated_plan_micros: 0,
      api_metered_micros: 0,
      self_hosted_tco_micros: 0,
      unmeasured_components: 0,
      total_components: 0,
      policy_denials: 0,
      tool_actions_denied: 0,
    },
  );
  return {
    strategy: name,
    tasks: runs.length,
    accepted_patches: accepted,
    accepted_patch_rate: runs.length === 0 ? null : accepted / runs.length,
    p50_latency_ms: percentile(runs.map((run) => run.duration_ms), 0.5),
    p95_latency_ms: percentile(runs.map((run) => run.duration_ms), 0.95),
    marginal_cash_micros_per_accepted_patch:
      accepted === 0 ? null : totals.marginal_cash_micros / accepted,
    allocated_plan_micros_per_accepted_patch:
      accepted === 0 ? null : totals.allocated_plan_micros / accepted,
    tokens_per_accepted_patch: accepted === 0 ? null : totals.total_tokens / accepted,
    cache_read_fraction:
      totals.input_tokens === 0 ? null : totals.cached_input_tokens / totals.input_tokens,
    failed_runs: totals.failed_runs,
    retry_count: totals.retry_count,
    rollback_count: totals.rollback_count,
    human_interventions_observed: null,
    api_metered_micros_total: totals.api_metered_micros,
    self_hosted_tco_micros_total: totals.self_hosted_tco_micros,
    unmeasured_component_rate:
      totals.total_components === 0
        ? null
        : totals.unmeasured_components / totals.total_components,
    policy_denials: totals.policy_denials,
    tool_actions_denied: totals.tool_actions_denied,
  };
}

function synthesizePriceFirst(task, cheap, premium) {
  const selected = cheap.accepted ? cheap : premium;
  const runs = cheap.accepted ? [cheap] : [cheap, premium];
  const combinedCosts = runs.reduce(
    (sum, run) => {
      for (const key of [
        "marginal_cash_micros",
        "allocated_plan_micros",
        "api_metered_micros",
        "self_hosted_tco_micros",
        "measured_components",
        "unmeasured_components",
        "total_components",
      ]) {
        sum[key] += run.costs[key];
      }
      return sum;
    },
    {
      marginal_cash_micros: 0,
      allocated_plan_micros: 0,
      api_metered_micros: 0,
      self_hosted_tco_micros: 0,
      measured_components: 0,
      unmeasured_components: 0,
      total_components: 0,
    },
  );
  return {
    task_id: task.id,
    strategy: "price_first",
    model: cheap.accepted ? cheap.model : `${cheap.model} -> ${premium.model}`,
    accepted: selected.accepted,
    escalated: !cheap.accepted,
    invocation_exit_code: selected.invocation_exit_code,
    stop_reason: selected.stop_reason,
    failure: selected.failure,
    duration_ms: runs.reduce((sum, run) => sum + run.duration_ms, 0),
    input_tokens: runs.reduce((sum, run) => sum + run.input_tokens, 0),
    cached_input_tokens: runs.reduce((sum, run) => sum + run.cached_input_tokens, 0),
    output_tokens: runs.reduce((sum, run) => sum + run.output_tokens, 0),
    total_tokens: runs.reduce((sum, run) => sum + run.total_tokens, 0),
    tool_actions_requested: runs.reduce((sum, run) => sum + run.tool_actions_requested, 0),
    tool_actions_denied: runs.reduce((sum, run) => sum + run.tool_actions_denied, 0),
    policy_denials: runs.reduce((sum, run) => sum + run.policy_denials, 0),
    retry_count: runs.reduce((sum, run) => sum + run.retry_count, 0),
    denied_actions: runs.flatMap((run) => run.denied_actions),
    rollback: selected.rollback,
    receipt_reasons: selected.receipt_reasons,
    costs: combinedCosts,
    validation: selected.validation,
    receipt_sha256: selected.receipt_sha256,
    patch_sha256: selected.patch_sha256,
    runtime: selected.runtime,
  };
}

function main() {
  const options = parseArgs(process.argv.slice(2));
  const corpusBytes = readFileSync(options.corpus);
  const corpus = JSON.parse(corpusBytes);
  if (corpus.schema_version !== 1 || !Array.isArray(corpus.tasks) || corpus.tasks.length === 0) {
    throw new Error("unsupported or empty evaluation corpus");
  }
  if (!existsSync(options.ttBin)) throw new Error(`tt binary not found: ${options.ttBin}`);

  const version = runCommand(options.ttBin, ["--version"], { cwd: publicRoot, timeout: 30_000 });
  if (version.status !== 0) throw new Error(`tt version probe failed: ${version.stderr}`);
  const revision = runCommand("git", ["rev-parse", "HEAD"], {
    cwd: publicRoot,
    timeout: 30_000,
  });
  const runs = [];
  for (const task of corpus.tasks) {
    process.stderr.write(`Evaluating ${task.id} with cheap-only model\n`);
    const cheap = evaluateRun(
      options.ttBin,
      corpus,
      task,
      "cheap_only",
      corpus.models.cheap_only,
    );
    process.stderr.write(`Evaluating ${task.id} with fixed-premium model\n`);
    const premium = evaluateRun(
      options.ttBin,
      corpus,
      task,
      "fixed_premium",
      corpus.models.fixed_premium,
    );
    runs.push(cheap, premium, synthesizePriceFirst(task, cheap, premium));
  }

  const cheapRuns = runs.filter((run) => run.strategy === "cheap_only");
  const premiumRuns = runs.filter((run) => run.strategy === "fixed_premium");
  const priceFirstRuns = runs.filter((run) => run.strategy === "price_first");
  const result = {
    schema_version: 1,
    corpus_id: corpus.id,
    corpus_sha256: sha256(corpusBytes),
    evaluated_at: new Date().toISOString(),
    repository_revision: revision.status === 0 ? revision.stdout.trim() : null,
    tt_version: version.stdout.trim().split("\n").at(-1),
    authorization: {
      comparative_product_claims: false,
      reason:
        "Small local subscription-plan corpus with no provider invoice, no API-metered cash baseline, and no independent human patch review.",
    },
    accounting: {
      marginal_cash_is_realized: true,
      allocated_plan_cost_is_user_configured: true,
      token_counts_are_quota_not_cash: true,
      api_equivalent_cost_available: false,
      human_patch_review_performed: false,
      secret_exposure_cases_in_corpus: 0,
    },
    strategies: [
      aggregate("cheap_only", cheapRuns),
      aggregate("fixed_premium", premiumRuns),
      {
        ...aggregate("price_first", priceFirstRuns),
        escalations: priceFirstRuns.filter((run) => run.escalated).length,
      },
    ],
    runs,
    limitations: [
      "Three deterministic repair tasks are not representative of production repositories.",
      "Acceptance is automated by source-controlled behavioral checks; no blinded human review was run.",
      "Codex ChatGPT-plan usage exposes tokens but no invoice-reconciled per-run cash amount.",
      "Only one vendor, one hardware platform, and one run per task/model were measured.",
      "The corpus contains no planted secret-exposure case, so secret exposure remains unmeasured.",
      "Results authorize no cheaper-or-equal-quality claim.",
    ],
  };
  const encoded = `${JSON.stringify(result, null, 2)}\n`;
  if (options.output) writeFileSync(options.output, encoded, "utf8");
  process.stdout.write(encoded);
}

try {
  main();
} catch (error) {
  process.stderr.write(`${error instanceof Error ? error.stack : String(error)}\n`);
  process.exitCode = 1;
}
