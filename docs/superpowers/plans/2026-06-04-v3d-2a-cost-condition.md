# V3d-2a Cost-Condition Routing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `estimated_cost_gt` / `estimated_cost_lt` route conditions so expensive requests reroute to a cheaper model.

**Architecture:** Dollar parallel of `input_tokens_gt/_lt`. The gateway computes the estimate (`input_tokens × input_rate + (max_tokens || 1000) × output_rate`) on the originally-requested model and passes it to `engine.evaluate`. Plan-core mirrors the condition and matches on the logged `baseline_cost_usd`.

**Tech Stack:** `tt-routing`, `tt-core` (axum gateway), `tt-plan-core`, `tt-cli`. Spec: `docs/superpowers/specs/2026-06-04-v3d-2a-cost-condition-design.md`.

---

## Task 1: Cost condition in the engine + gateway

The `evaluate` signature change and its single prod caller (`apply_routing`) are coupled, so they land together.

**Files:**
- Modify: `crates/routing/src/lib.rs` (`RouteConditions`, `evaluate`, `matches`, tests)
- Modify: `crates/core/src/routes/chat.rs` (`apply_routing` estimate + `DEFAULT_OUTPUT_TOKENS_ESTIMATE`)
- Create: `crates/core/tests/cost_routing.rs` (integration)

- [ ] **Step 1: Write the failing matcher unit test** (in `crates/routing/src/lib.rs` tests module)

```rust
    #[test]
    fn cost_gt_matches_above_threshold_only() {
        let eng = RoutingEngine::with_routes([route_to(
            "expensive",
            100,
            RouteConditions {
                estimated_cost_gt: Some(0.02),
                ..Default::default()
            },
        )]);
        // est_cost 0.03 > 0.02 → match; 0.01 !> 0.02 → no match.
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100, 0.03)
            .is_some());
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100, 0.01)
            .is_none());
    }

    #[test]
    fn cost_lt_and_model_in_anded() {
        let eng = RoutingEngine::with_routes([route_to(
            "cheap-small",
            100,
            RouteConditions {
                model_in: vec!["gpt-4o".into()],
                estimated_cost_lt: Some(0.05),
                ..Default::default()
            },
        )]);
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100, 0.01)
            .is_some());
        // cost not below threshold → no match
        assert!(eng
            .evaluate(&make_req("gpt-4o"), &make_ctx(None), 100, 0.09)
            .is_none());
        // wrong model → no match
        assert!(eng
            .evaluate(&make_req("claude-x"), &make_ctx(None), 100, 0.01)
            .is_none());
    }
```

NOTE: confirm the existing test helper name for constructing a route (e.g. `route_to(name, priority, when)` or inline `Route {…}`); match whatever the lib's test module already uses. If no helper exists, build the `Route` inline mirroring the other tests.

- [ ] **Step 2: Run to verify it fails (compile error: unknown field + arity)**

Run: `cargo test -p tt-routing cost_gt_matches_above_threshold_only`
Expected: FAIL to compile — `estimated_cost_gt` unknown and `evaluate` takes 3 args not 4.

- [ ] **Step 3: Add the fields + thread the estimate**

In `crates/routing/src/lib.rs`, add to `RouteConditions` (after `prompt_contains_any_of`):

```rust
    /// Match only if the request's estimated cost (USD) is greater than this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_gt: Option<f64>,
    /// Match only if the request's estimated cost (USD) is less than this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_lt: Option<f64>,
```

Change `evaluate` and `matches` to thread the estimate:

```rust
    pub fn evaluate(
        &self,
        req: &ChatCompletionRequest,
        ctx: &RequestContext,
        input_tokens_estimate: u32,
        estimated_cost_usd: f64,
    ) -> Option<&Route> {
        self.routes
            .iter()
            .find(|r| r.enabled && matches(r, req, ctx, input_tokens_estimate, estimated_cost_usd))
    }
```

```rust
fn matches(
    r: &Route,
    req: &ChatCompletionRequest,
    ctx: &RequestContext,
    input_tokens: u32,
    estimated_cost_usd: f64,
) -> bool {
```

Add the cost arms (after the `input_tokens_gt` arm, before `tag_equals`):

