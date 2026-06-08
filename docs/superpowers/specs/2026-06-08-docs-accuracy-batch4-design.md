# Docs-accuracy sweep (batch 4) — Design

**Status:** approved (design + decisions, 2026-06-08)
**Date:** 2026-06-08
**Slice:** Audit-remediation, public repo, `docs/` only. Closes seven docs-accuracy findings by making the docs match code reality. All re-verified STILL-PRESENT against current code+docs with exact line numbers before scoping. **No code changes** — docs only.

## Decisions (user-approved)
- **Unimplemented-but-documented features** (§7.3 metadata, §9 webhooks, §15 version pinning, admin-API PATCH/plans/inspect/usage): **mark "Planned (not yet honored)"**, mirroring the existing precedent for `X-TokenTrimmer-Trace-Parent` (api-ref line 412). Don't delete.
- **§10 admin/config API:** **split hosted vs self-hosted** — label `/v1/admin/*` as "Hosted (cloud) only" and document the actual self-hosted `/v1/routes` surface (GET/POST/DELETE only, no PATCH). Fix the arch-spec §17 claim that self-hosted serves no route API.

## Edit map (verified line numbers)

### 1. §7.3 upstream-error `error.tokentrimmer` metadata — `docs/04-gateway-api-reference.md:502-520`
Doc shows an `error.tokentrimmer` object (`provider`, `upstream_status`, `fallback_attempted`, `trace_id`). Real `ErrorBody` (`crates/core/src/error.rs:63-76`) serializes only `message`/`type`/`code`/`param`.
**Fix:** Replace the example with the actual flat envelope; add a "Planned (not yet honored)" note for the `tokentrimmer` enrichment object; note that `trace_id` is currently available via the `X-TokenTrimmer-Trace-Id` response header.

### 2. §10 admin API + arch-spec §17 — `docs/04-gateway-api-reference.md:587-627`, `docs/tokentrimmer-architecture-spec-v1.md:812`
Doc claims `/v1/admin/routes` GET/POST, `PATCH /v1/admin/routes/:id`, DELETE, plus `/v1/admin/plans|inspect/runs|usage|invoices`. Public gateway (`server.rs:50-56`) serves `/v1/routes` + `/v1/routes/:id` with GET/POST/DELETE only (`routes_api.rs:31-91` — no PATCH).
**Fix:** Label the `/v1/admin/*` block "Hosted (cloud) only". Add a "Self-hosted gateway routes API" subsection documenting `/v1/routes` (GET list, POST create) and `/v1/routes/:id` (GET, DELETE); mark PATCH/update "Planned". Correct arch-spec §17's "No `/v1/admin/*` API surface … YAML only" to note the self-hosted `/v1/routes` API exists (no `/admin/` prefix; GET/POST/DELETE).

### 3. §9 webhooks + §15 version pinning — `docs/04-gateway-api-reference.md:538-583`, `766-777`
Neither implemented (no webhook signing/delivery; no `x-tokentrimmer-version` reader).
**Fix:** Add a "Planned (not yet honored)" banner to §9 and §15, mirroring line 412's wording.

### 4. Pricing "daily-refreshed" — `docs/04-gateway-api-reference.md:957`, `docs/02-provider-adapter-guide.md:65`, `:649`
Code (`crates/shared/src/pricing.rs:1-14`) says manually-curated snapshot, embedded at build time, NOT auto-refreshed.
**Fix:** Replace "daily-refreshed"/"refreshed daily" with "manually curated and embedded at build time, refreshed on release cadence; reconciled against invoices" (keep the existing §19 reconciliation note).

### 5. Provider trait listing stale — `docs/02-provider-adapter-guide.md:35-77`
Guide omits `fee_multiplier`, `dropped_params`, `supports_response_schema`, `temperature_range` (all in `crates/shared/src/provider.rs:31-62`), and wrongly says `health_check` "hits the provider's models endpoint" (real default is a no-op `Ok(())`, provider.rs:91-93).
**Fix:** Add the four default-method hooks with their default values + one-line guidance (tie each to the behavior the same guide documents: param_dropped warnings, response_format_downgrade, temperature clamping, BYOK fee). Correct the `health_check` description. Mention the four hooks in the §6 "adding a provider" checklist.

### 6. `is_retriable` example doesn't compile — `docs/02-provider-adapter-guide.md:199-210`
The `matches!(... | ProviderError::ProviderUpstream { status, .. } if *status >= 500)` form doesn't compile (guard var unbound across alternatives).
**Fix:** Replace with the real `match`-based impl from `crates/shared/src/error.rs:41-49`.

### 7. base_url SSRF guard undocumented — add to `docs/04-gateway-api-reference.md` §2.2 + `docs/02-provider-adapter-guide.md`
`crates/shared/src/url_guard.rs` implements `validate_provider_url` (blocks loopback/private/link-local/ULA/CGNAT/cloud-metadata; https-only unless `allow_local`; best-effort DNS check) and `filter_extra_headers` (denies `authorization`/`x-api-key`/`host`/`content-type`/`anthropic-version` + hop-by-hop). Undocumented.
**Fix:** Add a short "Security: custom provider URLs & headers" subsection summarizing: blocked ranges, denied headers, the DNS-rebind/TOCTOU limitation, and the recommended network-policy mitigation for operators.

## Out of scope
- Implementing any of the Planned features (webhooks, version pinning, the metadata object, PATCH route update) — each is its own slice.
- Cloud-repo docs (the `/v1/admin/*` surface's own hosted docs).
- The `pub-scripts-corpora` findings (batch 5).

## Testing / verification
Docs-only; no unit tests. Verification = (a) every claim cross-checked against the cited code file:line (done in the edit map), (b) the corrected `is_retriable` example compiles (it's copied verbatim from shipping code), (c) a final read-through that no "daily"/`/v1/admin/routes`/un-flagged-webhook claim remains via grep.
