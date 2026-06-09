# RouteConditions cross-type drift guard (batch 7e) — Design

**Status:** approved (working through remaining audit lows, 2026-06-09)
**Date:** 2026-06-09
**Slice:** Audit-remediation, public repo, `crates/plan-core` (test + dev-dep only). One clean dx/low. No production-code change.

## The finding (dx/low)
`tt_routing::RouteConditions` (`crates/routing/src/lib.rs:53`) and `tt_plan_core::RouteConditions` (`crates/plan-core/src/types.rs:113`) are two hand-maintained, field-identical structs. The 2026-05-30 `rv-routeaction-shared-type` follow-up aligned `RouteAction` across the two crates and locked it with `route_action_cross_type_wire_compat`, but `RouteConditions` was left with no equivalent guard. A future condition predicate added to one struct but not the other would silently fail to project in a Plan or fail to route at the gateway, with no test catching the drop.

## Decision: guard, don't unify
The Action offered two options: (a) a cross-type round-trip test, or (b) define the shape once in `tt-shared` and re-export. **Choosing (a)**, mirroring the precedent set for `RouteAction` — which is *also* still two separate structs kept aligned by a test, not a single shared type. Unifying into one `tt-shared` type would erase each crate's field-level doc comments, which deliberately document divergent *runtime* semantics for the same wire field (e.g. `plan-core` documents `has_images`/`has_audio` as never-evaluable in replay since `RequestLog` records no modality, while `tt_routing` evaluates them live). The wire shape must stay identical; the semantics intentionally differ. A test is the correct tool.

## The fix — a stronger guard than RouteAction's
The existing `route_action_cross_type_wire_compat` hand-writes a JSON string, so it cannot catch a *new* field added to `tt_routing` (the literal doesn't know about it). This batch does better: add `tt-routing` as a `plan-core` **dev-dependency** (no cycle — `routing` does not depend on `plan-core`) and write `route_conditions_cross_type_wire_compat` that:

1. Constructs a `tt_routing::RouteConditions { … }` with **every field set explicitly and no `..Default::default()`**. This struct literal is the compile-time tripwire: adding a field to `tt_routing::RouteConditions` makes the test fail to compile (missing field), forcing the author to extend the round-trip and discover any drop.
2. Serializes it, deserializes the JSON as `tt_plan_core::RouteConditions`, and asserts each field survived.
3. Re-serializes the `plan-core` value and asserts the JSON equals the `tt_routing` JSON byte-for-byte — proving declaration order and `skip_serializing_if` gating are identical on both sides (the runtime drift catch).

## Verification (to run)
- `cargo test -p tt-plan-core` — existing tests + the new `route_conditions_cross_type_wire_compat` pass.
- `cargo clippy -p tt-plan-core --all-targets` clean; `cargo fmt … --check` clean.
- Confirm no dependency cycle: `cargo tree -p tt-plan-core -i tt-routing` resolves (dev-only edge).
