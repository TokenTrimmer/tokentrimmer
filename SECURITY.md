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

## Source-evidenced controls and proof boundaries

This repository can show implementation and CI policy. It cannot by itself
prove a deployed environment, GitHub configuration/history, a release-signing
ceremony, a Cloudflare configuration, or an operational security outcome.

- CI source contains dependency-policy checks including `cargo deny`; current
  advisory-feed results are an operational CI record, not a static-repository
  guarantee.
- Provider-credential code uses XChaCha20-Poly1305 with per-org derived keys.
  Deployment key custody, backups, and rotation execution require operator
  evidence.
- API-key code stores Argon2 hashes and only retains a prefix for lookup.
- Audit-entry code uses a BLAKE3 hash chain and Ed25519 signatures. Signature
  integrity is not a claim of independent issuer trust or economic truth.
- Branch-protection, commit-signature enforcement, signed release artifacts,
  CDN/DDoS configuration, TLS/HSTS headers, monitoring, and incident response
  must be independently verified for a live deployment; they are not asserted
  as completed facts here.

No current external penetration-test report, compliance certification,
recovery exercise, RTO/RPO result, or access-review record is published in
this repository. See the Trust Center for the current public evidence status.

## Bug bounty

Not yet active. Will launch with v1 GA. Stay tuned.
