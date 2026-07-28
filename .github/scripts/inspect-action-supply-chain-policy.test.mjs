import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const manifest = readFileSync("inspect-action/action.yml", "utf8");

test("Inspect action installs tt-cli from an immutable full commit SHA", () => {
  assert.match(
    manifest,
    /tt-version:[\s\S]*?default:\s*[0-9a-f]{40}/i,
    "the default tt-cli source revision must be a full commit SHA",
  );
  assert.match(
    manifest,
    /cargo install --locked --force --root "\$install_root"\s+\\\n\s*--git https:\/\/github\.com\/tokentrimmer\/tokentrimmer\s+\\\n\s*--rev "\$TT_REVISION" tt-cli/,
    "cargo must install the explicitly pinned revision",
  );
  assert.doesNotMatch(manifest, /--branch\s+/);
  assert.doesNotMatch(manifest, /--tag\s+/);
  assert.doesNotMatch(manifest, /\bdefault:\s*latest\b/i);
});

test("Inspect action never trusts a pre-existing tt binary on PATH", () => {
  assert.doesNotMatch(manifest, /command -v tt/);
  assert.match(manifest, /echo "TT_BIN=\$tt_bin" >> "\$GITHUB_ENV"/);
  assert.match(manifest, /\$\{TT_BIN:\?tt-cli install did not export TT_BIN\}/);
});

test("Inspect action records the resolved source revision and installed binary digest", () => {
  assert.match(manifest, /TokenTrimmer tt-cli source revision: %s/);
  assert.match(manifest, /TokenTrimmer tt-cli binary SHA-256: %s/);
  assert.match(manifest, /(?:sha256sum|shasum -a 256)/);
  assert.match(manifest, /echo "TT_CLI_REVISION=\$TT_REVISION" >> "\$GITHUB_ENV"/);
  assert.match(manifest, /echo "TT_CLI_SHA256=\$tt_bin_sha256" >> "\$GITHUB_ENV"/);
});

test("Inspect action pins its SARIF uploader to an immutable commit", () => {
  assert.match(
    manifest,
    /uses:\s*github\/codeql-action\/upload-sarif@[0-9a-f]{40}(?:\s|#)/i,
    "a mutable major tag would let an upstream action change customer CI",
  );
});
