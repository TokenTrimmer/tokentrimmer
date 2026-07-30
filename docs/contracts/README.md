# Generated product contracts

`product-contracts.manifest.json` records the route, route-preview coverage,
workflow-definition, workflow-write, model-catalog, gateway-capability, and
request-preflight contract IDs, versions, endpoints, schema/vector/corpus
paths, and SHA-256 hashes for every generated or public-owned compatibility
artifact. The canonical generated TypeScript is emitted both for repository
consumers and directly into the TypeScript SDK package; the manifest pins both
byte-identical copies. The model/capability/preflight subset is also emitted as
frozen Python dataclasses directly into the Python SDK package.

Regenerate with `cargo run -p tt-ts-types -- write`; verify checked-in bytes
with `cargo run --locked -p tt-ts-types -- check`. The generator derives from
the Rust parser/output types plus the Rust-tested route-preview coverage corpus;
it does not replace semantic validation, authorization, readiness, or execution
checks.
