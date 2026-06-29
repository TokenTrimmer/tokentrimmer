# W3b IP-literal SSRF guard fix — run-time re-assertion in `run_http`

## What was fixed

`run_http` in `crates/core/src/workflow/http.rs` now calls
`tt_shared::validate_provider_url(&spec.url, false)` immediately after the
existing allowlist check (step 1b). This re-asserts the full SSRF guard at
run time:

- **https-only scheme** — http URLs are rejected even if the host is in `allowed_hosts`.
- **IP-literal private/loopback/link-local block** — `127.0.0.1`, `169.254.169.254`,
  RFC-1918 ranges, CGNAT, IPv6 loopback/ULA/link-local are all blocked.
- **Hostname denylist** — `localhost`, `*.local`, `metadata.google.internal`.
- **Best-effort DNS-resolved-IP block** — defence-in-depth against rebind via hostname.

The root gap: reqwest/hyper connect **directly** to IP-literal hosts, bypassing the
custom DNS resolver (`with_guarded_dns`). So `http://127.0.0.1/` and
`http://169.254.169.254/…` slip past `GuardedResolver` entirely. The save-time
`validate_provider_url` call covers the normal save→store→run path, but `run_http`
already re-checks userinfo (defence-in-depth), so the IP/scheme guard must be
re-checked by the same logic.

A new `HttpError::BlockedUrl` variant was added (static, non-leaking message:
`"url rejected by SSRF guard (blocked scheme or private/internal address)"`).
The error maps all `UrlGuardError` variants to `BlockedUrl` — no URL, no secret.

## Three new tests (all in `workflow::http::tests`)

| Test | URL | `allowed_hosts` | Expected error |
|---|---|---|---|
| `run_http_rejects_ip_literal_private` | `http://127.0.0.1/` | `["127.0.0.1"]` | `BlockedUrl` |
| `run_http_rejects_metadata_ip` | `http://169.254.169.254/latest/meta-data/` | `["169.254.169.254"]` | `BlockedUrl` |
| `run_http_rejects_non_https_at_run` | `http://allowed.example.com/` | `["allowed.example.com"]` | `BlockedUrl` |

All three **failed before the fix** (they reached the network / got a connection
error instead of a pre-flight rejection), and **pass after the fix**.

## Verification

```
cargo test -p tt-core --lib workflow
test result: ok. 114 passed; 0 failed; 0 ignored; 0 measured
```

```
cargo fmt --check -p tt-core → fmt OK
cargo clippy -p tt-core --lib --tests → exit 0 (no errors or new warnings)
```

## Self-review

- IP-literal private (`127.0.0.1`) blocked at run ✓
- IP-literal metadata (`169.254.169.254`) blocked at run ✓
- Non-https scheme blocked at run ✓
- Error message contains no URL, no secret ✓ (`BlockedUrl` is a unit variant)
- Existing allowlist + userinfo + timeout + byte-cap + redirect-none behavior unchanged ✓
- Consistent with save-time `validate_provider_url` call ✓
- `rand`/`rand_chacha` not touched ✓
