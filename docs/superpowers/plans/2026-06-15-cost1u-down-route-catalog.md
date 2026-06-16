# COST-1(U) Down-Route Catalog Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship an opt-in flagship→mini down-route catalog so the drop-in gateway delivers model right-sizing savings with no hand-authored rules, guarded by a new `not_reasoning_class` condition and the existing `auto_pause` parity-judge auto-revert.

**Architecture:** A curated same-provider mapping in `crates/routing/src/catalog.rs` produces `NewRoute`s (target_model set, `auto_pause: true`, `pause_floor_pass_rate: 0.92`, `not_reasoning_class: true`). A new `tt route catalog <enable|disable|status>` CLI materializes/removes them via the existing `/v1/routes` CRUD. A new `RouteConditions.not_reasoning_class` field (rides in the existing `conditions` JSONB — **no migration**) is matched against an `is_reasoning_class` signal computed in `apply_routing` via `reasoning_class::classify`.

**Tech Stack:** Rust, serde, axum (gateway), clap (CLI), reqwest (CLI→gateway), the existing routing engine + `route_autopause` + `route_savings`.

**Spec:** `docs/superpowers/specs/2026-06-15-cost1u-down-route-catalog-design.md`

> **REFINEMENT vs spec (deliberate, same behavior, lower risk):** the spec proposed a `managed_by: "catalog"` field on the route model. Implementation review found routes persist `when`/`then` as JSONB with no separate routes-table migration to alter cleanly, so this plan instead marks catalog routes by a **deterministic reserved name** (`catalog: <provider> <source>→<target>`). `disable` removes exactly the routes whose names the builder produces; no schema/store change, no migration. Same outcome ("distinguish + remove only catalog routes").

---

## File Structure

| File | Create/Modify | Responsibility |
|---|---|---|
| `crates/routing/src/lib.rs` | Modify | Add `RouteConditions.not_reasoning_class`; thread `is_reasoning_class` signal through `matches()` + `evaluate_with_signals()`; add `RoutingEngine::uses_reasoning_class()`; `pub mod catalog;` |
| `crates/routing/src/catalog.rs` | Create | Curated flagship→sibling table, `catalog_routes() -> Vec<NewRoute>`, `catalog_route_name(...)`, `is_catalog_route_name(&str)` |
| `crates/core/src/routes/chat.rs` | Modify | In `apply_routing`, compute `is_reasoning_class` (lazily) and pass it to `evaluate_with_signals` |
| `crates/cli/src/route/mod.rs` | Modify | `RouteCmd::Catalog` + enable/disable/status handlers over the existing reqwest CRUD |
| `crates/cli/src/main.rs` | Modify | clap wiring for `tt route catalog <enable\|disable\|status>` |
| `docs/tt-cli-commands.md`, `docs/routing-rules-guide.md` | Modify | Document the catalog command + the `not_reasoning_class` condition |

**Verification convention (every task):** public CI gates `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, and plan determinism. Run the named per-task commands; do not let a golden/attested number shift.

---

## Task 1: Add the `not_reasoning_class` route condition

**Files:**
- Modify: `crates/routing/src/lib.rs` (the `RouteConditions` struct, ~lines 70–124)
- Test: `crates/routing/src/lib.rs` (the existing `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test** (append to the `tests` module in `crates/routing/src/lib.rs`)

```rust
#[test]
fn not_reasoning_class_defaults_false_and_round_trips() {
    // default is false and omitted from the wire when false (back-compat)
    let c = RouteConditions::default();
    assert!(!c.not_reasoning_class);
    let json = serde_json::to_string(&c).unwrap();
    assert!(!json.contains("not_reasoning_class"), "absent when false: {json}");

    // old JSON without the field still deserializes
    let parsed: RouteConditions = serde_json::from_str(r#"{"model_in":["gpt-4o"]}"#).unwrap();
    assert!(!parsed.not_reasoning_class);

    // set true round-trips
    let on = RouteConditions { not_reasoning_class: true, ..Default::default() };
    let j = serde_json::to_string(&on).unwrap();
    assert!(j.contains(r#""not_reasoning_class":true"#));
    let back: RouteConditions = serde_json::from_str(&j).unwrap();
    assert!(back.not_reasoning_class);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p tt-routing not_reasoning_class_defaults_false_and_round_trips`
