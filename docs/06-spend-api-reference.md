# Spend API reference (`/v1/spend`)

The tenant-facing spend surface — authed by the `tt_live_` key (never
caller-supplied), so the org is always derived from the authenticated key.
Backs the MCP cost-control tools (`get_spend_today`, `check_budget_remaining`,
`set_cost_limit`). Source: `crates/core/src/routes/spend_api.rs`.

Auth: requires a real `tt_live_` key. Anonymous / dogfood / sandbox keys →
`401`. A `tt_live_` key always resolves to its org; `key_id` parameters must
belong to the caller's org (else `404` — same opaque outcome as "not found").

## `GET /v1/spend` — spend-today + MTD + budget-remaining

The caller-org's spend summary. `503` until a `SpendSource` is wired
(`AppState::with_spend_source`); an org with no in-window traffic answers an
honest all-zero body, NOT `404`.

**Response (`SpendSummary`):** spend-today, MTD, + budget-remaining (the
`monthly_cap_usd` minus MTD spend). Assembled from the raw in-window spend via
`SpendSummary::assemble(org, raw)`.

## `POST /v1/spend/limit` — set or clear the monthly spend cap

Set (or clear) the caller-org's monthly spend cap, OR a per-key cap
(key-ownership-gated). The gateway already READS these caps for enforcement
(`crate::tier_resolver`); this is the matching tenant-self-serve write on the
same auth seam.

**Request body (`SetSpendLimitRequest`):**

| Field | Type | Meaning |
|---|---|---|
| `monthly_cap_usd` | `Option<f64>` | Monthly USD cap; `null` CLEARS it. Must be finite + ≥ 0. |
| `key_id` | `Option<Uuid>` | When set, scope the cap to this key (must be org-owned); else org-wide. |

**Response (`SetSpendLimitResponse`):**

| Field | Type | Meaning |
|---|---|---|
| `org_id` | `Uuid` | The caller's org. |
| `key_id` | `Option<Uuid>` | The key the cap was scoped to (None = org-wide). |
| `monthly_cap_usd` | `Option<f64>` | The new cap (None = cleared). |
| `applied` | `bool` | Always `true` on a 2xx (the change was persisted). Mirrors the MCP `CostLimitSet.applied`. |

**Errors:** `503` until a `SpendSource` is wired; `400` on a
negative/non-finite cap; `404` when `key_id` isn't the caller-org's.

## Example

```bash
# Set a $50 monthly org-wide cap
curl -X POST https://api.tokentrimmer.com/v1/spend/limit \
  -H "Authorization: Bearer $TT_LIVE_KEY" \
  -H "content-type: application/json" \
  -d '{"monthly_cap_usd": 50.0}'

# Clear the cap
curl -X POST https://api.tokentrimmer.com/v1/spend/limit \
  -H "Authorization: Bearer $TT_LIVE_KEY" \
  -H "content-type: application/json" \
  -d '{"monthly_cap_usd": null}'

# Set a $10 cap scoped to one key
curl -X POST https://api.tokentrimmer.com/v1/spend/limit \
  -H "Authorization: Bearer $TT_LIVE_KEY" \
  -H "content-type: application/json" \
  -d '{"monthly_cap_usd": 10.0, "key_id": "00000000-0000-0000-0000-000000000001"}'

# Read spend
curl https://api.tokentrimmer.com/v1/spend -H "Authorization: Bearer $TT_LIVE_KEY"
```

## Related

- `crates/core/src/routes/spend_api.rs` — the source of truth.
- `crates/core/src/spend.rs` — `SpendSource` + `SpendSummary::assemble`.
- `crates/core/src/tier_resolver.rs` — the enforcement side (the gateway READS these caps on dispatch).
- The MCP `get_spend_today` / `check_budget_remaining` / `set_cost_limit` tools — `tt mcp install` exposes these to your agent client.
- `docs/coding-agents.md` — the coding-agent wedge (where the runtime `$` cap lives).
