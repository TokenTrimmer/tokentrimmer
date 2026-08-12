# TokenTrimmer for coding agents — cap your Claude Code / Cursor spend, with receipts

Coding-agent token burn is arguably the #1 felt LLM cost pain of 2026: a tight
feedback loop, long-running sessions, and tool calls that balloon context until
a single `claude code` run costs more than a human dev's daily wage. Every
saving the agents claim is a black-box number you can't challenge.

TokenTrimmer ships the full substrate to fix this — natively, no SDK change:

1. **The gateway natively serves Anthropic's `/v1/messages`.** Claude Code,
   Cursor, and every Anthropic-SDK agent can point `ANTHROPIC_BASE_URL` at the
   gateway and keep working exactly as before.
2. **Durable `$` budget admission + a kill-switch apply at the gateway**, not
   in your code. `monthly_cap_usd` atomically reserves an estimated upper bound
   before every provider attempt, per org and per key; the `CircuitBreaker`
   remains the route kill-switch.
3. **Eligible compression events can be signed.** A Verifiable Compression
   Receipt (VCR): `POST /v1/admin/requests/{trace_id}/compression-receipt/sign`
   mints an Ed25519-signed record of a calculated `{savings, route, model,
   trace_id, ts}` estimate. `tt verify-receipt --receipt <json> --key-hex <hex>`
   checks that payload offline with a key you pinned. Signature tooling is not
   exclusive to any vendor; the useful question is what evidence, coverage, and
   reconciliation a product exposes.

This page wires the shipped pieces into one 5-minute path.

## What's shipped (verified, not aspirational)

- **`POST /v1/messages`** — Anthropic-native Messages API ingress
  (`crates/core/src/routes/messages.rs`). Translates the Anthropic Messages
  shape to the canonical chat request, dispatches through the same chat
  pipeline (cost accounting, routing, caching, credential resolution, the
  BYO-only guard all apply identically), and translates the response back —
  non-streaming `{type:"message",...}` JSON or Anthropic typed SSE frames
  (`message_start`, `content_block_*`, `message_delta`, `message_stop`). The
  `x-tokentrimmer-*` cost headers are preserved verbatim.
- **Durable runtime `$` admission + kill-switch** —
  `crates/core/src/budget_reservation.rs` (Postgres reservation, settlement,
  retry identity, and provenance) + `crates/core/src/state.rs`
  (`CircuitBreaker`). Set per org / per API key.
- **Signed receipts** — `tt_telemetry::vcr` (the `vcr:v1|` Ed25519 primitive) +
  the cloud mint endpoint + `tt verify-receipt` (the offline verify CLI).
- **`tt init`** — installs the TokenTrimmer best-practices harness into a repo
  (the `.claude/` hooks + the `.tokentrimmer/budgets.toml` PR-cost-gate below).
- **`tt mcp install --client claude-code`** — autowires the MCP client config
  so Claude Code sees TokenTrimmer's tools.
- **`tt login`** — browser-assisted key paste (opens the dashboard keys page,
  reads the pasted key; no manual copy). `tt connect` generates SDK-specific
  connect snippets instead.

## Two complementary cost gates (don't confuse them)

TokenTrimmer has **two** cost-control surfaces, at different points in the dev
loop. Both are shipped; the coding-agent wedge uses both:

### 1. Runtime `$` admission + kill-switch (the runaway-agent guard)
Set per org / per key on the gateway. For capped tenants, every provider
attempt atomically reserves catalog-estimated headroom in Postgres before
dispatch. Concurrent gateway replicas share the same monthly totals; retries
with the same `Idempotency-Key` cannot admit the same provider attempt twice.
Provider-reported usage settles the reservation, while missing usage settles
conservatively with explicit provenance. Unknown pricing fails closed. The
agent receives a clean 402 when no headroom remains.

This is a durable admission bound, not provider-invoice reconciliation or a
promise that final provider usage cannot exceed a catalog estimate. The
`CircuitBreaker` is the separate route-level quality kill-switch.

### 2. PR cost-gate (the `.tokentrimmer/budgets.toml` — the endemic-inflation guard)
A declarative, per-glob ceiling on the **per-call cost of model references that
a PR adds or changes** — an offline `git diff` cost-gate, run by `tt inspect`
or in CI. It catches expensive-model creep in hot paths (a `gpt-5.5` call
added under `src/routes/**`) and the whole-PR net projected per-call delta.