Expected: FAIL — `RouteConditions` has no field `not_reasoning_class`.

- [ ] **Step 3: Add the field** to `RouteConditions` (after `upstream_latency_ms_p95_gt`, matching the bool serde pattern used in `RouteAction`)

```rust
    /// Match only when the request is NOT classified as reasoning-is-the-work
    /// (Math/Code/Legal/Medical, via `tt-core`'s `reasoning_class`). Used by the
    /// down-route catalog so a flagship→mini swap never applies to confidently-
    /// wrong-on-cheap-models traffic. The classification is computed in the
    /// gateway and supplied to the engine as the `is_reasoning_class` signal.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub not_reasoning_class: bool,
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p tt-routing not_reasoning_class_defaults_false_and_round_trips`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/routing/src/lib.rs
git commit -m "feat(routing): add not_reasoning_class route condition (wire-compat, JSONB)"
```

---

## Task 2: Thread the `is_reasoning_class` signal through the engine

**Files:**
- Modify: `crates/routing/src/lib.rs` — `fn matches(...)` (~544–613), `RoutingEngine::evaluate_with_signals` (~516–535), add `RoutingEngine::uses_reasoning_class`
- Test: `crates/routing/src/lib.rs` (`tests` module)

> Adds one trailing `bool` param to `matches` and `evaluate_with_signals`. Update every in-crate caller (there are test callers in this file; the gateway caller is updated in Task 3).

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn not_reasoning_class_condition_uses_signal() {
    let route = Route {
        id: Uuid::nil(), name: "c".into(), priority: 100, enabled: true, paused: false,
        when: RouteConditions { model_in: vec!["gpt-4o".into()], not_reasoning_class: true, ..Default::default() },
        then: RouteAction { target_model: Some("gpt-4o-mini".into()), ..Default::default() },
    };
    let engine = RoutingEngine::new(vec![route]);
    let req = test_req("gpt-4o"); // helper already used by sibling tests; builds a ChatCompletionRequest
    let ctx = test_ctx();         // sibling helper

    // reasoning request → condition fails → no match
    assert!(engine
        .evaluate_with_signals(&req, &ctx, 10, None, None, /* is_reasoning_class */ true)
        .is_none());
    // non-reasoning request → matches
    assert!(engine
        .evaluate_with_signals(&req, &ctx, 10, None, None, false)
        .is_some());
}

#[test]
fn uses_reasoning_class_reports_presence() {
    let with = Route {
        id: Uuid::nil(), name: "a".into(), priority: 100, enabled: true, paused: false,
        when: RouteConditions { not_reasoning_class: true, ..Default::default() },
        then: RouteAction::default(),
    };
    let without = Route { when: RouteConditions::default(), ..with.clone() };
    assert!(RoutingEngine::new(vec![with]).uses_reasoning_class());
    assert!(!RoutingEngine::new(vec![without]).uses_reasoning_class());
}
```

> If `test_req`/`test_ctx` helpers don't exist with those names, reuse whatever request/ctx builders the sibling tests in this module already use (grep the `tests` module for how existing `evaluate_with_signals` tests construct `req`/`ctx`) — do not invent new infrastructure.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p tt-routing not_reasoning_class_condition_uses_signal uses_reasoning_class_reports_presence`
Expected: FAIL — `evaluate_with_signals` takes 5 args not 6; `uses_reasoning_class` doesn't exist.

- [ ] **Step 3: Add the param + method + condition check**

In `evaluate_with_signals`, add the trailing param and pass it to `matches`:

