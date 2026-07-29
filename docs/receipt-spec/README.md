# TokenTrimmer proof contracts

This directory publishes the versioned structural schemas and deterministic
fixtures for every public-core proof family:

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

## Cloud Chat dispatch interoperability vector

[`ctdr-v1.golden.json`](ctdr-v1.golden.json) is a deterministic cross-product
vector for the Cloud-minted Chat dispatch artifact. It is intentionally outside
the generated public-core manifest: the public repository provides the
independent `tt verify-receipt` consumer, while the private Cloud API owns the
wire type, persistence, authorization, and
`POST /v1/admin/chat-dispatch-receipts/{receipt_id}/sign` mint.

The CLI rejects unknown fields, reconstructs the exact `ctdr:v1|...` canonical
payload, checks the embedded key against a key supplied out of band, verifies
the Ed25519 signature, and replays the signed request-delta formula when its
components are present:

```sh
tt verify-receipt \
  --receipt docs/receipt-spec/ctdr-v1.golden.json \
  --key-hex ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c
```

The explicit key establishes only which key the verifier selected. To bind it
to complete issuer-key history, also supply a v1 registry/keyset file and the
canonical manifest SHA-256 obtained through an independent channel:

```sh
tt verify-receipt \
  --receipt receipt.json \
  --key-hex <hex> \
  --issuer-registry issuer-registry.json \
  --registry-sha256 <64-lowercase-hex>
```

The CLI reconstructs and hashes the strict keyset, validates every Ed25519 key
fingerprint and lifecycle state, and requires the selected receipt key to be
currently active. Retired keys fail the current issuer-trust gate because the
receipt families do not share one uniformly trusted signed issuance-time
field. Revoked, absent, malformed, unpinned, and out-of-window keys also fail
closed. The pin is trusted only to the extent that it was obtained
independently; the same-origin registry cannot authenticate its own issuer.

`evidence_scope=tokentrimmer_gateway_accounting` is deliberate. A passing CTDR
proves only that the trusted key signed unchanged TokenTrimmer gateway
accounting; it is not provider telemetry, an issuer-identity proof, or invoice
reconciliation.
