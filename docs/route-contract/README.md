# Route contract corpus

This directory publishes two complementary artifacts for
`tokentrimmer.route.v1`:

- `route-write.schema.json` is generated from the exact Rust
  `tt_routing::RouteWriteRequest` parser and its nested live-routing types; the
  matching TypeScript is in `bindings/product-contracts.generated.ts`.
- `tokentrimmer.route.v1.corpus.json` is the versioned semantic compatibility
  corpus consumed by the real canonicalizer and its control-plane adapter.

The generated schema is structural, not a second semantic validator: route
effect, exclusivity, finite-money, catalog, capability, and activation rules
remain in the Rust canonicalizer. Hosted consumers must vendor the artifacts
from the same immutable public revision and compare exact bytes in
cross-repository CI.

## Format v1

The top-level object has three fields:

```json
{
  "corpus": { "id": "tokentrimmer.route-contract-corpus", "version": 1 },
  "contract": { "id": "tokentrimmer.route.v1", "version": 1 },
  "cases": []
}
```

Each case has a unique `id`, one or both input projections, and matching
expected outcomes:

```json
{
  "id": "stable-case-name",
  "gateway": { "name": "…", "when": {}, "then": {} },
  "control_plane": {
    "schema_version": 1,
    "name": "…",
    "priority": 100,
    "enabled": true,
    "conditions": {},
    "target": {}
  },
  "expected": {
    "gateway": { "outcome": "accepted", "canonical": {} },
    "control_plane": { "outcome": "rejected", "issues": [] }
  }
}
```

`gateway` is the public `{when, then}` wire shape. `control_plane` is the
split `{conditions, target}` shape supplied to the canonical-parts adapter.
Each present input must have a corresponding expected outcome. A projection
may be omitted when its native boundary cannot express the test input; for
example, a gateway `u32` priority above the shared PostgreSQL `INT` maximum
cannot be passed to the control-plane adapter's `i32` parameter.

The format is intentionally strict: the three top-level fields are required;
case IDs are unique; each case has at least one projection; and each outcome
is exactly either `accepted` with `canonical` or `rejected` with ordered
`issues`. This makes the file simple to copy as test data in another Rust
crate without generating bindings or embedding a second validator.

Accepted outcomes contain the complete canonical identity used for storage and
activation: `schema_version`, `name`, `priority`, `enabled`, `conditions`,
`target`, and `canonical_hash`. Rejected outcomes contain the ordered
field/code issue pairs. Messages are deliberately omitted because explanatory
copy is not the portable compatibility surface.

Dashboard proxy validation is deliberately not a semantic corpus consumer: it
must preserve nested route JSON and tt-api's field-addressed 422 response
rather than growing a third hand-maintained canonicalizer.