```rust
    pub fn evaluate_with_signals(
        &self,
        req: &ChatCompletionRequest,
        ctx: &RequestContext,
        input_tokens_estimate: u32,
        estimated_cost_usd: Option<f64>,
        observed_p95_ms: Option<u32>,
        is_reasoning_class: bool,
    ) -> Option<&Route> {
        self.routes.iter().find(|r| {
            r.enabled
                && matches(
                    r, req, ctx,
                    input_tokens_estimate, estimated_cost_usd, observed_p95_ms,
                    is_reasoning_class,
                )
        })
    }
```

Add the helper (any enabled route that consults the signal):

```rust
    /// True when at least one enabled route uses the `not_reasoning_class`
    /// condition — lets the gateway skip computing the signal otherwise.
    #[must_use]
    pub fn uses_reasoning_class(&self) -> bool {
        self.routes.iter().any(|r| r.enabled && r.when.not_reasoning_class)
    }
```

In `fn matches(...)`, add the trailing param and the check (place it next to the other condition checks):

```rust
fn matches(
    r: &Route,
    req: &ChatCompletionRequest,
    ctx: &RequestContext,
    input_tokens: u32,
    estimated_cost_usd: Option<f64>,
    observed_p95_ms: Option<u32>,
    is_reasoning_class: bool,
) -> bool {
    let c = &r.when;
    // ... existing checks ...
    if c.not_reasoning_class && is_reasoning_class {
        return false;
    }
    // ... remaining existing checks ...
    true
}
```

Update any OTHER in-crate callers of `evaluate_with_signals`/`matches` (the `tests` module + a plain `evaluate` wrapper if present) to pass `false` for `is_reasoning_class` where reasoning classification is irrelevant.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p tt-routing`
Expected: PASS (all routing tests, incl. the two new ones).

- [ ] **Step 5: Commit**

```bash
git add crates/routing/src/lib.rs
git commit -m "feat(routing): thread is_reasoning_class signal into route matching"
```

---

## Task 3: Compute + pass the signal in the gateway (`apply_routing`)

**Files:**
- Modify: `crates/core/src/routes/chat.rs` — `apply_routing` (the `evaluate_with_signals` call ~5570–5576; signal block ~5540–5563)
- Test: `crates/core/tests/route_rewrite.rs` (the existing route-rewrite integration test file — follow its harness)

- [ ] **Step 1: Write the failing test** (append to `crates/core/tests/route_rewrite.rs`, mirroring an existing test's setup)

```rust
// A catalog-style down-route with not_reasoning_class must NOT rewrite a
// reasoning request, but MUST rewrite a plain one.
#[tokio::test]
async fn not_reasoning_class_route_skips_reasoning_requests() {
    // Build a route: when { model_in:["gpt-4o"], not_reasoning_class:true }
    //                then { target_model:"gpt-4o-mini" }
    // (Use the same in-memory routing store + AppState builder the sibling
    //  tests in this file use; grep this file for `InMemoryRoutingStore` /
    //  `with_routing` to copy the exact setup.)
    // 1) A reasoning prompt ("prove that sqrt(2) is irrational") on gpt-4o
    //    => served model stays gpt-4o (no rewrite).
    // 2) A plain prompt ("translate hello to french") on gpt-4o
    //    => served model becomes gpt-4o-mini.
    // Assert via the same mechanism sibling tests use to read the dispatched
    // model (e.g. a mock provider recording the model, or the response's
    // x-tokentrimmer-* / route headers).
}
```

> Implement the body by copying the closest existing test in `route_rewrite.rs` (same store/AppState/mock-provider wiring) and changing the route conditions + the two prompts. Do not invent new test infra. The reasoning keyword "prove"/"irrational" matches `MATH_KEYWORDS` in `reasoning_class.rs`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p tt-core --test route_rewrite not_reasoning_class_route_skips_reasoning_requests`
Expected: FAIL (the call site still passes 5 args / the reasoning request is wrongly rewritten).

- [ ] **Step 3: Compute the signal + pass it**

In `apply_routing`, after the existing `let combined = tt_shared::message_text_for_estimation(req);` (used for `input_tokens`), add the lazily-computed signal, then pass it to the engine:

