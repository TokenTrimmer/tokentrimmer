# SDK honesty cleanup (batch 7b) — Design

**Status:** approved (gap-sweep follow-up, 2026-06-09)
**Date:** 2026-06-09
**Slice:** Audit-remediation, public repo. The last two in-sandbox sweep findings — both honesty/doc, no behavior change. Locally verifiable.

## Fix 1 — `tt-client` streaming silently drops un-deserializable tool-call frames (low)
`crates/client/src/lib.rs` `parse_sse_frame` does `if let Ok(calls) = serde_json::from_value::<Vec<ToolCall>>(...)` and falls through on the `Err`/empty path with no diagnostic. Against the TokenTrimmer gateway this is correct — the provider adapters reassemble fragmented upstream tool-call deltas into complete `ToolCall`s before the SSE wire, so the `Ok` path always hits. But a user pointing `base_url` at a *raw* OpenAI-compatible endpoint (a supported config) that streams partial deltas would lose streaming tool calls with zero explanation — a silent trap.
**Fix (proportionate, no new dep):** the SDK carries no logging dependency, and this is gateway-safe + low-severity, so make the behavior *documented* rather than silent: an explicit comment at the swallow site explaining the gateway-reassembly assumption, and a "Streaming tool calls" section on the public `stream()` method telling callers that partial frames from a raw endpoint are skipped — route streaming tool calls through the gateway or use non-streamed `send()`. (Reassembling partial deltas by `index` would be the full fix, but it's a behavior/API change disproportionate to a low, gateway-safe finding.)

## Fix 2 — `ts-types` placeholder doc overclaims a non-existent CI gate (gap/medium)
`crates/ts-types/src/lib.rs` doc said "Tests emit `.d.ts` into `bindings/` — CI's bindings-drift-check job fails if generated output differs" — but there are no `ts-rs` derives, no `bindings/`, and no such CI job; it's a one-test placeholder shipping in the workspace.
**Fix:** rewrite the module doc to state plainly it's an unimplemented placeholder (reserved workspace slot, ships nothing today, don't depend on it), mirroring the cloud `api-client` honesty fix. Kept the crate (removing a workspace member is broader churn; the honest-placeholder framing closes the "mistaken for shipped" risk).

## Verification (done)
- `cargo test -p tt-client -p tt-ts-types` — 38 + 1 pass. `cargo clippy --all-targets` clean. `cargo fmt --check` clean on both files. Doc/comment-only — no behavior change.

## Out of scope
- Reassembling partial tool-call deltas in the SDK (own slice if raw-endpoint streaming tool calls become a supported requirement).
- Implementing the actual `ts-rs` codegen + bindings-drift CI (the real `ts-types` work).