```toml
# .tokentrimmer/budgets.toml
[global]
max_pr_delta_usd = 0.05        # the whole-PR net per-call delta ceiling

[globs."src/routes/**"]
max_call_usd = 0.02            # no ADDED model call in a routes file > $0.02/call

[globs."src/experimental/**"]
max_call_usd = 0.20            # experimental paths get a higher tolerance
```

`tt init` drops this + the `.claude/` hooks. It is **not** a runtime spend cap
(no request volume is knowable from a diff) — it's the PR-time guard against
endemic cost inflation as the codebase evolves.

## The 5-minute path (Claude Code)

```bash
# 1. Get a gateway key (browser-assisted, no manual copy)
tt login

# 2. Point Claude Code at the gateway — that's it.
#    NB: the BASE URL has NO /v1 — Claude Code appends /v1/messages itself,
#    so /v1 here would double to /v1/v1/messages. Use ANTHROPIC_AUTH_TOKEN
#    (sent as Authorization: Bearer); ANTHROPIC_API_KEY is sent as x-api-key,
#    which the gateway also accepts as an auth alias.
export ANTHROPIC_BASE_URL=https://api.tokentrimmer.com
export ANTHROPIC_AUTH_TOKEN=<your tt_live_ key>

# 3. (optional) Wire the MCP tools into Claude Code's config
tt mcp install --client claude-code

# 4. (optional, repo-level) Drop the PR cost-gate + .claude/ hooks into a repo
cd your-repo && tt init

# 5. Run Claude Code exactly as before — the gateway applies configured guards;
#    eligible compression receipts can be minted on demand.
claude
```

Now every request Claude Code makes flows through the gateway: the org's
`monthly_cap_usd` + `CircuitBreaker` apply, content-aware compression trims the
prompt (the isolated `content_compress_saved_est_usd` saving never enters the
catalog-priced `Saved-Usd` headline), and an eligible compression can be
minted on demand as a VCR signed estimate.

## Reviewing a savings estimate

For an eligible compression, request a signed receipt estimate:

```bash
# Mint (cloud admin endpoint, your org asserted):
curl -X POST https://api.tokentrimmer.com/v1/admin/requests/<trace_id>/compression-receipt/sign \
  -H "Authorization: Bearer $TT_ADMIN_KEY" -d '{"org_id":"<your-org-uuid>"}' > receipt.json

# Verify offline — the customer's stronger trust model (pin the key, not the receipt):
tt verify-receipt --receipt receipt.json --key-hex <the-verifying-key-hex>
# → PASS: signature verifies against the supplied verifying key
```

The receipt is self-contained: the embedded `verifying_key_hex` +
`signature` let `tt verify-receipt` run a mathematical signature check with no
network or DB. Supply a key out of band (the stronger posture) to establish the
issuer. That check confirms the signed payload has not changed under that key;
it does not independently establish the savings math or reconcile a provider
invoice.

## What to verify end-to-end before publishing the marketing page

Before the marketing page goes live, run the full loop once on staging:

1. `tt login` → key works.
2. `ANTHROPIC_BASE_URL=<staging>` (no `/v1` — Claude Code appends `/v1/messages`)
   + Claude Code → a non-streaming request + a streaming request both round-trip
   (the SSE event frames are Anthropic-shaped).
3. A tool-use round-trip (Claude Code's `read_file` / `edit_file` tools) — the
   tool-call blocks translate cleanly both directions.
4. Set a low `monthly_cap_usd` on a test key → run past it → confirm the 402 +
   the clean agent experience (the agent sees a billing error, not a hang).
5. Mint a receipt for a compression + `tt verify-receipt` → PASS.

If all five pass, the marketing page ("Cap your Claude Code / Cursor spend —
with receipts") is honest.

## Related

- `docs/04-gateway-api-reference.md` — the gateway API (incl. `/v1/messages`).
- `docs/tt-cli-commands.md` — `tt init`, `tt mcp install`, `tt connect`,
  `tt verify-receipt`.
- `docs/tt-init-usage.md` + `docs/tt-mcp-usage.md` — the usage detail.
- The VCR primitive: `crates/telemetry/src/vcr.rs` (`vcr:v1|` canonical payload).
