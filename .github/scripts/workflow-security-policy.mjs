import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const REPO_ROOT = join(fileURLToPath(new URL("../..", import.meta.url)));
const FULL_GIT_SHA = /^[0-9a-f]{40}$/i;
const OCI_DIGEST = /@sha256:[0-9a-f]{64}$/i;
const DEPLOY_WORKFLOW_PATH = ".github/workflows/deploy.yml";
const CI_WORKFLOW_PATH = ".github/workflows/ci.yml";
const DEPLOY_SECRETS = [
  "FLY_API_TOKEN",
  "STAGING_DATABASE_URL_DIRECT",
  "PROD_DATABASE_URL_DIRECT",
];

function collectYamlFiles(directory) {
  if (!existsSync(directory)) return [];

  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) return collectYamlFiles(path);
    return /\.ya?ml$/i.test(entry.name) ? [path] : [];
  });
}

function lineForOffset(text, offset) {
  return text.slice(0, offset).split("\n").length;
}

/**
 * The deployment containment policy needs a few structural YAML assertions in
 * addition to actionlint. Keep this deliberately small and fail closed: these
 * workflows use ordinary block mappings, so accepting a more exotic YAML form
 * would weaken the policy rather than make it more useful.
 */
function yamlLines(text) {
  return text.split(/\r?\n/).map((raw, index) => {
    let quote = null;
    let clean = raw;

    for (let cursor = 0; cursor < raw.length; cursor += 1) {
      const character = raw[cursor];
      if (quote) {
        if (character === quote) {
          // YAML single quotes escape themselves by doubling. The workflows do
          // not rely on this today, but recognising it avoids a misleading
          // comment boundary if one is added to a condition later.
          if (quote === "'" && raw[cursor + 1] === "'") {
            cursor += 1;
          } else {
            quote = null;
          }
        }
        continue;
      }
      if (character === "'" || character === '"') {
        quote = character;
      } else if (character === "#" && (cursor === 0 || /\s/.test(raw[cursor - 1]))) {
        clean = raw.slice(0, cursor);
        break;
      }
    }

    const match = /^([ ]*)([A-Za-z0-9_-]+):(?:[ ]*(.*))?$/.exec(clean);
    return {
      raw: clean,
      number: index + 1,
      indent: match ? match[1].length : null,
      key: match?.[2] ?? null,
      value: match?.[3]?.trim() ?? null,
    };
  });
}

function blockEnd(lines, entry) {
  for (let index = entry.index + 1; index < lines.length; index += 1) {
    const candidate = lines[index];
    if (!candidate.raw.trim()) continue;
    const indentation = candidate.raw.match(/^ */)?.[0].length ?? 0;
    if (indentation <= entry.indent) return index;
  }
  return lines.length;
}

function mappingEntries(lines, parent) {
  const entries = [];
  for (let index = parent.index + 1; index < blockEnd(lines, parent); index += 1) {
    const candidate = lines[index];
    if (candidate.key && candidate.indent === parent.indent + 2) {
      entries.push({ ...candidate, index });
    }
  }
  return entries;
}

function rootEntry(lines, key) {
  for (let index = 0; index < lines.length; index += 1) {
    const candidate = lines[index];
    if (candidate.key === key && candidate.indent === 0) {
      return { ...candidate, index };
    }
  }
  return null;
}

function directEntry(lines, parent, key) {
  return mappingEntries(lines, parent).find((entry) => entry.key === key) ?? null;
}

function sameKeys(entries, expected) {
  const actual = new Set(entries.map((entry) => entry.key));
  return (
    entries.length === expected.length
    && actual.size === expected.length
    && expected.every((key) => actual.has(key))
  );
}

function sourceBlock(lines, entry) {
  return lines
    .slice(entry.index, blockEnd(lines, entry))
    .map((line) => line.raw)
    .join("\n");
}

function validateReadOnlyPermissions(lines, job, label, errors) {
  const permissions = directEntry(lines, job, "permissions");
  const entries = permissions ? mappingEntries(lines, permissions) : [];
  if (!permissions || permissions.value || !sameKeys(entries, ["contents"])) {
    errors.push(`${label} must explicitly grant only contents: read`);
    return;
  }
  if (entries[0].value !== "read") {
    errors.push(`${label} must explicitly grant only contents: read`);
  }
}

