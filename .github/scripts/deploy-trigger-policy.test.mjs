import assert from "node:assert/strict";
import test from "node:test";

import {
  PROTECTED_REF,
  evaluateDeployTrigger,
} from "./deploy-trigger-policy.mjs";

const REPOSITORY = "TokenTrimmer/tokentrimmer";
const SHA = "1111111111111111111111111111111111111111";

function protectedPush(overrides = {}) {
  return {
    eventName: "push",
    repository: REPOSITORY,
    ref: PROTECTED_REF,
    sha: SHA,
    event: {
      ref: PROTECTED_REF,
      after: SHA,
      deleted: false,
      repository: { full_name: REPOSITORY },
    },
    branch: { protected: true, commit: { sha: SHA } },
    ...overrides,
  };
}

test("protected same-repository main push can deploy", () => {
  assert.deepEqual(evaluateDeployTrigger(protectedPush()), {
    trusted: true,
    failures: [],
  });
});

test("a retry of the current protected-main CI run remains deployable", () => {
  // GitHub preserves the original `push` event context when CI is re-run.
  // The policy still requires that original SHA to be the current protected
  // main head, so a retry after a later push is denied by the next fixture.
  assert.equal(evaluateDeployTrigger(protectedPush()).trusted, true);
});

for (const [name, pullRequest] of [
  [
    "fork PR whose head branch is named main",
    { head: { ref: "main", repo: { full_name: "attacker/fork" } } },
  ],
  [
    "same-repository PR",
    { head: { ref: "feature", repo: { full_name: REPOSITORY } } },
  ],
]) {
  test(`direct ${name} cannot deploy even with main-shaped SHA metadata`, () => {
    const input = protectedPush({ eventName: "pull_request" });
    input.event.pull_request = pullRequest;
    const result = evaluateDeployTrigger(input);
    assert.equal(result.trusted, false);
    assert.ok(result.failures.includes("event is not a push"));
  });
}

for (const [name, event] of [
  ["fork PR whose branch is main", { head_branch: "main", head_repository: { full_name: "attacker/fork" } }],
  ["fork PR from another branch", { head_branch: "feature", head_repository: { full_name: "attacker/fork" } }],
  ["same-repository PR", { head_branch: "feature", head_repository: { full_name: REPOSITORY } }],
  ["legacy workflow_run rerun", { event: "push", run_attempt: 2 }],
]) {
  test(`${name} cannot deploy`, () => {
    const input = protectedPush({
      eventName: "workflow_run",
      event: { workflow_run: event, repository: { full_name: REPOSITORY } },
    });
    assert.equal(evaluateDeployTrigger(input).trusted, false);
  });
}

test("deleted main SHA cannot deploy", () => {
  const input = protectedPush();
  input.event.deleted = true;
  assert.equal(evaluateDeployTrigger(input).trusted, false);
});

test("rebased or superseded SHA cannot deploy", () => {
  const input = protectedPush();
  input.branch.commit.sha = "2222222222222222222222222222222222222222";
  assert.equal(evaluateDeployTrigger(input).trusted, false);
});

test("unprotected main cannot deploy", () => {
  const input = protectedPush();
  input.branch.protected = false;
  assert.equal(evaluateDeployTrigger(input).trusted, false);
});

test("manual dispatch cannot deploy", () => {
  assert.equal(
    evaluateDeployTrigger(protectedPush({ eventName: "workflow_dispatch" }))
      .trusted,
    false,
  );
});
