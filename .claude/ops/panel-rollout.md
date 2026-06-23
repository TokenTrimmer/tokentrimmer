# Panel Rollout Runbook — Deep Research Panel Kill-Switch + Entitlement

**Status:** ops-runbook (human-gated)
**Feature:** Deep Research Panel (Phases 1–6, `feat/panel-phase7-entitlement`)
**Infra write policy:** Flipping environment variables on the production gateway is a **user-gated infra action** per the project's [infra-writes-user-gated](../../../DECISIONS.md) convention. This runbook documents the steps; it does **not** perform them. A human operator must apply these changes via the Fly.io console, the `fly secrets set` CLI, or the equivalent deployment mechanism.

---

## Purpose

The Deep Research Panel is off by default (`TT_PANEL_ENABLED` absent/unset). This runbook describes how to:

1. Enable the panel for all callers (alpha/beta rollout)
2. Optionally gate access to a minimum tier
3. Monitor for cost and correctness regressions
4. Rollback immediately if needed

---

## Pre-checks

Before enabling, verify the following:

- [ ] **Catalog coverage:** at least two member models and one arbiter model are in the gateway's model catalog (`GET /v1/models`) and have stored org credentials for the target org(s). Members with no stored credential are silently skipped (`skipped_no_cred`) — a panel request with all members skipped will fail quorum.
- [ ] **Default members + arbiter configured** (if you want callers to omit `tt_extras.panel`): confirm `TT_PANEL_DEFAULT_MEMBERS` and `TT_PANEL_DEFAULT_ARBITER` are set in the gateway's process environment.
- [ ] **Database reachable:** `panel_legs` rows are written async; confirm `DATABASE_URL` is set and the schema migration for `panel_legs` has been applied.
- [ ] **Budget ceiling policy:** decide whether to require callers to pass `X-TokenTrimmer-Cost-Limit-Usd` per-request (the default, fail-closed) or whether to set a route-level `max_cost_usd` for panel routes. A panel request with no ceiling is always rejected `402` — this is by design.
- [ ] **Metrics baseline:** note current `http_requests_total` and `provider_request_duration_seconds` baselines before enabling, so cost increases from panel traffic are distinguishable.

---

## Enable steps

### Step 1: Enable the kill-switch

Set the environment variable on the gateway process:

```
TT_PANEL_ENABLED=1
```

Truthy values: `1` or `true` (case-insensitive). All other values (including absent) leave the panel disabled. Panel requests on a disabled gateway receive:

```json
{ "error": { "message": "The deep-research panel is not enabled on this gateway.", "type": "permission_error", "code": "panel_disabled" } }
```
HTTP status `403`.

### Step 2: Choose the minimum tier (optional)

By default (`TT_PANEL_MIN_TIER` absent or `Free`) all callers can use the panel. To restrict to a minimum tier, set:

```
TT_PANEL_MIN_TIER=Pro    # or Team or Scale
```

Accepted values (case-insensitive): `Free`, `Pro`, `Team`, `Scale`. Unknown values log a warning and fall back to `Free` (allow-all). Below-tier callers receive `403`.

**Tier ordering:** Free < Pro < Team < Scale.

To gate only paid callers:
```
TT_PANEL_MIN_TIER=Pro
```

### Step 3: Confirm member cap

```
TT_PANEL_MAX_MEMBERS=8    # default — 8 members max (arbiter not counted)
```

Adjust downward for cost-control during limited rollout (e.g. `TT_PANEL_MAX_MEMBERS=3`). Requests specifying more members than the cap receive `400`.

### Step 4: Configure defaults (if desired)

```
TT_PANEL_DEFAULT_MEMBERS=gpt-4o,claude-3-5-sonnet,google/gemini-1.5-pro
TT_PANEL_DEFAULT_ARBITER=gpt-4o
```

These are used when a caller sends `X-TokenTrimmer-Panel: synthesize` without a `tt_extras.panel` body config. If absent, callers must specify members + arbiter in `tt_extras.panel` or the panel resolves an empty member list and returns `400`.

---

## What to watch

### OTel span attributes (per-request, on `http_request` spans)

| Attribute | What to watch |
|---|---|
| `tokentrimmer.panel_strategy` | Strategy distribution (synthesize/best-of-n/majority) |
| `tokentrimmer.panel_leg_count` | Average leg count (cost signal) |
| `tokentrimmer.panel_quorum_required` / `panel_quorum_met` | Quorum failures indicate credential gaps |

### Prometheus metrics

| Metric | What to watch |
|---|---|
| `panel_requests_total{outcome="success"}` | Successful panel completions |
| `panel_requests_total{outcome="quorum_unmet"}` | Quorum failures — check member credential coverage |
| `panel_requests_total{outcome="error"}` | Unexpected errors |
| `panel_legs_total{role="leg",status="skipped_no_cred"}` | Members skipped — check stored credentials |
| `panel_legs_total{role="leg",status="error"}` | Upstream leg failures |

### Cost monitoring

- **`X-TokenTrimmer-Cost-Usd` on panel responses** is the aggregate (Σ members + arbiter). A 3-member + 1-arbiter panel at gpt-4o rates with a medium-sized prompt costs roughly 4–10× a single equivalent request.
- **`request_logs` table** (`provider='panel'`): `cost_usd` is the aggregate; join to `panel_legs` for per-leg breakdown.
- **`tokentrimmer.usage` SSE event** on streaming responses carries `cost_usd` = the panel aggregate. Streaming clients should read this frame to surface real cost.
- Watch `http_requests_total` cost rate for a step-up on first rollout; alert if daily panel cost exceeds budget thresholds.

### Quality signal

`best-of-n` and `majority` strategies report arbitration detail in `tokentrimmer.panel.arbiter`:
- `fell_back: true` on `best-of-n` means the judge returned an unparseable response.
- `degraded: true` on `majority` means embedding failed; the first surviving leg was returned.
- High `no_majority: true` rate on `majority` means all answers were distinct (no consensus cluster).

---

## Rollback

To disable the panel immediately:

```
TT_PANEL_ENABLED=0    # or unset the variable entirely
```

**Effect:** takes effect on the next request after the deployment/secret-refresh propagates. In-flight panel requests that have already begun leg dispatch are not affected (they complete normally). After the kill-switch is applied:
- All new panel requests receive `403 panel disabled` before any dispatch.
- No new panel billing rows are written.
- No upstream calls are made for panel purposes.
- Single-model requests are **completely unaffected** (the panel code path is only entered on `X-TokenTrimmer-Panel` header presence).

There is no in-flight corruption risk: each panel request is atomic — it either completes and writes one `request_logs` row + `panel_legs` rows, or it fails before any billing side effects. Rollback cannot interleave with a partially-committed panel.

---

## Quick reference

```
# Enable (allow all tiers, default cap of 8 members)
TT_PANEL_ENABLED=1

# Enable with Pro-tier gate and 3-member cap
TT_PANEL_ENABLED=1
TT_PANEL_MIN_TIER=Pro
TT_PANEL_MAX_MEMBERS=3

# Disable immediately (rollback)
TT_PANEL_ENABLED=0
```

---

*This runbook covers the server-side panel feature gate only. Client-side changes (SDK updates, dashboard toggles) are out of scope here.*
