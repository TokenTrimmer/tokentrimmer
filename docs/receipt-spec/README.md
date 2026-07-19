# TokenTrimmer proof contracts

This directory publishes the versioned structural schemas and deterministic
fixtures for every public proof family:

- compression receipts (VCR v1);
- L2 cache-hit receipts (L2 v1);
- workflow-run receipts (WFR v1–v4);
- agent-run receipts (ARR v1–v2); and
- deterministic savings replay bundles (v1).

[`receipt-contracts.manifest.json`](receipt-contracts.manifest.json) is the
machine-readable index. It binds each family to its versions, canonical domain,
schema, vectors, mint/share surface, verifier command, and artifact SHA-256.
[`../../bindings/receipt-contracts.generated.ts`](../../bindings/receipt-contracts.generated.ts)
is generated from the same JSON Schemas.

The checked-in artifacts are generated from the real Rust wire types,
canonicalizers, signers, and replay engine:

```sh
cargo run -p tt-ts-types -- write
cargo run -p tt-ts-types -- check
node scripts/verify-receipt-contracts.mjs
```

The first command is the only supported edit path for generated schemas,
TypeScript, vectors, and the manifest. CI runs the latter two commands plus the
Rust generator tests. The JavaScript verifier reconstructs canonical payloads
without importing the Rust implementation and rejects re-signed forged formula,
coverage, projection, legacy-add-on, mixed-family, and future-version fixtures.

These are structural and cryptographic-integrity contracts. A valid shape or
signature does not establish issuer identity, provider usage, independent math
replay (except a successfully replayed bundle), or invoice reconciliation.
