# V3d-2b Per-Request Cost Ceiling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `RouteAction.max_cost_usd` — after a route's rewrite, if the rerouted model's estimated cost still exceeds the ceiling, reject with **402 `cost_limit_exceeded`**.

**Architecture:** Extract V3d-2a's cost estimate into a shared helper; carry `max_cost_usd` + the token estimate out of `apply_routing` via `RouteMatch`; the handler re-estimates on the routed model after the V3d-1 provider re-resolve and blocks if over the ceiling. Plan-core mirrors the field and projects a would-block request as unchanged + a caveat.

**Tech Stack:** `tt-routing`, `tt-core`, `tt-plan-core`, `tt-cli`. Spec: `docs/superpowers/specs/2026-06-05-v3d-2b-cost-ceiling-design.md`. Builds on V3d-2a (#20).

---

## Task 1: The ceiling field + gateway gate

**Files:**
- Modify: `crates/routing/src/lib.rs` (`RouteAction.max_cost_usd` + a round-trip test)
- Modify: `crates/core/src/error.rs` (`CostLimitExceeded`)
- Modify: `crates/core/src/routes/chat.rs` (extract helper, `RouteMatch`, handler gate, 2 ApiError match arms)
- Modify: `crates/core/tests/cost_routing.rs` (integration)

- [ ] **Step 1: Write the failing integration test** (append to `crates/core/tests/cost_routing.rs`)

```rust
#[tokio::test]
async fn reroute_then_block_on_ceiling() {
    let served = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(RecordingProvider {
        served: Arc::clone(&served),
    }));
    let raw = InMemoryKeyStore::new();
    let org = Uuid::now_v7();
    let key = issue_key_for(&raw, org).await;
    let key_store: Arc<dyn KeyStore> = Arc::new(raw);

    // Downgrade expensive gpt-4o → gpt-4o-mini, with a tight $0.0008 ceiling on
    // the rerouted cost.
    let backing = Arc::new(InMemoryRoutingStore::new());
    backing.set_routes(
        org,
        vec![Route {
            id: Uuid::now_v7(),
            name: "downgrade-and-cap".into(),
            priority: 100,
            enabled: true,
            when: RouteConditions {
                estimated_cost_gt: Some(0.005),
                ..Default::default()
            },
            then: RouteAction {
                target_model: "gpt-4o-mini".into(),
                fallbacks: Vec::new(),
                force_cache_layer: None,
                disable_cache: false,
                max_cost_usd: Some(0.0008),
            },
        }],
    );
    let routing = Arc::new(CachingRoutingStore::new(backing as Arc<dyn RoutingStore>));
    let app = build_router(
        AppState::new(registry)
            .with_key_store(key_store)
            .with_routing_store(routing),
    );

    // max_tokens=500 → gpt-4o est ≈ $0.0075 (>0.005 → reroute); mini est ≈ $0.0003 (<0.0008 → served).
    let ok = app
        .clone()
        .oneshot(chat_req("gpt-4o", &key, 500))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    assert_eq!(
        ok.headers()["x-tokentrimmer-model-used"].to_str().unwrap(),
        "gpt-4o-mini"
    );

    // max_tokens=2000 → gpt-4o est ≈ $0.030 (reroute); mini est ≈ $0.0012 (>0.0008 → 402).
    let blocked = app.oneshot(chat_req("gpt-4o", &key, 2000)).await.unwrap();
    assert_eq!(blocked.status(), StatusCode::PAYMENT_REQUIRED);
    // The over-budget request was never dispatched.
    assert_eq!(served.lock().unwrap().clone(), vec!["gpt-4o-mini".to_string()]);
}
```

(`chat_req`, `RecordingProvider`, `issue_key_for` already exist in this file from V3d-2a.)

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p tt-core --test cost_routing reroute_then_block_on_ceiling`
Expected: FAIL to compile — `RouteAction` has no `max_cost_usd` field.

- [ ] **Step 3: Add `RouteAction.max_cost_usd` in tt_routing**

In `crates/routing/src/lib.rs`, add to `RouteAction` (after `disable_cache`):

```rust
    /// Hard per-request ceiling (USD). After this route's rewrite, if the
    /// rerouted model's estimated cost still exceeds this, the gateway rejects
    /// the request (402) instead of dispatching. `None` = no ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
```

Add a serde round-trip unit test in the lib's test module:

```rust
    #[test]
    fn max_cost_usd_round_trips_and_omits_when_none() {
        let mut a = make_route("x", 10, vec![], "gpt-4o-mini").then;
        assert!(!serde_json::to_string(&a).unwrap().contains("max_cost_usd"));
        a.max_cost_usd = Some(0.1);
        let j = serde_json::to_string(&a).unwrap();
        assert!(j.contains("\"max_cost_usd\":0.1"));
        let back: RouteAction = serde_json::from_str(&j).unwrap();
        assert_eq!(back.max_cost_usd, Some(0.1));
    }
```

(Verify `make_route(...).then` yields a `RouteAction`; if not, construct one inline.)

- [ ] **Step 4: Add the `CostLimitExceeded` error**

In `crates/core/src/error.rs`, add to the enum (after `MissingProviderCredential`):

```rust
    #[error("estimated cost ${estimated_usd:.4} exceeds the ${ceiling_usd:.4} per-request ceiling")]
    CostLimitExceeded { estimated_usd: f64, ceiling_usd: f64 },
```

And the `into_response` arm (after the `MissingProviderCredential` arm):

```rust
            ApiError::CostLimitExceeded { estimated_usd, ceiling_usd } => (
                StatusCode::PAYMENT_REQUIRED,
                "billing_error",
                "cost_limit_exceeded",
                format!(
                    "Estimated request cost ${estimated_usd:.4} exceeds the configured ${ceiling_usd:.4} per-request ceiling."
                ),
            ),
```

- [ ] **Step 5: Extract the estimate helper + thread `max_cost_usd` through `RouteMatch`**

In `crates/core/src/routes/chat.rs`:

Add a free helper near `DEFAULT_OUTPUT_TOKENS_ESTIMATE`:

```rust
/// Estimated request cost (USD): input tokens at the input rate + output tokens
/// (from `max_tokens`, else the default) at the output rate.
fn estimate_cost_usd(pricing: &ModelPricing, input_tokens: u32, max_tokens: Option<u32>) -> f64 {
    let output_est = max_tokens.unwrap_or(DEFAULT_OUTPUT_TOKENS_ESTIMATE);
    (f64::from(input_tokens) * pricing.input_per_million
        + f64::from(output_est) * pricing.output_per_million)
        / 1_000_000.0
}
```

(`ModelPricing` is already imported in chat.rs.)

Refactor `apply_routing`'s inline estimate to use it:

```rust
    let estimated_cost_usd = req_provider
        .as_ref()
        .and_then(|p| p.pricing(&req.model))
        .map(|pr| estimate_cost_usd(&pr, input_tokens, req.max_tokens));
```

Extend `RouteMatch` (struct at ~line 1673):

```rust
struct RouteMatch {
    route_id: Uuid,
    fallbacks: Vec<String>,
    disable_cache: bool,
    max_cost_usd: Option<f64>,
    input_tokens_estimate: u32,
}
```

In `apply_routing`, capture `let max_cost_usd = m.then.max_cost_usd;` alongside `fallbacks`/`disable_cache`, and add both fields to the returned `RouteMatch { … }`:

```rust
    Some(RouteMatch {
        route_id,
        fallbacks,
        disable_cache,
        max_cost_usd,
        input_tokens_estimate: input_tokens,
    })
```

- [ ] **Step 6: Add the ceiling gate in the handler**

In `chat.rs::handler`, before `route_match` is moved (it currently ends with `let route_fallbacks = route_match.map(|m| m.fallbacks)…`), capture the two new fields:

```rust
    let route_max_cost_usd = route_match.as_ref().and_then(|m| m.max_cost_usd);
    let route_input_tokens = route_match.as_ref().map(|m| m.input_tokens_estimate).unwrap_or(0);
```

Then inside the existing `if matched_route_id.is_some() { … }` block, AFTER the V3d-1 credential re-resolution, add the ceiling check (provider is now the routed provider):

```rust
        // Per-request cost ceiling (V3d-2b): reject when the rerouted model's
        // estimated cost still exceeds the route's max_cost_usd. Permissive when
        // pricing is unknown (can't prove an exceedance).
        if let Some(ceiling) = route_max_cost_usd {
            if let Some(pr) = provider.pricing(&req.model) {
                let routed_cost = estimate_cost_usd(&pr, route_input_tokens, req.max_tokens);
                if routed_cost > ceiling {
                    return Err(ApiError::CostLimitExceeded {
                        estimated_usd: routed_cost,
                        ceiling_usd: ceiling,
                    });
                }
            }
        }
```

Add the two exhaustiveness arms (compiler will flag them):
- `is_deterministic_client_error` — group with `MissingProviderCredential` (config-dependent → `false`, don't negative-cache): add `| ApiError::CostLimitExceeded { .. }`.
- `error_status_code` — add `ApiError::CostLimitExceeded { .. } => StatusCode::PAYMENT_REQUIRED,`.

- [ ] **Step 7: Run to verify green**

Run: `cargo test -p tt-routing && cargo test -p tt-core --test cost_routing --test route_rewrite`
Expected: PASS — reroute-then-fit served (200, gpt-4o-mini), reroute-then-block 402, never dispatched; existing routing/cost tests unaffected.

- [ ] **Step 8: Commit**

```bash
git add crates/routing/src/lib.rs crates/core/src/error.rs crates/core/src/routes/chat.rs crates/core/tests/cost_routing.rs
git commit -m "feat(routing): RouteAction.max_cost_usd per-request ceiling (402 when exceeded)"
```

---

## Task 2: CLI `--max-cost`

**Files:**
- Modify: `crates/cli/src/route/mod.rs` (`AddArgs`, `build_new_route`, test)
- Modify: `crates/cli/src/main.rs` (clap arg + dispatch)

- [ ] **Step 1: Write the failing CLI test** (in `crates/cli/src/route/mod.rs` tests)

```rust
    #[test]
    fn max_cost_maps_to_then() {
        let body = build_new_route(&AddArgs {
            always: Some("gpt-4o-mini".into()),
            from: None,
            to: None,
            when_has_images: false,
            when_has_audio: false,
            when_tag: None,
            when_prompt_contains: vec![],
            when_cost_gt: None,
            when_cost_lt: None,
            max_cost: Some(0.1),
            disable_cache: false,
            priority: 100,
            name: None,
            fallback: vec![],
            disabled: false,
        })
        .unwrap();
        assert_eq!(body["then"]["max_cost_usd"], 0.1);
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p tt-cli max_cost_maps_to_then`
Expected: FAIL to compile — `max_cost` not on `AddArgs`.

- [ ] **Step 3: Add the field + mapping + clap arg**

`crates/cli/src/route/mod.rs` — add `pub max_cost: Option<f64>,` to `AddArgs` (after `when_cost_lt`). In `build_new_route`, after the `disable_cache` insert in `then`:

```rust
    if let Some(v) = args.max_cost {
        then.insert("max_cost_usd".into(), json!(v));
    }
```

Add `max_cost: None,` to every existing `AddArgs` literal in this file's tests (compiler-driven).

`crates/cli/src/main.rs` — add the clap arg to `Route::Add` (after `when_cost_lt`):

```rust
        /// Reject (402) any matched request whose estimated cost exceeds this (USD).
        #[arg(long)]
        max_cost: Option<f64>,
```

Thread `max_cost,` into the destructure + the `AddArgs { … }` construction (both, after `when_cost_lt`).

- [ ] **Step 4: Run to verify green**

Run: `cargo test -p tt-cli && cargo run -q -p tt-cli -- route add --help`
Expected: PASS; `--max-cost <MAX_COST>` in help.

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/route/mod.rs crates/cli/src/main.rs
git commit -m "feat(cli): tt route add --max-cost (per-request ceiling)"
```

---

## Task 3: Plan-core mirror + would-block caveat

**Files:**
- Modify: `crates/plan-core/src/types.rs` (`RouteAction.max_cost_usd`)
- Modify: `crates/plan-core/src/replay.rs` (would-block handling + caveat)
- Modify: `crates/plan-core/tests/replay.rs` (test)
- Modify: `crates/plan-core/src/apply.rs` (fix any explicit `RouteAction` literal — compiler-driven)

- [ ] **Step 1: Write the failing replay test** (append to `crates/plan-core/tests/replay.rs`)

```rust
#[test]
fn route_over_ceiling_is_blocked_not_saved() {
    // A request routed to a model whose projected cost exceeds max_cost_usd is
    // counted unchanged (no fabricated savings) and surfaced as a would-block.
    let req = make_req(1, 0, "claude-3-5-sonnet", 1_000_000, 1_000_000, 18.0, false);
    let route = ProposedRoute {
        id: det_uuid(100),
        name: "capped".into(),
        priority: 100,
        enabled: true,
        when: RouteConditions::default(),
        then: RouteAction {
            target_model: "claude-3-5-haiku".into(),
            force_cache_layer: None,
            fallbacks: Vec::new(),
            disable_cache: false,
            max_cost_usd: Some(0.01), // haiku on 1M/1M still far exceeds $0.01
        },
    };
    let mut pricing = HashMap::new();
    let (k, v) = pricing_with("anthropic", "claude-3-5-haiku", 0.25, 1.25);
    pricing.insert(k, v);

    let result = replay(input_with_routes(vec![req], vec![route], pricing, 100)).unwrap();
    // Blocked → projected unchanged (no savings), and a caveat names it.
    assert_eq!(result.aggregates.projected_savings_usd, 0.0);
    assert!(result.caveats.iter().any(|c| c.contains("rejected")));
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p tt-plan-core --test replay route_over_ceiling_is_blocked_not_saved`
Expected: FAIL to compile — plan-core `RouteAction` has no `max_cost_usd`.

- [ ] **Step 3: Mirror the field + block handling**

`crates/plan-core/src/types.rs` `RouteAction` — add (after `disable_cache`, keeping field order matching `tt_routing::RouteAction`):

```rust
    /// Mirror of `tt_routing::RouteAction::max_cost_usd`. A matched request whose
    /// projected cost exceeds this would be rejected at runtime — replay counts
    /// it unchanged (no savings) and surfaces a caveat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_usd: Option<f64>,
```

`crates/plan-core/src/replay.rs` `project_requests` — in the `Some(route)` arm, after `projected_cost` is computed but before it's recorded, intercept a would-block:

```rust
                    // Per-request ceiling: a projected cost over max_cost_usd would
                    // be rejected at runtime — never claim it as a saving.
                    let blocked = route
                        .then
                        .max_cost_usd
                        .is_some_and(|c| projected_cost > c);
                    let projected_cost = if blocked { req.cost_usd } else { projected_cost };
                    if blocked {
                        would_block += 1;
                    }
```

Add a `let mut would_block: u32 = 0;` accumulator at the top of `project_requests` and carry it into the `Projection` struct (new field), then into a caveat in `build_caveats`:

```rust
    if would_block > 0 {
        caveats.push(format!(
            "{would_block} request(s) would be rejected by a max_cost_usd ceiling — counted unchanged, not as savings."
        ));
    }
```

(Thread `would_block` through `Projection` + `build_caveats`'s signature like `latency_unprojected`. No new `Aggregates` field → snapshot stays byte-identical for fixtures without `max_cost_usd`.)

NOTE: verify exactly where `projected_cost` is finalized in the current `project_requests` (post-V3d-1); place the interception so it applies to the rerouted projected cost and the per-route bucket records the (possibly reverted) `projected_cost` consistently.

- [ ] **Step 4: Run to verify green + determinism intact**

Run: `cargo test -p tt-plan-core`
Expected: PASS — new test green; `snapshot_canned_replay` + determinism byte-identical.

- [ ] **Step 5: Commit**

```bash
git add crates/plan-core/src/types.rs crates/plan-core/src/replay.rs crates/plan-core/tests/replay.rs crates/plan-core/src/apply.rs
git commit -m "feat(plan-core): mirror max_cost_usd; project would-block requests unchanged + caveat"
```

---

## Task 4: Final verification

- [ ] **Step 1: fmt + workspace clippy + tests**

```bash
cargo fmt -p tt-routing -p tt-core -p tt-plan-core -p tt-cli
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p tt-routing -p tt-plan-core -p tt-cli
cargo test -p tt-core --test cost_routing --test route_rewrite --test routes_api --test cross_provider
```
Expected: fmt clean, clippy clean (workspace), all green.

- [ ] **Step 2: Commit any fmt changes**

```bash
git commit -am "style: cargo fmt (v3d-2b)" || echo "nothing to commit"
```

---

## Self-review notes
- **Coupling:** `RouteAction.max_cost_usd` (tt_routing) + the gateway gate (which reads it via `RouteMatch`) are in Task 1 together so the crate compiles.
- **Reuse:** `estimate_cost_usd` is the single definition for the pre-rewrite condition (V3d-2a) and the post-rewrite ceiling (V3d-2b).
- **Permissive on unknown pricing:** unknown routed pricing → no block (can't prove exceedance), consistent with V3d-2a.
- **Plan honesty:** a would-block request is projected unchanged (never a fabricated saving) + a caveat; no new `Aggregates` field → determinism snapshot byte-identical.
- **402 distinctness:** `cost_limit_exceeded` (billing_error) is distinct from the subscription `PaymentRequired` (`subscription_required`).
- **Type consistency:** `max_cost_usd: Option<f64>` mirrored across `tt_routing`/`tt_plan_core` `RouteAction`; `estimate_cost_usd(pricing, input_tokens, max_tokens)` signature shared.