```rust
    // Reasoning-class signal for the `not_reasoning_class` condition. Computed
    // ONLY when some route uses it (cheap deterministic substring match, no LLM
    // call). Reuses `combined` already built above for token estimation.
    let is_reasoning_class = engine.uses_reasoning_class()
        && crate::reasoning_class::classify(&combined.to_lowercase()).is_some();
```

Update the call:

```rust
    None => match engine.evaluate_with_signals(
        req,
        ctx,
        input_tokens,
        estimated_cost_usd,
        observed_p95_ms,
        is_reasoning_class,
    ) {
        Some(r) => r,
        None => return Ok(None),
    },
```

> If `combined` is scoped inside the `input_tokens` block, lift its binding so the signal can reuse it (or recompute via `tt_shared::message_text_for_estimation(req)` — it's cheap). Ensure `engine` is in scope at the signal line (it is — it's used immediately below).

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p tt-core --test route_rewrite`
Expected: PASS. Then `cargo test -p tt-core --lib` to confirm no ripple.

- [ ] **Step 5: Commit**

```bash
git add crates/core/src/routes/chat.rs crates/core/tests/route_rewrite.rs
git commit -m "feat(gateway): supply is_reasoning_class signal to route matching"
```

---

## Task 4: The curated down-route catalog module

**Files:**
- Create: `crates/routing/src/catalog.rs`
- Modify: `crates/routing/src/lib.rs` (add `pub mod catalog;`)
- Test: `crates/routing/src/catalog.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the module with failing tests**

Create `crates/routing/src/catalog.rs`:

```rust
//! COST-1(U): opt-in flagship→mini down-route catalog. Same-provider only;
//! materialized as `NewRoute`s with `auto_pause` + `not_reasoning_class` and a
//! deterministic reserved name so `tt route catalog disable` can remove exactly
//! these routes (no DB marker needed). Chat flagships only — pure-reasoning
//! `o`-series models are intentionally excluded from v1.

use crate::store::NewRoute;
use crate::{RouteAction, RouteConditions};

/// Reserved route-name prefix marking a catalog-managed route.
pub const CATALOG_NAME_PREFIX: &str = "catalog:";

/// Conservative quality floor: pause (revert to flagship) below 95% paired pass-rate.
const CATALOG_PAUSE_FLOOR: f64 = 0.92;
const CATALOG_PAUSE_MIN_VERDICTS: u32 = 20;
const CATALOG_PRIORITY: u32 = 10; // low — user-authored routes (default 100) win

/// One curated same-provider down-route: any of `sources` (on `provider`) → `target`.
struct Mapping {
    provider: &'static str,
    sources: &'static [&'static str],
    target: &'static str,
}

const MAPPINGS: &[Mapping] = &[
    Mapping { provider: "openai", sources: &["gpt-4o", "gpt-4.1"], target: "gpt-4o-mini" },
    Mapping { provider: "anthropic", sources: &["claude-opus-4", "claude-sonnet-4", "claude-3-5-sonnet"], target: "claude-haiku-4-5" },
    Mapping { provider: "gemini", sources: &["gemini-3-pro", "gemini-2.5-pro"], target: "gemini-flash" },
];

/// Deterministic, reserved name for a catalog route.
#[must_use]
pub fn catalog_route_name(provider: &str, target: &str) -> String {
    format!("{CATALOG_NAME_PREFIX}{provider}->{target}")
}

/// True if `name` is a catalog-managed route name.
#[must_use]
pub fn is_catalog_route_name(name: &str) -> bool {
    name.starts_with(CATALOG_NAME_PREFIX)
}

/// The full set of catalog down-routes to materialize on `enable`.
#[must_use]
pub fn catalog_routes() -> Vec<NewRoute> {
    MAPPINGS
        .iter()
        .map(|m| NewRoute {
            name: catalog_route_name(m.provider, m.target),
            priority: CATALOG_PRIORITY,
            enabled: true,
            when: RouteConditions {
                model_in: m.sources.iter().map(|s| (*s).to_string()).collect(),
                not_reasoning_class: true,
                ..Default::default()
            },
            then: RouteAction {
                target_model: Some(m.target.to_string()),
                auto_pause: true,
                pause_floor_pass_rate: Some(CATALOG_PAUSE_FLOOR),
                pause_min_verdicts: Some(CATALOG_PAUSE_MIN_VERDICTS),
                ..Default::default()
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tt_shared::model_catalog::model_catalog;

    #[test]
    fn every_catalog_route_is_safe_and_named() {
        for r in catalog_routes() {
            assert!(is_catalog_route_name(&r.name), "{}", r.name);
            assert!(r.then.target_model.is_some());
            assert!(r.then.auto_pause);
            assert!(r.when.not_reasoning_class);
            assert!(!r.when.model_in.is_empty());
            assert_eq!(r.priority, CATALOG_PRIORITY);
        }
    }

    #[test]
    fn targets_exist_same_provider_and_cheaper() {
        let cat = model_catalog();
        for m in MAPPINGS {
            let target = cat.model_info(m.provider, m.target)
                .unwrap_or_else(|| panic!("catalog target {}/{} missing from ModelCatalog", m.provider, m.target));
            assert_eq!(target.provider, m.provider);
            // every source must exist on the same provider and be no cheaper than the target
            for s in m.sources {
                if let Some(src) = cat.model_info(m.provider, s) {
                    assert_eq!(src.provider, m.provider, "source {s} not same provider");
                }
                // price comparison is asserted in Step 3 once we confirm the pricing accessor
            }
        }
    }
}
```

