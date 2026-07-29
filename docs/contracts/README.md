# Generated product contracts

`product-contracts.manifest.json` records the route, route-preview coverage,
workflow-definition, workflow-write, model-catalog, and gateway-capability
plus request-preflight contract IDs, versions, endpoints,
schema/vector/corpus paths, and SHA-256 hashes for every generated or
public-owned compatibility artifact. It includes the TypeScript product
bindings and the Python SDK's generated frozen model/capability/preflight
dataclasses.

Regenerate with `cargo run -p tt-ts-types -- write`; verify checked-in bytes
with `cargo run --locked -p tt-ts-types -- check`. The generator derives from
the Rust parser/output types plus the Rust-tested route-preview coverage corpus;
it does not replace semantic validation, authorization, readiness, or execution
checks.
