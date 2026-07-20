# Generated product contracts

`product-contracts.manifest.json` records the route, workflow-definition,
workflow-write, and gateway-capability contract IDs, versions, endpoints,
schema/vector/corpus paths, and SHA-256 hashes for every generated artifact.

Regenerate with `cargo run -p tt-ts-types -- write`; verify checked-in bytes
with `cargo run --locked -p tt-ts-types -- check`. The generator derives from
the Rust parser/output types and does not replace semantic validation,
authorization, readiness, or execution checks.