```rust
    if let Some(t) = c.estimated_cost_gt {
        if estimated_cost_usd <= t {
            return false;
        }
    }
    if let Some(t) = c.estimated_cost_lt {
        if estimated_cost_usd >= t {
            return false;
        }
    }
```

- [ ] **Step 4: Update every existing `evaluate(...)` call in the lib's test module**

Add a `, 0.0` cost arg to each existing `.evaluate(req, ctx, <tokens>)` call in `crates/routing/src/lib.rs` (none of them set a cost condition, so `0.0` is inert). The compiler lists each site; append `, 0.0` before the closing `)`.

- [ ] **Step 5: Run the tt-routing tests**

Run: `cargo test -p tt-routing`
Expected: PASS — new cost tests green, all existing tests green.

- [ ] **Step 6: Write the failing gateway integration test**

Create `crates/core/tests/cost_routing.rs`. Reuse the `route_rewrite.rs` harness shape (a `RecordingProvider` with model-aware pricing `gpt-4o = 5/15`, `gpt-4o-mini = 0.15/0.6`, serving both models + recording served models; an `InMemoryRoutingStore`; an issued key). Plant a route `when { estimated_cost_gt: 0.02 } → gpt-4o-mini`. Send two requests:

```rust
// Helper: chat request with an explicit max_tokens (drives the output estimate).
fn chat_req(model: &str, bearer: &str, max_tokens: u32) -> Request<Body> {
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hi" }],
        "max_tokens": max_tokens,
        "stream": false,
    });
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {bearer}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn expensive_request_reroutes_cheap_one_passes_through() {
    // <build app with RecordingProvider(gpt-4o, gpt-4o-mini) + route
    //  when{estimated_cost_gt:0.02} -> gpt-4o-mini, mirroring route_rewrite.rs>

    // Expensive: max_tokens=2000 on gpt-4o → est ≈ (few×5 + 2000×15)/1e6 ≈ $0.030 > 0.02 → reroute.
    let r1 = app.clone().oneshot(chat_req("gpt-4o", &key, 2000)).await.unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    assert_eq!(r1.headers()["x-tokentrimmer-model-used"].to_str().unwrap(), "gpt-4o-mini");

    // Cheap: max_tokens=100 on gpt-4o → est ≈ (few×5 + 100×15)/1e6 ≈ $0.0015 < 0.02 → pass through.
    let r2 = app.oneshot(chat_req("gpt-4o", &key, 100)).await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    assert_eq!(r2.headers()["x-tokentrimmer-model-used"].to_str().unwrap(), "gpt-4o");
}
```

NOTE: build the app exactly like `route_rewrite.rs::route_rewrites_model_when_org_has_matching_rule` (registry + key store + routing store, no credential store needed). Verify `x-tokentrimmer-model-used` is the served-model header (it is, per `route_rewrite.rs`).

- [ ] **Step 7: Run to verify it fails**

