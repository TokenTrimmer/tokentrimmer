# Secret handling, dev/prod split & rotation runbook

> Status: this runbook is the **non-operator** half of `env-secret-split-rotate`.
> The actual rotation of live keys and the migration of production secrets into
> `fly secrets` are operator actions — see "Operator checklist" at the bottom.

## The problem this fixes

`./.env.development` and `./.env.production` are currently **byte-identical** and
both hold **live production** secrets (real provider keys, a 64-char
`TT_MASTER_KEY`, the audit signing key, the Fly deploy token, Stripe keys).
They are correctly gitignored and have never been committed — but production
secrets should not sit on a developer workstation under a `*.development` name,
and dev/prod should not share key material.

The fix is two-part:

1. **Split** the secret sets so development uses throwaway/test credentials and
   production secrets live only in the deploy platform's secret store.
2. **Rotate** the high-value keys that have been read into a workstation env.

## Principle: where each secret should live

| Environment | Where secrets live | Contents |
|---|---|---|
| Local dev | `./.env.development` (gitignored) | **test/sandbox** keys only — `sk-test-…`, `tt_test_*`, a *dev-only* `TT_MASTER_KEY`, a local Postgres/Redis URL. Never a live provider key. |
| CI | CI secret store (GitHub Actions secrets) | test keys + any keys needed for integration jobs |
| Production | `fly secrets` (gateway + `tt-api`) — never on disk | live provider keys, prod `TT_MASTER_KEY`, `TT_AUDIT_SIGNING_KEY`, `TT_ADMIN_TOKEN`, `STRIPE_*`, `FLY_API_TOKEN` |

`.env.production` should not exist on the workstation. Production config is set
with `fly secrets set KEY=value` and read from the Fly runtime environment.

## High-value keys (rotate these)

| Key | Blast radius if leaked | Rotation notes |
|---|---|---|
| `TT_MASTER_KEY` | Root for stored provider credentials, captured request/response bodies, encrypted L1 cache values, and short-lived account-purge capabilities in this repository; hosted deployments derive additional keys from the same root. | Never swap in place. Use a coordinated maintenance procedure that inventories every enabled key family; the public primitives alone are not a complete rotator. |
| `TT_AUDIT_SIGNING_KEY` (Ed25519) | Forge audit-chain signatures. | Rotate the keypair; record the rotation as an audit event; publish the new verifying key. Old entries stay verifiable under the old public key. |
| `TT_ADMIN_TOKEN` (cloud) | Full admin API access (`/v1/admin/*`). | Rotate immediately; single bearer value, no migration needed. |
| `FLY_API_TOKEN` | Deploy/destroy infrastructure. | `fly tokens revoke` + issue a new scoped deploy token. |
| `STRIPE_SECRET_KEY` / webhook signing secret | Billing actions / forged webhooks. | Roll in the Stripe dashboard; update `fly secrets`; re-point the webhook endpoint secret. |
| Provider keys (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, …) | Spend on your provider bill. | Roll at each provider; update `fly secrets`. These are normally **per-tenant** in `provider_credentials`; a top-level key only exists for dogfooding and is only ever served when `TT_ALLOW_ENV_CREDENTIAL_FALLBACK=1` is set explicitly (the gateway is BYO-only by default). |

## `TT_MASTER_KEY` rotation (re-encryption)

`TT_MASTER_KEY` is not safely rotatable by changing one environment value. In
this repository, it protects or derives keys for:

1. **`provider_credentials`** — per-(org, provider) derived keys + AAD.
2. **`request_body_captures`** — the opt-in encrypted request/response bodies for
   hosted `/logs` replay, per-org derived keys + (org, trace, kind) AAD.
3. **L1 response-cache ciphertext** — old entries must be invalidated as part of
   a rotation.
4. **Account-purge request capabilities** — old signatures must be allowed to
   expire before normal serving resumes.

Hosted deployments have additional persistent key families. A procedure that
only re-seals the two public Postgres tables is therefore incomplete.

The public crate exposes two useful re-encryption primitives:

- **`tt_auth::postgres::PostgresProviderCredentialStore::reencrypt_all(&new_master_key)`**
  (crates/auth/src/postgres.rs) — re-seals every `provider_credentials` row in a
  single all-or-nothing transaction; returns the row count.
- **`tt_telemetry::body_capture::postgres::PostgresBodyCaptureWriter::reencrypt_all(&new_master_key)`**
  (crates/telemetry/src/body_capture.rs, behind the `postgres` feature) — re-seals
  every `request_body_captures` row in keyset-paginated batches, each committed
  in its own transaction so the pass is **resumable** and **idempotent** (a re-run
  skips rows already sealed under the new key). Returns `ReencryptStats`
  (`scanned` / `reencrypted` / `already_current`). A row that decrypts under
  neither key aborts its batch and errors rather than being silently dropped.

They are building blocks, not an operator-facing rotation command.

Migration 0046 adds a key-material-free `public.master_key_rotation` journal
shared with the hosted control plane. The gateway consumes that journal at boot
and on `/ready`:

- no journal row preserves pre-first-rotation self-hosted behavior;
- `in_progress` always fences normal serving;
- `awaiting_promotion` and `complete` only allow the key whose
  domain-separated fingerprint matches the journal;
- unknown states, an absent key, or a fingerprint mismatch fail closed.

The journal stores fingerprints, phase, and timestamps—never either root key.
Do not edit it manually or treat it as the rotation coordinator. Hosted
deployments must use their reviewed control-plane maintenance procedure to
freeze writers, re-encrypt every persistent family, invalidate caches, drain
old short-lived capabilities, promote the new key, verify retained ciphertext,
and only then complete the journal.

For public-only/self-hosted deployments, do not attempt an in-place rotation
until your maintenance tooling covers every enabled family listed above. If a
root may have been exposed and no complete tool exists, keep the service
stopped while you recover or deliberately discard affected encrypted state;
silently proceeding with a partial two-table pass is not a safe rotation.

## Operator checklist (the human-gated part)

- [ ] Generate fresh **test** credentials; rewrite `./.env.development` to use only those.
- [ ] `fly secrets set` every production secret on the gateway and `tt-api` apps.
- [ ] Delete `./.env.production` from the workstation once `fly secrets` is the source of truth.
- [ ] Rotate the high-value keys above (`TT_MASTER_KEY` requires a complete, journaled maintenance procedure; individual table helpers are insufficient).
- [ ] Confirm `.gitignore` still covers `.env*` (it does) and that no secret ever entered git history (`git log -p -- .env*` → empty).

See also: [`SECURITY.md`](../SECURITY.md), `.env.example` (the committed, value-less template).
