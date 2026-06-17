# PROD-1: Close the Inspect → Plan → apply proof loop

**Status:** approved design (2026-06-16) · **Repo:** public OSS core · **Origin:** `COMPREHENSIVE_REVIEW_2026-06-15.md` finding `PROD-1` ("the verified-savings proof loop has stubbed handoffs — a toolbox, not one motion").

## Problem

TokenTrimmer's wedge is "verified savings as a financial product." That only holds if a buyer can experience **discover → simulate → realize → prove** as one motion. Today two handoffs are stubbed:

1. **Inspect → Plan (telemetry):** `tt inspect --suggest-plan` emits a *non-runnable* `PlanInput` skeleton — `org_id` is `Uuid::nil()` and `requests: []`. The user must hand-edit it, and there is no documented way to get real `request_logs` rows into the file. So the simulator has nothing to replay.
2. **Plan → apply (realize):** `tt plan --apply` is an honest stub — it prints the projection, then exits non-zero with "needs the hosted backend + a `tt_live_*` key." The realize step can't be done self-serve.

(The third historical gap — Plan under-counting because it round-tripped but didn't project levers — was already closed by `COST-7(U)`: `flex`/L1-cache/`target_model` are projected; `diff`/`minify` carry realized values. This spec does **not** touch projection.)

## Key insight

Both handoffs were assumed to be hosted-SaaS-only. They are not. The `tt` CLI **already** depends on `tt-core`, `sqlx`, `tt-telemetry` (feature `postgres`), and `tt-routing` (feature `postgres`) — it runs `tt gateway` in-process. Both `request_logs` and `routes` live in the **gateway's Postgres**. So for a **self-hosted gateway (your own `DATABASE_URL`)**, reading a telemetry window and writing routes are operations the OSS CLI can already perform. Closing the loop is pure OSS work: **no new cloud endpoint, no `tt_live_*` API, and it sidesteps the billing-dark cloud CI entirely.**

## Goal

Make the loop close self-serve for a DB-connected (self-hosted) gateway:

```bash
tt inspect --suggest-plan . --from-db --org <id> --window-days 7 -o plan.json   # discover + freeze a real window
tt plan --input plan.json                                                        # simulate (pure function of the file)
tt plan --input plan.json --apply                                                # realize (write the routes table)
tt audit verify                                                                  # prove (plan.applied chain row)
```

## Non-goals (v1)

- **No cloud endpoint / no `tt_live_*` hosted-API pull or apply.** Deferred — keeps this OSS-local and avoids billing-dark cloud CI. (A hosted convenience endpoint that pulls/apply via key, without exposing the DB connection string, is a clean v2 cloud follow-up.)
- **No L2-semantic or quality-scoring enrichment join.** `embedding` (L2 cache table) and `body`/`response_body` (opt-in body-capture table) are not joined in v1; the window maps base `request_logs` columns only. v1 therefore projects `target_model` + `flex` + L1-cache. L2-semantic projection and Tier-3 quality scoring are a v2 join.
- **No `routes`-table schema change.** Uses `tt-routing::store` as-is.
- **No change to the replay/projection engine** (`COST-7(U)` already did that).
- **No change to default behavior.** Without the new opt-in flags, `tt inspect --suggest-plan` and `tt plan` behave exactly as today.

## Reused existing assets

- `crates/cli` — already has `sqlx`, `tt-telemetry[postgres]`, `tt-routing[postgres]`, `tt-core`, `tt-config`; the `Config` struct exposes `database_url`.
- `crates/plan-core` — `PlanInput`, `RequestLog`, `replay()`.
- `crates/routing` — `validate_route_has_effect` (re-exported pub fn, the no-op guard the catalog/`add_route` use), and the `RoutingStore` trait impl `PostgresRoutingStore` with `create_route(org_id, NewRoute) -> Route` and `list_all_for_org(org_id) -> Vec<Route>` (the `routes`-table store the gateway refreshes ~every 60s). No new store method is needed.
- `crates/cli/src/audit.rs` + the telemetry audit chain — emit/verify `plan.applied`.
- `request_logs` table — indexed `request_logs_org_ts (org_id, ts DESC)`, ideal for the windowed pull.

## Design

### Handoff 1 — runnable PlanInput: `tt inspect --suggest-plan --from-db`

Add opt-in flags to the existing `Inspect` subcommand / `--suggest-plan` path:

- `--from-db` — opt in to pulling a real telemetry window (default off; behavior unchanged when absent).
- `--org <uuid>` — the tenant to pull.
- `--window-days <N>` — window size, default `7` (overrides the existing skeleton default).

When `--from-db` is set, after generating `proposed_routes` + `pricing` from the code scan as today, the handler connects via `config.database_url` (the same `DATABASE_URL` the gateway uses) and pulls the `request_logs` window into `requests`, **materializing and freezing the rows into the emitted `PlanInput`**. The output file is then immediately runnable and reproducible — `tt plan --input` is a pure function of it.

**Org resolution:** `--org` is explicit. If omitted, auto-detect the single distinct `org_id` present in the window; if 0 rows or >1 distinct org, error and list what was found (forces an explicit `--org` — never silently pulls the wrong tenant).

**Window bounds:** `window_end` = now (UTC); `window_start` = now − `window_days`. These also populate `PlanInput.window_start` / `window_end`.

**Column → struct mapping** (`request_logs` → `plan_core::RequestLog`):

| `request_logs` column | `RequestLog` field | Note |
|---|---|---|
| `id, org_id, ts, provider, model` | same | direct |
| `input_tokens, output_tokens, cached_tokens` | same | direct |
| `cost_usd::float8` | `cost_usd` | **NUMERIC → cast to float8** |
| `baseline_cost_usd::float8` | `baseline_cost_usd` | **NUMERIC → cast to float8** |
| `cached, cache_layer` | same | direct |
| `route_id` | `matched_route_id` | renamed |
| `latency_ms, upstream_latency_ms, status, tag` | same | direct |
| — | `task_class` | `L2TaskClass::default()` (ChatCompletions); base table has no column |
| — | `embedding, finish_reason, body, response_body` | `None` (v2 enrichment join) |
| — | `diff_saved_usd, minify_saved_est_usd` | `None` (realized-lever values; absent ⇒ no projection, the existing conservative behavior) |

> **NUMERIC decode landmine:** `cost_usd` / `baseline_cost_usd` are `NUMERIC(12,6)`. Decoding directly into `f64` errors (this is exactly the `DB-2` reconciliation bug). The SELECT MUST cast `cost_usd::float8, baseline_cost_usd::float8`. A regression test asserts a real-valued window decodes.

### Handoff 2 — real local `tt plan --apply`

Replace the honest non-zero stub in `run_plan()`. When `--apply` is passed, after computing + printing the projection:

1. **Validate** each `proposed_route` client-side via `tt_routing::validate_route_has_effect(&then)` (reject no-ops — the same guard `tt route catalog` / `add_route` use).
2. **Dry-run by default:** print the exact set of routes that *would* be written and require interactive confirmation. `--yes` skips the prompt (for automation/CI).
3. On confirm, **write the routes to the `routes` table** via `PostgresRoutingStore::create_route` (the `RoutingStore` trait), **idempotently** — first `list_all_for_org(org_id)` and skip any route whose `name` already exists (mirroring `tt route catalog enable`). The gateway picks them up on its next ~60s refresh. No running gateway is required; this is a direct DB write.
4. **Emit a signed `plan.applied` audit row** (carrying `plan_id`, the applied route names, and the projected savings) to the local audit chain `.claude/AUDIT-CHAIN.jsonl`, closing "prove" — verifiable by the existing `tt audit verify`.

**Local signed-append + key management (A-lite):** the public CLI today only *verifies* chains — there is no local append path and no local signing key. v1 adds both:
- **Shared primitive:** factor the entry-construction logic out of `InMemoryAuditWriter::write` into a pub `tt_telemetry::audit::build_entry(signing_key, prev: Option<&AuditEntry>, org, actor, event, payload) -> Result<AuditEntry, AuditError>` (genesis vs `prev.seq+1`/`prev.hash`, `compute_hash`, Ed25519 sign). `InMemoryAuditWriter::write` is refactored to call it (its existing tests prove no behavior change).
- **Signing key:** persisted at `~/.tokentrimmer/audit-signing-key` as 64 hex chars, file mode `0600`, **generated on first use** (`SigningKey::generate(OsRng)`). Local-operator trust model — the operator trusts their own machine's key.
- **Append:** read+parse the existing `.claude/AUDIT-CHAIN.jsonl` (reusing the CLI's `parse_chain_jsonl`), take the last entry as the chain tip, `build_entry` onto it, and append the serialized line. If the file doesn't exist, first write the `{"meta":true,"verifying_key":"<hex>"}` preamble so `tt audit verify` self-sources the key with no flags.
- **Output:** after apply, print the verifying (public) key hex so the operator can `tt audit verify` (or `tt audit verify --key-hex <pub>`).
- **Edge:** if `.claude/AUDIT-CHAIN.jsonl` already exists but was signed by a different key (e.g. a tt-api export), appending our entry would fail verification under that key — the writer warns and the operator can point at a different chain path. (v1 assumes the local chain is the operator's own.)

**Requirements / safety:**
- Requires `DATABASE_URL`; absent ⇒ a clear, actionable error (not a panic, not a silent no-op).
- Rejects a nil `PlanInput.org_id` at apply (regenerate with `--from-db`, or set a real org) — never writes routes to the nil org.
- If every proposed route is a no-op or already exists ⇒ report "nothing to apply", exit 0, write no routes and no audit row.
- `--apply` remains mutually exclusive with `--example` (unchanged); a new `--yes` flag skips the confirm prompt for automation. Without `--yes` on a non-interactive stdin, apply aborts with a message to pass `--yes`.

## Components (isolation)

| Unit | Location | Responsibility | Depends on |
|---|---|---|---|
| telemetry window reader | new `crates/cli/src/telemetry_window.rs` | `fetch_window(pool, org: Option<Uuid>, since, until) -> anyhow::Result<(Uuid, Vec<RequestLog>)>`: the `::float8`-cast windowed SELECT, column→struct map, and org auto-detect/ambiguity error | `sqlx`, `tt-plan-core` |
| suggest-plan `--from-db` wiring | `crates/cli/src/plan_suggest.rs` + clap in `main.rs` | gate the pull behind `--from-db`; materialize the window into the emitted `PlanInput` | telemetry_window |
| shared audit entry builder | `crates/telemetry/src/audit/writer.rs` | extract `build_entry(signing_key, prev, org, actor, event, payload) -> Result<AuditEntry, AuditError>`; refactor `InMemoryAuditWriter::write` to call it | existing audit primitives |
| local signed audit append + key | new `crates/cli/src/local_audit.rs` | resolve/generate the `~/.tokentrimmer/audit-signing-key` (0600); read chain tip from `.claude/AUDIT-CHAIN.jsonl`; `build_entry` + append; write preamble on a new file; return the verifying-key hex | `tt_telemetry::audit::{build_entry, AuditEntry, Actor}`, `ed25519_dalek` |
| local apply | new `crates/cli/src/plan_apply.rs` | `apply_routes(pool, org, routes, plan_id, projection, assume_yes)`: reject nil org → validate → dry-run/confirm/`--yes` → idempotent route write via `PostgresRoutingStore::{list_all_for_org, create_route}` → emit signed `plan.applied` via `local_audit` | `tt_routing::{PostgresRoutingStore, RoutingStore, validate_route_has_effect, NewRoute}`, `tt-plan-core`, local_audit |
| `tt plan --apply` handler | `crates/cli/src/main.rs` (`run_plan`, ~2283) + `--yes` clap flag on `Plan` | replace the stub with a call into `plan_apply` (build pool via `tt_core::connect`) | plan_apply |

## Error handling / edge cases

- **No `DATABASE_URL`** with `--from-db` or `--apply` → actionable error ("set `DATABASE_URL` to your gateway's Postgres").
- **Empty window** → emit a runnable PlanInput with `requests: []` + a note ("no telemetry in the selected window"); replay yields an honest "no savings."
- **Ambiguous org** (>1 distinct in window, no `--org`) → error listing the orgs found.
- **No effective routes at apply** (all no-ops / already present) → "nothing to apply", exit 0, no audit row.
- **NUMERIC decode** → pre-empted by `::float8` casts; covered by a regression test.
- **Read-only by default:** `--from-db` only reads; the only write is `--apply` after explicit confirmation.

## Determinism / attestation

The frozen `requests` in the file + the existing `seed` + `bootstrap_iterations` make `tt plan --input` deterministic — re-running produces an identical `PlanResult`. The `plan.applied` audit row binds the realize step to the chain, so `tt audit verify` proves what was applied. **No golden / RNG changes** (this spec touches neither the replay math nor the bootstrap), preserving the determinism gate and the savings-attestation reproducibility guarantee.

## Testing

- **Window reader** (live `pgvector:pg17` per MERGE_RUNBOOK, seeded `request_logs`): window filtering by `[since, until)`; org auto-detect for single/none/multiple; **`::float8` decode regression** (a real-valued window decodes without error); full column → `RequestLog` mapping (incl. `route_id → matched_route_id`, `task_class` default).
- **suggest-plan `--from-db`:** emitted `PlanInput` has non-empty `requests` + a concrete `org_id`/window; feeding it to `tt plan --input` produces a projection; without `--from-db`, output is the unchanged skeleton.
- **apply:** dry-run prints the route set without writing; `--yes` writes; a second `--apply` run is a no-op (idempotent by name); `plan.applied` audit row is emitted and verifies under `tt audit verify`; missing `DATABASE_URL` errors cleanly; all-no-op set ⇒ "nothing to apply", exit 0.
- **determinism:** frozen file + seed ⇒ identical `PlanResult` across runs (existing golden discipline; no golden changes).

> Note: the CLI `cli_spawn_smoke` tests time out in the local sandbox (can't spawn the binary) but pass in CI — DB-backed tests run against the live `pgvector` gate locally per the MERGE_RUNBOOK.

## Rollout

1. **v1 (this spec):** `telemetry_window` reader + `--suggest-plan --from-db` + real local `tt plan --apply` (direct DB), py/ts/js code-scan unchanged, plus updated `GETTING_STARTED.md` showing the closed loop.
2. **v2 candidates:** L2/quality enrichment join (embeddings + bodies for L2-semantic + Tier-3 quality projection); a hosted `tt_live_*` API pull/apply convenience endpoint (cloud, key-gated, no DB exposure); a persisted local plan history.