Run: `cargo test -p tt-core --test cost_routing`
Expected: FAIL — `apply_routing` still calls `evaluate` with 3 args (won't compile) / the route never fires.

- [ ] **Step 8: Compute + pass the estimate in `apply_routing`**

In `crates/core/src/routes/chat.rs`, add near the other consts (top of file):

```rust
/// Pre-flight output-token estimate when the request doesn't set `max_tokens`.
/// Used only to estimate request cost for cost-based route conditions.
const DEFAULT_OUTPUT_TOKENS_ESTIMATE: u32 = 1000;
```

Replace the provider/estimate block in `apply_routing`:

```rust
    let req_provider = state.registry.resolve(&req.model);
    let provider_id = req_provider.as_ref().map(|p| p.id()).unwrap_or("");
    let input_tokens = last_user_message_text(req)
        .map(|s| tt_tokenize::estimate_tokens(provider_id, s))
        .unwrap_or(0);
    // Estimated request cost (USD) on the originally-requested model, for
    // cost-based route conditions. Output tokens are unknown pre-flight, so use
    // `max_tokens` when set else a default. Unknown pricing → 0 (cost conditions
    // stay permissive, mirroring other unknown-data conditions).
    let estimated_cost_usd = req_provider
        .as_ref()
        .and_then(|p| p.pricing(&req.model))
        .map(|pr| {
            let output_est = req.max_tokens.unwrap_or(DEFAULT_OUTPUT_TOKENS_ESTIMATE);
            (f64::from(input_tokens) * pr.input_per_million
                + f64::from(output_est) * pr.output_per_million)
                / 1_000_000.0
        })
        .unwrap_or(0.0);

    let m = engine.evaluate(req, ctx, input_tokens, estimated_cost_usd)?;
```

(`p.pricing(&req.model)` returns `Option<tt_shared::ModelPricing>` with `input_per_million` / `output_per_million`.)

- [ ] **Step 9: Run both crates to verify green**

Run: `cargo test -p tt-routing && cargo test -p tt-core --test cost_routing --test route_rewrite`
Expected: PASS — expensive request reroutes, cheap passes through, existing route tests unaffected.

- [ ] **Step 10: Commit**

```bash
git add crates/routing/src/lib.rs crates/core/src/routes/chat.rs crates/core/tests/cost_routing.rs
git commit -m "feat(routing): estimated_cost_gt/lt route conditions (cost-based reroute)"
```

---

## Task 2: Plan-core mirror (projectable via baseline_cost_usd)

**Files:**
- Modify: `crates/plan-core/src/types.rs` (`RouteConditions`)
- Modify: `crates/plan-core/src/routing.rs` (`matches_conditions` + a test)

- [ ] **Step 1: Write the failing replay/match test** (in `crates/plan-core/src/routing.rs` tests module)

```rust
    #[test]
    fn cost_gt_matches_on_baseline_cost() {
        let r = route(
            "expensive",
            10,
            true,
            RouteConditions {
                estimated_cost_gt: Some(0.02),
                ..Default::default()
            },
        );
        // baseline_cost_usd 0.03 > 0.02 → match; 0.01 → no match.
        let mut hi = req("m", 100, None);
        hi.baseline_cost_usd = 0.03;
        let mut lo = req("m", 100, None);
        lo.baseline_cost_usd = 0.01;
        assert!(match_route(&hi, std::slice::from_ref(&r)).is_some());
        assert!(match_route(&lo, &[r]).is_none());
    }
```

NOTE: confirm the test-module helpers `route(name, priority, enabled, when)` and `req(model, tokens, tag)` exist in `routing.rs` tests (they do per the modality tests); `req` returns a `RequestLog` whose `baseline_cost_usd` field is settable.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p tt-plan-core --lib cost_gt_matches_on_baseline_cost`
Expected: FAIL to compile — `estimated_cost_gt` unknown on plan-core `RouteConditions`.

- [ ] **Step 3: Add the mirror fields + matcher arms**

In `crates/plan-core/src/types.rs` `RouteConditions`, add (after the existing fields):

```rust
    /// Mirror of `tt_routing::RouteConditions::estimated_cost_gt`. Evaluated
    /// against `RequestLog.baseline_cost_usd` (the request's logged cost on its
    /// original model) — accurately projectable, unlike modality/topic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_gt: Option<f64>,
    /// Mirror of `tt_routing::RouteConditions::estimated_cost_lt`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_lt: Option<f64>,
```

In `crates/plan-core/src/routing.rs` `matches_conditions`, add (after the `input_tokens_gt` arm):

```rust
    if let Some(t) = c.estimated_cost_gt {
        if req.baseline_cost_usd <= t {
            return false;
        }
    }
    if let Some(t) = c.estimated_cost_lt {
        if req.baseline_cost_usd >= t {
            return false;
        }
    }
```

- [ ] **Step 4: Run to verify green + determinism intact**

Run: `cargo test -p tt-plan-core`
Expected: PASS — new test green; `snapshot_canned_replay` + determinism tests byte-identical (new `Option` fields skip-serialize when `None`).

- [ ] **Step 5: Commit**

```bash
git add crates/plan-core/src/types.rs crates/plan-core/src/routing.rs
git commit -m "feat(plan-core): mirror estimated_cost_gt/lt (matches logged baseline_cost_usd)"
```

---

## Task 3: CLI flags

**Files:**
- Modify: `crates/cli/src/route/mod.rs` (`AddArgs`, `build_new_route`, tests)
- Modify: `crates/cli/src/main.rs` (clap args + dispatch)

- [ ] **Step 1: Write the failing CLI mapping test** (in `crates/cli/src/route/mod.rs` tests)

```rust
    #[test]
    fn cost_conditions_map_through() {
        let body = build_new_route(&AddArgs {
            always: Some("gpt-4o-mini".into()),
            from: None,
            to: None,
            when_has_images: false,
            when_has_audio: false,
            when_tag: None,
            when_prompt_contains: vec![],
            when_cost_gt: Some(0.05),
            when_cost_lt: None,
            disable_cache: false,
            priority: 100,
            name: None,
            fallback: vec![],
            disabled: false,
        })
        .unwrap();
        assert_eq!(body["when"]["estimated_cost_gt"], 0.05);
        assert!(body["when"].get("estimated_cost_lt").is_none());
    }
```

NOTE: match the exact current `AddArgs` field set (verify against the file — it has `when_prompt_contains` etc. from V3c); add the two new fields in the same positions you add them to the struct.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p tt-cli cost_conditions_map_through`
Expected: FAIL to compile — `when_cost_gt`/`when_cost_lt` not on `AddArgs`.

- [ ] **Step 3: Add the fields + mapping + clap args**

`crates/cli/src/route/mod.rs` — add to `AddArgs` (after `when_prompt_contains`):

```rust
    pub when_cost_gt: Option<f64>,
    pub when_cost_lt: Option<f64>,
```

In `build_new_route`, after the existing `when` inserts:

```rust
    if let Some(v) = args.when_cost_gt {
        when.insert("estimated_cost_gt".into(), json!(v));
    }
    if let Some(v) = args.when_cost_lt {
        when.insert("estimated_cost_lt".into(), json!(v));
    }
```

Update the existing `build_new_route` tests' `AddArgs` literals to add `when_cost_gt: None, when_cost_lt: None` (compiler-driven).

`crates/cli/src/main.rs` — add clap args to `Route::Add` (after `when_prompt_contains`):

```rust
        #[arg(long)]
        when_cost_gt: Option<f64>,
        #[arg(long)]
        when_cost_lt: Option<f64>,
```

And thread them into the `AddArgs { … }` construction in the dispatch match (add `when_cost_gt, when_cost_lt,` alongside `when_prompt_contains`).

- [ ] **Step 4: Run to verify green**

Run: `cargo test -p tt-cli && cargo run -p tt-cli -- route add --help`
Expected: PASS; `--when-cost-gt <WHEN_COST_GT>` and `--when-cost-lt` appear in help.

- [ ] **Step 5: Commit**

```bash
git add crates/cli/src/route/mod.rs crates/cli/src/main.rs
git commit -m "feat(cli): tt route add --when-cost-gt/--when-cost-lt"
```

---

## Task 4: Final verification

- [ ] **Step 1: fmt + workspace clippy + tests**

```bash
cargo fmt -p tt-routing -p tt-core -p tt-plan-core -p tt-cli
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p tt-routing -p tt-plan-core -p tt-cli
cargo test -p tt-core --test cost_routing --test route_rewrite --test routes_api
```
Expected: fmt clean, clippy clean (workspace — catches any missed `evaluate` call site), all tests green.

- [ ] **Step 2: Commit any fmt changes**

```bash
git commit -am "style: cargo fmt (v3d-2a)" || echo "nothing to commit"
```

---

## Self-review notes
- **Coupling:** the `evaluate` signature change + its one prod caller (`apply_routing`) are in Task 1 together so no crate is left non-compiling between tasks.
- **Permissive on unknown pricing:** unknown model → `estimated_cost_usd = 0.0` → cost conditions don't fire (consistent with the engine's other "unknown data → don't match" stances).
- **Plan accuracy:** plan-core matches on `baseline_cost_usd` (real logged cost) — no estimation, no caveat.
- **Determinism:** new `Option<f64>` fields use `skip_serializing_if = "Option::is_none"` → existing snapshots byte-identical.
- **Type consistency:** `estimated_cost_gt/lt: Option<f64>` everywhere; gateway passes `estimated_cost_usd: f64` to `evaluate`; matcher uses `<=`/`>=` for the false-branch exactly like `input_tokens_gt/lt`.