function validateDeploySecretMap(lines, caller, errors) {
  const secrets = directEntry(lines, caller, "secrets");
  const entries = secrets ? mappingEntries(lines, secrets) : [];
  if (!secrets || secrets.value || !sameKeys(entries, DEPLOY_SECRETS)) {
    errors.push(
      `deploy caller must pass exactly ${DEPLOY_SECRETS.join(", ")}; never use secrets: inherit`,
    );
    return;
  }

  for (const secret of DEPLOY_SECRETS) {
    const entry = entries.find((candidate) => candidate.key === secret);
    if (entry?.value !== `\${{ secrets.${secret} }}`) {
      errors.push(`deploy caller must map ${secret} directly from the same-named repository secret`);
    }
  }
}

/**
 * Checks the actual caller/callee topology that keeps deploy secrets out of
 * fork and same-repository PR runs. This complements (rather than replaces)
 * the runtime GitHub branch/head check in deploy-trigger-policy.mjs.
 */
export function validateDeployWorkflowTopology({ ci, deploy, workflows }) {
  const errors = [];
  const deployLines = yamlLines(deploy);
  const ciLines = yamlLines(ci);

  const deployOn = rootEntry(deployLines, "on");
  const deployTriggers = deployOn ? mappingEntries(deployLines, deployOn) : [];
  if (!deployOn || deployOn.value || !sameKeys(deployTriggers, ["workflow_call"])) {
    errors.push(
      `${DEPLOY_WORKFLOW_PATH}: on must expose only workflow_call (never push, pull_request, workflow_run, or dispatch)`,
    );
  }

  const workflowCall = deployOn ? directEntry(deployLines, deployOn, "workflow_call") : null;
  const callProperties = workflowCall ? mappingEntries(deployLines, workflowCall) : [];
  if (!workflowCall || workflowCall.value || !sameKeys(callProperties, ["secrets"])) {
    errors.push(`${DEPLOY_WORKFLOW_PATH}: workflow_call must declare the deploy-secret allowlist`);
  }

  const declaredSecrets = workflowCall ? directEntry(deployLines, workflowCall, "secrets") : null;
  const secretDeclarations = declaredSecrets
    ? mappingEntries(deployLines, declaredSecrets)
    : [];
  if (!declaredSecrets || declaredSecrets.value || !sameKeys(secretDeclarations, DEPLOY_SECRETS)) {
    errors.push(
      `${DEPLOY_WORKFLOW_PATH}: workflow_call.secrets must declare exactly ${DEPLOY_SECRETS.join(", ")}`,
    );
  } else {
    for (const secret of secretDeclarations) {
      const properties = mappingEntries(deployLines, secret);
      if (!sameKeys(properties, ["required"]) || properties[0].value !== "false") {
        errors.push(`${DEPLOY_WORKFLOW_PATH}: ${secret.key} must be an optional explicit workflow_call secret`);
      }
    }
  }

  const referencedSecrets = new Set(
    [...deployLines
      .map((line) => line.raw)
      .join("\n")
      .matchAll(/\bsecrets\.([A-Za-z_][A-Za-z0-9_]*)\b/g)]
      .map((match) => match[1]),
  );
  if (
    referencedSecrets.size !== DEPLOY_SECRETS.length
    || DEPLOY_SECRETS.some((secret) => !referencedSecrets.has(secret))
  ) {
    errors.push(
      `${DEPLOY_WORKFLOW_PATH}: jobs may reference exactly the declared deploy secrets (${DEPLOY_SECRETS.join(", ")})`,
    );
  }

  const deployJobs = rootEntry(deployLines, "jobs");
  const verifyTrust = deployJobs ? directEntry(deployLines, deployJobs, "verify-trust") : null;
  const preflight = deployJobs ? directEntry(deployLines, deployJobs, "preflight") : null;
  const staging = deployJobs ? directEntry(deployLines, deployJobs, "staging") : null;
  if (!verifyTrust || !preflight || !staging) {
    errors.push(`${DEPLOY_WORKFLOW_PATH}: verify-trust, preflight, and staging jobs are all required`);
  } else {
    if (directEntry(deployLines, verifyTrust, "if")?.value !== "github.event_name == 'push' && github.ref == 'refs/heads/main'") {
      errors.push(`${DEPLOY_WORKFLOW_PATH}: verify-trust must be restricted to a main push`);
    }
    validateReadOnlyPermissions(deployLines, verifyTrust, `${DEPLOY_WORKFLOW_PATH}: verify-trust`, errors);
    const verifyTrustSource = sourceBlock(deployLines, verifyTrust);
    if (
      !verifyTrustSource.includes("node .github/scripts/deploy-trigger-policy.mjs")
      || /\bsecrets\./.test(verifyTrustSource)
    ) {
      errors.push(`${DEPLOY_WORKFLOW_PATH}: verify-trust must run the trigger policy without repository secrets`);
    }
    if (directEntry(deployLines, preflight, "needs")?.value !== "verify-trust") {
      errors.push(`${DEPLOY_WORKFLOW_PATH}: preflight must depend on verify-trust`);
    }
    if (
      directEntry(deployLines, preflight, "if")?.value
        !== "github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.verify-trust.outputs.trusted == 'true'"
    ) {
      errors.push(`${DEPLOY_WORKFLOW_PATH}: preflight must require a main push and trusted re-verification`);
    }
    if (directEntry(deployLines, staging, "needs")?.value !== "preflight") {
      errors.push(`${DEPLOY_WORKFLOW_PATH}: staging must depend on preflight`);
    }
    if (directEntry(deployLines, staging, "if")?.value !== "needs.preflight.outputs.enabled == 'true'") {
      errors.push(`${DEPLOY_WORKFLOW_PATH}: staging must require an enabled preflight`);
    }
  }

  const jobs = rootEntry(ciLines, "jobs");
  const authorize = jobs ? directEntry(ciLines, jobs, "authorize-deploy") : null;
  const caller = jobs ? directEntry(ciLines, jobs, "deploy-gateway") : null;
  if (!authorize || !caller) {
    errors.push(`${CI_WORKFLOW_PATH}: authorize-deploy and deploy-gateway jobs are both required`);
    return errors;
  }

  if (directEntry(ciLines, authorize, "if")?.value !== "github.event_name == 'push' && github.ref == 'refs/heads/main'") {
    errors.push(`${CI_WORKFLOW_PATH}: authorize-deploy must be restricted to a main push`);
  }
  validateReadOnlyPermissions(ciLines, authorize, `${CI_WORKFLOW_PATH}: authorize-deploy`, errors);
  const authorizeSource = sourceBlock(ciLines, authorize);
  if (
    !authorizeSource.includes("node .github/scripts/deploy-trigger-policy.mjs")
    || /\bsecrets\./.test(authorizeSource)
  ) {
    errors.push(`${CI_WORKFLOW_PATH}: authorize-deploy must run the trigger policy without repository secrets`);
  }
  if (!/^[ ]*- deploy-trigger-policy$/m.test(authorizeSource)) {
    errors.push(`${CI_WORKFLOW_PATH}: authorize-deploy must wait for deploy-trigger-policy tests`);
  }

  if (directEntry(ciLines, caller, "needs")?.value !== "authorize-deploy") {
    errors.push(`${CI_WORKFLOW_PATH}: deploy-gateway must depend only on authorize-deploy`);
  }
  if (
    directEntry(ciLines, caller, "if")?.value
      !== "github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.authorize-deploy.outputs.trusted == 'true'"
  ) {
    errors.push(`${CI_WORKFLOW_PATH}: deploy-gateway must require a main push and trusted authorization output`);
  }
  if (directEntry(ciLines, caller, "uses")?.value !== "./.github/workflows/deploy.yml") {
    errors.push(`${CI_WORKFLOW_PATH}: deploy-gateway must call the local deploy workflow`);
  }
  validateReadOnlyPermissions(ciLines, caller, `${CI_WORKFLOW_PATH}: deploy-gateway`, errors);
  validateDeploySecretMap(ciLines, caller, errors);
  const ciOutsideCaller = ciLines
    .filter((_, index) => index < caller.index || index >= blockEnd(ciLines, caller))
    .map((line) => line.raw)
    .join("\n");
  if (/\bsecrets\./.test(ciOutsideCaller)) {
    errors.push(
      `${CI_WORKFLOW_PATH}: repository secrets may appear only in deploy-gateway's explicit allowlist`,
    );
  }

  const deployReferences = [];
  for (const [path, workflow] of Object.entries(workflows)) {
    for (const line of yamlLines(workflow)) {
      if (line.key === "uses" && line.value?.includes(".github/workflows/deploy.yml")) {
        deployReferences.push(`${path}:${line.number}`);
      }
    }
  }
  if (
    deployReferences.length !== 1
    || !deployReferences[0].startsWith(`${CI_WORKFLOW_PATH}:`)
  ) {
    errors.push(
      `only ${CI_WORKFLOW_PATH}'s gated deploy-gateway job may call ${DEPLOY_WORKFLOW_PATH}; found ${deployReferences.join(", ") || "<none>"}`,
    );
  }

  return errors;
}