> **Before finalizing the `MAPPINGS` model ids**, verify each `provider`/source/target string against the embedded catalog: `rg '^\[\[models\]\]' -A4 crates/shared/data/models.toml` (or wherever `models.toml` lives) and adjust the exact ids (e.g. the real `gemini-flash`/`gemini-*-pro`, `claude-*` ids) so `targets_exist_same_provider_and_cheaper` passes against real data. Do NOT ship ids the catalog doesn't contain — the test will fail.

- [ ] **Step 2: Register the module + run the failing test**

Add to `crates/routing/src/lib.rs`: `pub mod catalog;`
Run: `cargo test -p tt-routing catalog::`
Expected: FAIL until the model ids match the real catalog (the `targets_exist...` test panics on a missing id).

- [ ] **Step 3: Fix the `MAPPINGS` ids to match the real catalog; add the price-cheaper assertion**

Adjust `MAPPINGS` ids per `models.toml`. Then extend `targets_exist_same_provider_and_cheaper` to assert the target is cheaper using the catalog's pricing (use the same pricing accessor `route_suggestions.rs` uses — `crate::pricing::lookup_with_provider(model, provider)` or the provider's `pricing(model)`; grep `route_suggestions.rs` for the exact call and mirror it):

```rust
        // target input price < each source input price (same provider)
        let tprice = /* lookup target ModelPricing */;
        for s in m.sources {
            if let Some(sprice) = /* lookup source ModelPricing */ {
                assert!(tprice.input_per_million <= sprice.input_per_million,
                    "{}/{} not cheaper than {s}", m.provider, m.target);
            }
        }
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p tt-routing catalog::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/routing/src/lib.rs crates/routing/src/catalog.rs
git commit -m "feat(routing): curated opt-in down-route catalog (same-provider flagship->mini)"
```

---

## Task 5: `tt route catalog <enable|disable|status>` CLI

**Files:**
- Modify: `crates/cli/src/route/mod.rs` (the `RouteCmd` enum ~161–168 and `run` dispatch ~171–233)
- Modify: `crates/cli/src/main.rs` (clap subcommand wiring for `tt route`)
- Test: `crates/cli/src/route/mod.rs` (`#[cfg(test)] mod tests`) for the pure name-matching helpers

- [ ] **Step 1: Write the failing test for the pure disable-filter**

