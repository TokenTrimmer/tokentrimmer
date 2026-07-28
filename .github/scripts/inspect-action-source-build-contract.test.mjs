import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const contract = JSON.parse(
  readFileSync("inspect-action/source-build-contract.json", "utf8"),
);
const manifest = readFileSync("inspect-action/action.yml", "utf8");
const readme = readFileSync("inspect-action/README.md", "utf8");
const architecture = readFileSync("docs/tokentrimmer-architecture-spec-v1.md", "utf8");
const releaseRunbook = readFileSync("docs/release-runbook.md", "utf8");
const cratesRelease = readFileSync(".github/workflows/release-crates.yml", "utf8");

const FULL_SHA = /^[0-9a-f]{40}$/i;
const escapeRegExp = (value) => value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

test("Inspect source-build contract declares an immutable source build, not a release artifact", () => {
  assert.equal(contract.schema_version, 1);
  assert.equal(contract.cli_source_repository, "https://github.com/tokentrimmer/tokentrimmer");
  assert.match(contract.cli_source_revision, FULL_SHA);
  assert.equal(contract.distribution_model, "locked_source_build");
  assert.equal(contract.signed_release_artifact, false);
});

test("Inspect action default and every shipped monorepo example use the contract revision", () => {
  const defaultRevision = /tt-version:[\s\S]*?default:\s*([0-9a-f]{40})/i.exec(manifest)?.[1];
  assert.equal(defaultRevision, contract.cli_source_revision);
  assert.match(
    manifest,
    new RegExp(
      `--git ${escapeRegExp(contract.cli_source_repository)}[\\s\\S]*?--rev "\\$TT_REVISION" tt-cli`,
    ),
  );

  const documentedPins = [
    ...readme.matchAll(/TokenTrimmer\/tokentrimmer\/inspect-action@([0-9a-f]{40})/gi),
  ].map((match) => match[1]);
  assert.ok(documentedPins.length > 0, "README must ship at least one usable monorepo action pin");
  assert.deepEqual([...new Set(documentedPins)], [contract.cli_source_revision]);
  assert.ok(readme.includes("| `tt-version` | `" + contract.cli_source_revision + "`"));
  assert.match(readme, /source-build-contract\.json/);
});

test("published guidance does not substitute a mutable action tag or source-build record for artifact provenance", () => {
  assert.doesNotMatch(architecture, /uses:\s*tokentrimmer\/inspect-action@v1/i);
  assert.match(
    architecture,
    new RegExp(`TokenTrimmer/tokentrimmer/inspect-action@${contract.cli_source_revision}`),
  );
  assert.match(readme, /not a substitute for a separately signed release\s+archive/i);
  assert.match(releaseRunbook, /source-build contract/i);
  assert.match(releaseRunbook, /\*\*not\*\* a signed\s+release artifact/i);
  assert.match(releaseRunbook, /does \*\*not\*\* currently build or upload platform CLI binaries/i);
  assert.doesNotMatch(cratesRelease, /upload-artifact|action-gh-release|gh release|cargo dist/i);
});
