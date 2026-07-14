# DX-1 Release Train — One-Command Publish Runbook

## Status (verified 2026-07-14)

All three release workflows are **tag-wired** and ready:

| SDK | Version | Tag pattern | Workflow | Secret required |
|-----|---------|-------------|----------|----------------|
| Rust `tt-cli` + crates | 0.2.0 | `cli-v*` | `release-crates.yml` | `CARGO_REGISTRY_TOKEN` |
| Python `tokentrimmer` | 0.3.0 | `py-v*` | `release-pypi.yml` | PyPI OIDC trusted publishing |
| TypeScript `@tokentrimmer/client` | 0.1.0 | `ts-v*` | `release-npm.yml` | `NPM_TOKEN` |

The workflows build + publish on tag push — no manual steps beyond the tag.

## Pre-release checklist

1. **Verify the version matches the tag:**
   - Rust: `grep '^version' crates/cli/Cargo.toml` → must match `cli-v{version}`
   - Python: `grep 'version' sdk-python/pyproject.toml` → must match `py-v{version}`
   - TypeScript: `grep '"version"' sdk-typescript/package.json` → must match `ts-v{version}`

2. **Verify the workspace builds clean:**
   ```bash
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace --no-run  # compile test targets
   ```

3. **Verify the SDKs import:**
   ```bash
   python3 -c "import tokentrimmer; print(tokentrimmer.__version__)"
   cd sdk-typescript && npm run build && node -e "const t = require('./dist/index.js'); console.log('OK')"
   ```

## Publishing

### tt-cli 0.2.0 (crates.io)

```bash
git tag cli-v0.2.0
git push origin cli-v0.2.0
```

This triggers `release-crates.yml` which:
- Builds `tt-cli` (and the workspace crates) for `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-pc-windows-msvc`
- Publishes all `tt-*` crates to crates.io
- Uploads GitHub Release binaries

**Requires:** `CARGO_REGISTRY_TOKEN` secret set in the repo settings.

### tokentrimmer 0.3.0 (PyPI)

```bash
git tag py-v0.3.0
git push origin py-v0.3.0
```

This triggers `release-pypi.yml` which:
- Builds the Python wheel
- Publishes via OIDC trusted publishing (no token needed — GitHub OIDC → PyPI)

**Requires:** PyPI trusted-publishing configured (GitHub Actions as a trusted publisher on pypi.org).

### @tokentrimmer/client 0.1.0 (npm)

```bash
git tag ts-v0.1.0
git push origin ts-v0.1.0
```

This triggers `release-npm.yml` which:
- Builds the TypeScript SDK
- Publishes to npm as `@tokentrimmer/client`

**Requires:** `NPM_TOKEN` secret set in the repo settings.

## Post-publish

- **Cloud public-pin bump:** After the Rust publish, the cloud repo's `tt-*` git-dep pin must advance to the new public-main SHA (memory [[cloud-public-deps-git-model]]). The `public-pin-drift.yml` workflow auto-opens a PR for this.
- **Homebrew tap:** (future) The cargo-dist shell installer + Homebrew tap is a follow-up — not blocking the tag-based release.

## What's shipped since 0.1.0

The Rust CLI (0.1.0 → 0.2.0) gained:
- `tt doctor` (DX-2) — DNS/key/MCP health diagnostics
- `tt login` health-check (DX-4) — confirms gateway reachable after login
- `tt whoami --check` (DX-4) — authenticated round-trip
- `verify-receipt` in the Prove help group (DX-7)
- `tt init` + `tt chat` first-savings moment (DX-3)
- `x-api-key` Bearer alias (P0-5)
- The workflow triggers type + validation (CO-2)
- The budget-breach policy enforcement (CO-4)

The Python SDK (0.2.0 → 0.3.0) gained:
- CrewAI adapter (DX-5) — budget-STOP + cost attribution
- OpenAI Agents SDK adapter (DX-5) — tracing processor with budget-STOP
- Document distillation helpers (D3)