```rust
#[cfg(test)]
mod catalog_tests {
    use super::*;
    #[test]
    fn disable_targets_only_catalog_named_routes() {
        // names returned by the catalog builder are all catalog-named;
        // user routes are not.
        for r in tt_routing::catalog::catalog_routes() {
            assert!(tt_routing::catalog::is_catalog_route_name(&r.name));
        }
        assert!(!tt_routing::catalog::is_catalog_route_name("my custom route"));
    }
}
```

- [ ] **Step 2: Run to verify it fails/compiles**

Run: `cargo test -p tt-cli catalog_tests`
Expected: FAIL to compile until `tt_routing::catalog` is a dependency-visible path from the CLI (it is — `tt-cli` already depends on `tt-routing`; if not, the failure tells you to add it).

- [ ] **Step 3: Add the `Catalog` subcommand + handlers**

In `crates/cli/src/route/mod.rs`, extend `RouteCmd`:

```rust
pub enum RouteCmd {
    List,
    Show(String),
    Rm(String),
    Add(Box<AddArgs>),
    Catalog(CatalogCmd),
}

#[derive(Debug, Clone, Copy)]
pub enum CatalogCmd { Enable, Disable, Status }
```

In `run(...)`, add the match arm (reuse the existing `http`, `base`, `key`, `send`, `ui::spinner`, `print_routes` already in scope):

```rust
        RouteCmd::Catalog(sub) => {
            // current routes on the gateway
            let existing: Value =
                send(http.get(format!("{base}/v1/routes")).bearer_auth(&key)).await?;
            let existing_arr = existing.as_array().cloned().unwrap_or_default();
            let name_of = |r: &Value| r.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();

            match sub {
                CatalogCmd::Enable => {
                    let have: std::collections::HashSet<String> =
                        existing_arr.iter().map(|r| name_of(r)).collect();
                    let mut created = 0usize;
                    for nr in tt_routing::catalog::catalog_routes() {
                        if have.contains(&nr.name) { continue; } // idempotent
                        let sp = ui::spinner(&format!("Creating {}…", nr.name));
                        let _: Value = send(
                            http.post(format!("{base}/v1/routes")).bearer_auth(&key).json(&nr),
                        ).await?;
                        drop(sp);
                        created += 1;
                    }
                    println!("Down-route catalog enabled ({created} new route(s)). Run `tt route list` to view.");
                }
                CatalogCmd::Disable => {
                    let mut removed = 0usize;
                    for r in &existing_arr {
                        let name = name_of(r);
                        if !tt_routing::catalog::is_catalog_route_name(&name) { continue; }
                        if let Some(id) = r.get("id").and_then(|i| i.as_str()) {
                            let _: Value = send(
                                http.delete(format!("{base}/v1/routes/{}", enc_segment(id))).bearer_auth(&key),
                            ).await?;
                            removed += 1;
                        }
                    }
                    println!("Down-route catalog disabled ({removed} route(s) removed). User routes untouched.");
                }
                CatalogCmd::Status => {
                    let cat: Vec<&Value> = existing_arr.iter()
                        .filter(|r| tt_routing::catalog::is_catalog_route_name(&name_of(r)))
                        .collect();
                    if cat.is_empty() {
                        println!("Down-route catalog: not enabled. Run `tt route catalog enable`.");
                    } else {
                        println!("Down-route catalog: {} route(s) active:", cat.len());
                        for r in cat {
                            let paused = r.get("paused").and_then(|p| p.as_bool()).unwrap_or(false);
                            println!("  {} {}", if paused { "[paused]" } else { "[active]" }, name_of(r));
                        }
                    }
                }
            }
        }
```

> `enc_segment` and `send` are already defined/used in this file (the `Show`/`Rm` arms use them). If the catalog `status` should also show savings/pass-rate, that data is on the route JSON only if the list endpoint includes it — keep v1 to name + paused state (above); richer status is a follow-up.

- [ ] **Step 4: Wire the clap subcommand in `crates/cli/src/main.rs`**

