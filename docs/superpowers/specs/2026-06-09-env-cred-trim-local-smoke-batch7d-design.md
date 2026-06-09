# Env-cred trim + local-adapter smoke test (batch 7d) — Design

**Status:** approved (working through remaining audit lows, 2026-06-09)
**Date:** 2026-06-09
**Slice:** Audit-remediation, public repo, `crates/{auth,providers/local}`. Two clean, fully-testable lows. No behavior change to existing happy paths (additive trim + new test file).

## Fix 1 — `EnvProviderCredentialStore` trims env-sourced keys (opportunity/low)
`get()` returned `std::env::var(...)` verbatim. A key with a trailing newline (common from `echo secret | fly secrets set`) was forwarded to the upstream provider unchanged, producing a confusing 401 from OpenAI/Anthropic that is hard to diagnose. The `from_env` master-key path already trims (`postgres.rs:116`); the env credential path did not.
**Fix:** add a private `normalize_key(raw: &str) -> Option<String>` helper that `trim()`s and treats a now-empty value as absent (`None`); route `get()`'s `Ok(v)` arm through it. A whitespace-only var is therefore treated as unset rather than a blank key. Left the per-call env read in place — the store is single-tenant dogfooding, the doc already notes updates are rare, and a hot-path cache is unwarranted complexity for the finding. Test `env_store_trims_surrounding_whitespace_and_newlines` covers the trailing-newline-trimmed case and the whitespace-only→`None` case.

## Fix 2 — HTTP-level smoke test for the local adapter (dx/low)
Every hosted group-B adapter (groq, mistral, together, openrouter) has a `tests/smoke.rs` exercising the real HTTP path against httpmock; `LocalProvider` only had in-crate unit tests for id/url/prefix-strip, so the security-sensitive `allow_local` behavior was untested at the wire layer.
**Fix:** add `crates/providers/local/tests/smoke.rs` mirroring the hosted adapters. Tests: (1) `strips_backend_prefix_and_reaches_loopback` — the mock only matches the bare `llama3.1:8b` body, so a pass proves the `ollama/` prefix was stripped before dispatch AND that `allow_local` let the adapter reach a loopback `base_url` (the SSRF guard is bypassed for local backends); (2) `bare_model_is_forwarded_unchanged`; (3) `pricing_is_zero_for_any_local_model`.

## Verification (done)
- `cargo test -p tt-auth -p tt-provider-local` — all pass (incl. the new unit test + 3 smoke tests). `cargo clippy -p tt-auth -p tt-provider-local --all-targets` clean. `cargo fmt … --check` clean.
- `normalize_key` is additive; `get()`'s signature/return type unchanged. New test file has no production-code dependents.
