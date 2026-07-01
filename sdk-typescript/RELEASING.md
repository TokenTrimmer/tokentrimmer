# Releasing `@tokentrimmer/client` to npm

The TypeScript SDK is published to npm as
[`@tokentrimmer/client`](https://www.npmjs.com/package/@tokentrimmer/client) by
the [`release-npm.yml`](../.github/workflows/release-npm.yml) workflow, which
triggers on a `ts-v*` tag. It publishes with **npm provenance** (`--provenance`,
OIDC-attested) and **public access** (`--access public`, required because the
package is scoped under `@tokentrimmer`).

npm registry auth still uses an **`NPM_TOKEN` secret** (npm's own OIDC trusted
publishing is newer/less universally available; a token is the safe path today).

## One-time setup (USER-GATED — do this once before the first release)

1. **Reserve the scope + name.** Create/own the `@tokentrimmer` org (scope) on
   npm and confirm `@tokentrimmer/client` is available:
   https://www.npmjs.com/package/@tokentrimmer/client. If the scope is taken by
   someone else, change `name` in `package.json` (and the README/imports) first.

2. **Create an npm automation token.** On npmjs.com:
   *Access Tokens → Generate New Token → Granular Access Token* (or "Automation").
   Grant **publish** rights to the `@tokentrimmer` scope. Copy it.

3. **Add the repo secret.** In the GitHub repo:
   *Settings → Secrets and variables → Actions → New repository secret* named
   exactly **`NPM_TOKEN`**, value = the token from step 2. The workflow reads it
   as `NODE_AUTH_TOKEN`.

   > Until `NPM_TOKEN` is set, the registry publish step fails, **but** the
   > workflow still builds, tests, packs the tarball, and attaches it to the
   > GitHub Release for the tag — so adopters can `npm install <release-asset-url>`
   > even before the registry publish is wired up.

   No extra setup is needed for provenance: the workflow already grants
   `id-token: write` and the repo is public, which is all `--provenance` requires.

## Cutting a release

1. Bump the version in `sdk-typescript/package.json` → `version`.
2. Commit on `main`.
3. Tag and push: the tag is `ts-v<version>` and **must** match `package.json`
   exactly (the workflow fails the build otherwise).

   ```bash
   git tag ts-v0.1.0
   git push origin ts-v0.1.0
   ```

The workflow then verifies the tag/version match, runs `npm install` →
`npm run build` → `npm test`, `npm pack`s the built `dist/` and attaches the
tarball to the GitHub Release, then `npm publish --provenance --access public`.

## Local verification (no publish)

```bash
cd sdk-typescript
npm install
npm run build
npm pack --dry-run    # shows the exact tarball contents (dist/, README, LICENSE)
npm pack             # produces the installable tarball (e.g. tokentrimmer-client-0.1.0.tgz)
```

> Do **not** run `npm publish` locally — publishing (with provenance) is the
> workflow's job. The `.tgz` produced by `npm pack` can be shared or installed
> directly (`npm install ./tokentrimmer-client-0.1.0.tgz`) but should not be
> committed to the repository.