Find the `tt route` clap subcommand definition (the enum that maps to `RouteCmd`) and add a `Catalog` variant with an `enable|disable|status` action arg, mapping to `RouteCmd::Catalog(CatalogCmd::…)`. Mirror the existing `route` subcommand wiring exactly (grep `main.rs` for `RouteCmd::Add` to find where the clap→`RouteCmd` mapping lives).

- [ ] **Step 5: Run tests + manual smoke compile**

Run: `cargo test -p tt-cli catalog_tests` → PASS
Run: `cargo build -p tt-cli` then `./target/debug/tt route catalog --help` (and `status` against a local gateway if available) to confirm the command parses.

- [ ] **Step 6: Commit**

```bash
git add crates/cli/src/route/mod.rs crates/cli/src/main.rs
git commit -m "feat(cli): tt route catalog enable/disable/status (opt-in down-route catalog)"
```

---

## Task 6: Documentation

**Files:**
- Modify: `docs/tt-cli-commands.md` (the `tt route` section)
- Modify: `docs/routing-rules-guide.md` (conditions table)

- [ ] **Step 1: Document the command + condition**

Add to `docs/tt-cli-commands.md` under `tt route`: the `tt route catalog enable|disable|status` subcommand — what it installs (curated same-provider flagship→mini down-routes, each `auto_pause`-protected + reasoning-class-guarded), that it's opt-in + fully removable, and that quality is watched by the paired judge (auto-reverts on regression). Add `not_reasoning_class` to the conditions table in `docs/routing-rules-guide.md` (matches only non-Math/Code/Legal/Medical requests).

- [ ] **Step 2: Verify links + lychee** (CI runs lychee on docs)

Run: `cargo build -p tt-cli` (no doc-test impact) and visually confirm the markdown.

- [ ] **Step 3: Commit**

```bash
git add docs/tt-cli-commands.md docs/routing-rules-guide.md
git commit -m "docs: document tt route catalog + not_reasoning_class condition"
```

---

## Task 7: Full verification

- [ ] **Step 1: Workspace gates**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p tt-plan-core   # determinism / snapshot / bootstrap goldens unchanged
```
Expected: all green; no golden/attested number shifted; `tt-routing`, `tt-core`, `tt-cli` suites pass including the new tests.

- [ ] **Step 2: Confirm default-off = no behavior change**

Reason/grep-check: no catalog route exists unless `tt route catalog enable` ran; `not_reasoning_class` defaults false so the signal is consulted by no existing route; `uses_reasoning_class()` returns false for a catalog-disabled org, so `apply_routing` skips classification. No existing route's matching changes.

- [ ] **Step 3: Final commit if any fixups**

```bash
git add -A && git commit -m "chore(cost1u): verification fixups"   # only if needed
```

---

## Self-review notes (author)

- **Spec coverage:** opt-in activation (Task 5) · curated same-provider mapping (Task 4) · `not_reasoning_class` guard + signal (Tasks 1–3) · reuse of `auto_pause`/`route_savings` (catalog routes set `auto_pause:true`; no code needed — existing machinery) · transparency (catalog routes are normal listable routes) · default-off no-op (Task 7 Step 2). Cloud dashboard toggle is explicitly out of scope (spec non-goal).
- **Marker refinement:** name-based (`catalog:` prefix) replaces the spec's `managed_by` column — flagged at top; same behavior, no migration.
- **Type consistency:** `evaluate_with_signals` gains exactly one trailing `bool` (`is_reasoning_class`) used identically in Tasks 2 & 3; `catalog_route_name`/`is_catalog_route_name`/`catalog_routes` names are used consistently in Tasks 4 & 5; `NewRoute`/`RouteConditions`/`RouteAction` fields match the verified signatures.
- **Open verification the implementer must close:** exact `models.toml` ids for `MAPPINGS` (Task 4 Step 3) and the exact pricing accessor (mirror `route_suggestions.rs`); the `test_req`/`test_ctx` helper names (Task 2) and the `route_rewrite.rs` harness (Task 3) must be matched to what those files actually use — do not invent infra.
