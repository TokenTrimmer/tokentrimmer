# Security Policy

## Reporting a vulnerability

Email **security@tokentrimmer.com** with details. We acknowledge within one business day.

For sensitive reports, encrypt with our PGP key (published at `https://tokentrimmer.com/.well-known/security.txt` once production launches).

Please do **not** open public GitHub issues for security vulnerabilities.

## Scope

The OSS Gateway, Inspect CLI, Plan engine, and SDKs in this repository are in scope. The hosted SaaS (`api.tokentrimmer.com`, `app.tokentrimmer.com`) is in scope and is the primary concern.

## What we consider in-scope

- Authentication and session handling
- API key handling (storage, rotation, revocation)
- Provider credential encryption at rest
- Cross-tenant data exposure
- Audit log integrity (hash-chain tampering, signature bypass)
- Injection vulnerabilities (SQL, command, prompt)
- Cryptographic weaknesses

## Out of scope

- Findings produced by automated scanners without a working PoC
- Self-XSS or attacks requiring physical access
- Denial-of-service against the OSS components (rate limiting is the customer's responsibility for self-hosted)
- Issues in third-party dependencies without a TokenTrimmer-specific exploit path

## Coordinated disclosure

We aim to release a fix within 30 days of confirmation. We will credit reporters in the changelog unless anonymity is requested.

## Our security commitments

- All commits to `main` are signed.
- All releases are signed.
- Dependencies are vetted via `cargo deny` and `cargo audit` in CI.
- Provider credentials are encrypted at rest with XChaCha20-Poly1305 using per-org data encryption keys.
- API keys are stored as argon2 hashes; only the prefix is recoverable.
- Audit log entries are hash-chained (BLAKE3) and signed (Ed25519).
- All hosted-service infrastructure runs behind Cloudflare with DDoS protection.

## Bug bounty

Not yet active. Will launch with v1 GA. Stay tuned.