/**
 * Validates a workflow/action manifest without relying on a YAML parser. `uses:`
 * is deliberately line-oriented in GitHub Actions syntax, and retaining its
 * source line makes a policy failure immediately actionable.
 */
export function validateUses(text, file = "workflow.yml") {
  const errors = [];
  const uses = /^[\t ]*(?:-[\t ]*)?uses:[\t ]*([^\s#]+)(?:[\t ]+#.*)?$/gm;

  for (const match of text.matchAll(uses)) {
    const reference = match[1];
    const line = lineForOffset(text, match.index ?? 0);
    const location = `${file}:${line}`;

    // Repository-local actions and reusable workflows are reviewed in this
    // repository, so an external pin is not applicable.
    if (reference.startsWith("./")) continue;

    if (reference.startsWith("docker://")) {
      if (!OCI_DIGEST.test(reference)) {
        errors.push(
          `${location}: container action ${reference} must use an immutable sha256 digest`,
        );
      }
      continue;
    }

    const at = reference.lastIndexOf("@");
    if (at <= 0 || !FULL_GIT_SHA.test(reference.slice(at + 1))) {
      errors.push(
        `${location}: third-party action ${reference} must be pinned to a full 40-character commit SHA`,
      );
    }
  }

  return errors;
}

export function repositoryManifestPaths(root = REPO_ROOT) {
  const manifests = collectYamlFiles(join(root, ".github", "workflows"));
  const inspectAction = join(root, "inspect-action", "action.yml");
  if (existsSync(inspectAction)) manifests.push(inspectAction);
  return manifests;
}

export function validateDeploymentTopology(root = REPO_ROOT) {
  const workflowDirectory = join(root, ".github", "workflows");
  const deployPath = join(workflowDirectory, "deploy.yml");
  const ciPath = join(workflowDirectory, "ci.yml");
  if (!existsSync(deployPath) || !existsSync(ciPath)) {
    return [
      "deployment topology policy requires .github/workflows/ci.yml and .github/workflows/deploy.yml",
    ];
  }

  const workflows = Object.fromEntries(
    collectYamlFiles(workflowDirectory).map((path) => [
      relative(root, path),
      readFileSync(path, "utf8"),
    ]),
  );
  return validateDeployWorkflowTopology({
    ci: readFileSync(ciPath, "utf8"),
    deploy: readFileSync(deployPath, "utf8"),
    workflows,
  });
}

export function validateRepository(root = REPO_ROOT) {
  return [
    ...repositoryManifestPaths(root).flatMap((path) =>
      validateUses(readFileSync(path, "utf8"), relative(root, path)),
    ),
    ...validateDeploymentTopology(root),
  ];
}

function main() {
  const errors = validateRepository();
  if (errors.length > 0) {
    console.error("GitHub Actions security policy failed:\n" + errors.join("\n"));
    process.exitCode = 1;
    return;
  }

  const manifests = repositoryManifestPaths().map((path) => relative(REPO_ROOT, path));
  console.log(`workflow security policy passed for ${manifests.length} manifest(s)`);
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  main();
}
