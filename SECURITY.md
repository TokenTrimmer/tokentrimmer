# Security Policy

## Reporting a vulnerability

Email **security@tokentrimmer.com** with a description and reproduction. We aim
to acknowledge reports within 24 hours and triage within 72 hours. These are
operational targets, not a response or remediation SLA.

The marketing-site source builds the public reporting file at
<https://tokentrimmer.com/.well-known/security.txt>; verify a live deployment
before relying on that URL. Its policy link is <https://tokentrimmer.com/trust>.
No PGP key or encrypted-reporting channel is currently published. Do not send
live credentials, customer data, or other secrets in an email report.

Please do **not** open public GitHub issues for security vulnerabilities.

## Scope

The OSS Gateway, Inspect CLI, Plan engine, and SDKs in this repository are in scope. The hosted SaaS (`api.tokentrimmer.com`, `dashboard.tokentrimmer.com`) is in scope and is the primary concern.

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

We do not publish a remediation timeline or reporter-credit commitment in this
repository. Use the private reporting channel above rather than a public issue.

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
