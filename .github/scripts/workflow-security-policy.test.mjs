import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import {
  validateDeployWorkflowTopology,
  validateRepository,
  validateUses,
} from "./workflow-security-policy.mjs";

const SHA = "0123456789abcdef0123456789abcdef01234567";
const DIGEST = "a".repeat(64);

test("immutable GitHub Action commit SHAs and local actions are accepted", () => {
  assert.deepEqual(
    validateUses(`
      - uses: actions/checkout@${SHA} # v7
      - uses: ./inspect-action
      uses: ./.github/workflows/deploy.yml
    `),
    [],
  );
});

test("a mutable action tag or abbreviated SHA is rejected", () => {
  const errors = validateUses(`
      - uses: actions/checkout@v7
      - uses: example/action@0123456
    `, "fixture.yml");
  assert.equal(errors.length, 2);
  assert.match(errors[0], /fixture\.yml:2/);
  assert.match(errors[0], /full 40-character commit SHA/);
});

test("container actions require an immutable OCI digest", () => {
  assert.deepEqual(
    validateUses(`- uses: docker://example/action@sha256:${DIGEST}`),
    [],
  );
  assert.match(
    validateUses("- uses: docker://example/action:latest")[0],
    /immutable sha256 digest/,
  );
});

test("all committed public workflow and Inspect action references are immutable", () => {
  assert.deepEqual(validateRepository(), []);
});

function deploymentSources() {
  const ci = readFileSync(".github/workflows/ci.yml", "utf8");
  const deploy = readFileSync(".github/workflows/deploy.yml", "utf8");
  return {
    ci,
    deploy,
    workflows: {
      ".github/workflows/ci.yml": ci,
      ".github/workflows/deploy.yml": deploy,
    },
  };
}

test("deployment containment has one push-gated caller and an explicit secret allowlist", () => {
  assert.deepEqual(validateDeployWorkflowTopology(deploymentSources()), []);
});

test("deployment containment rejects an unsafe trigger or inherited secrets", () => {
  const unsafeTrigger = deploymentSources();
  unsafeTrigger.deploy = unsafeTrigger.deploy.replace(
    "  workflow_call:\n    secrets:",
    "  workflow_run:\n    secrets:",
  );
  assert.match(
    validateDeployWorkflowTopology(unsafeTrigger).join("\n"),
    /on must expose only workflow_call/,
  );

  const inheritedSecrets = deploymentSources();
  inheritedSecrets.ci = inheritedSecrets.ci.replace(
    "    secrets:\n      FLY_API_TOKEN: ${{ secrets.FLY_API_TOKEN }}\n      STAGING_DATABASE_URL_DIRECT: ${{ secrets.STAGING_DATABASE_URL_DIRECT }}\n      PROD_DATABASE_URL_DIRECT: ${{ secrets.PROD_DATABASE_URL_DIRECT }}",
    "    secrets: inherit",
  );
  assert.match(
    validateDeployWorkflowTopology(inheritedSecrets).join("\n"),
    /never use secrets: inherit/,
  );

  const unsafePreflight = deploymentSources();
  unsafePreflight.deploy = unsafePreflight.deploy.replace(
    "if: github.event_name == 'push' && github.ref == 'refs/heads/main' && needs.verify-trust.outputs.trusted == 'true'",
    "if: always()",
  );
  assert.match(
    validateDeployWorkflowTopology(unsafePreflight).join("\n"),
    /preflight must require a main push and trusted re-verification/,
  );

  const missingApproval = deploymentSources();
  missingApproval.deploy = missingApproval.deploy.replace(
    "needs: [staging, verify-production-approval]",
    "needs: staging",
  );
  assert.match(
    validateDeployWorkflowTopology(missingApproval).join("\n"),
    /prod must depend on staging and verify-production-approval/,
  );

  const weakenedApproval = deploymentSources();
  weakenedApproval.deploy = weakenedApproval.deploy.replace(
    "            reviewerRule.prevent_self_review !== true ||\n",
    "",
  );
  assert.match(
    validateDeployWorkflowTopology(weakenedApproval).join("\n"),
    /must inspect required reviewers, self-review prevention, admin bypass, and protected-branch policy/,
  );

  const missingBranchPolicy = deploymentSources();
  missingBranchPolicy.deploy = missingBranchPolicy.deploy.replace(
    "            branchPolicy.protected_branches !== true ||\n",
    "",
  );
  assert.match(
    validateDeployWorkflowTopology(missingBranchPolicy).join("\n"),
    /must inspect required reviewers, self-review prevention, admin bypass, and protected-branch policy/,
  );

  const unsafeCiSecret = deploymentSources();
  unsafeCiSecret.ci = unsafeCiSecret.ci.replace(
    "  fmt-and-clippy:\n",
    "  fmt-and-clippy:\n    env:\n      OPENAI_API_KEY: ${{ secrets.OPENAI_API_KEY }}\n",
  );
  assert.match(
    validateDeployWorkflowTopology(unsafeCiSecret).join("\n"),
    /repository secrets may appear only in deploy-gateway's explicit allowlist/,
  );
});

test("deployment containment rejects an additional local caller", () => {
  const sources = deploymentSources();
  sources.workflows[".github/workflows/unsafe-deploy-caller.yml"] = `
name: unsafe deploy caller
on:
  push:
jobs:
  deploy:
    uses: ./.github/workflows/deploy.yml
`;

  assert.match(
    validateDeployWorkflowTopology(sources).join("\n"),
    /only .github\/workflows\/ci.yml's gated deploy-gateway job may call/,
  );
});

test("the blocking CI gate runs both analyzers and gates deployments", () => {
  const ci = readFileSync(".github/workflows/ci.yml", "utf8");
  assert.match(ci, /workflow-security:[\s\S]*?actionlint -color -shellcheck= -pyflakes=/);
  assert.match(ci, /workflow-security:[\s\S]*?zizmor \.github\/workflows/);
  assert.match(ci, /authorize-deploy:[\s\S]*?needs:[\s\S]*?- workflow-security/);
});

test("published OCI images carry provenance and an SBOM", () => {
  const dockerBuild = readFileSync(".github/workflows/docker-build.yml", "utf8");
  assert.match(dockerBuild, /provenance:\s*mode=max/);
  assert.match(dockerBuild, /sbom:\s*true/);
});

test("secret scanner installation verifies the released archive", () => {
  const ci = readFileSync(".github/workflows/ci.yml", "utf8");
  assert.match(ci, /GITLEAKS_SHA256:\s*"[a-f0-9]{64}"/);
  assert.match(ci, /sha256sum --check --strict/);
  assert.doesNotMatch(ci, /gitleaks_[^\n]*\|\s*sudo tar/);
});
