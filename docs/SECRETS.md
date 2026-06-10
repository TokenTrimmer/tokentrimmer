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
| `TT_MASTER_KEY` | Decrypts **all** stored provider credentials (XChaCha20-Poly1305, per-row derived keys). | **Rotation requires re-encrypting `provider_credentials`** — you cannot just swap the key. See the re-encryption procedure below. |
| `TT_AUDIT_SIGNING_KEY` (Ed25519) | Forge audit-chain signatures. | Rotate the keypair; record the rotation as an audit event; publish the new verifying key. Old entries stay verifiable under the old public key. |
| `TT_ADMIN_TOKEN` (cloud) | Full admin API access (`/v1/admin/*`). | Rotate immediately; single bearer value, no migration needed. |
| `FLY_API_TOKEN` | Deploy/destroy infrastructure. | `fly tokens revoke` + issue a new scoped deploy token. |
| `STRIPE_SECRET_KEY` / webhook signing secret | Billing actions / forged webhooks. | Roll in the Stripe dashboard; update `fly secrets`; re-point the webhook endpoint secret. |
| Provider keys (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, …) | Spend on your provider bill. | Roll at each provider; update `fly secrets`. These are normally **per-tenant** in `provider_credentials`; a top-level key only exists for dogfooding and is only ever served when `TT_ALLOW_ENV_CREDENTIAL_FALLBACK=1` is set explicitly (the gateway is BYO-only by default). |

## `TT_MASTER_KEY` rotation (re-encryption)

Because every row in `provider_credentials` is sealed with a key **derived from**
`TT_MASTER_KEY` (+ per-(org,provider) salt + AAD), rotating the master key means
re-sealing every credential:

The re-encryption primitive now exists:
**`tt_auth::postgres::PostgresProviderCredentialStore::reencrypt_all(&new_master_key)`**
(crates/auth/src/postgres.rs) decrypts every row under the current key and
re-seals it under the new one in a single all-or-nothing transaction. Procedure:

1. Build the store with the **current** key: `PostgresProviderCredentialStore::from_env(pool)`.
2. Call `store.reencrypt_all(&new_key_bytes)` — returns the row count re-sealed
   (transaction rolls back if any row fails, so the table is never half-rotated).
3. Promote the new key → `TT_MASTER_KEY` (e.g. `fly secrets set`); restart so the
   gateway/api read it. Remove the old key.
4. Verify a sample credential decrypts under the new key; record the rotation as
   an audit event.

Do **not** swap `TT_MASTER_KEY` in place without running `reencrypt_all` first, or
every stored credential becomes undecryptable. _(An operator-facing `tt`
subcommand wrapping `reencrypt_all` is a thin follow-up; the primitive + its
DB-free round-trip test ship today.)_

## Operator checklist (the human-gated part)

- [ ] Generate fresh **test** credentials; rewrite `./.env.development` to use only those.
- [ ] `fly secrets set` every production secret on the gateway and `tt-api` apps.
- [ ] Delete `./.env.production` from the workstation once `fly secrets` is the source of truth.
- [ ] Rotate the high-value keys above (run the `TT_MASTER_KEY` re-encryption first).
- [ ] Confirm `.gitignore` still covers `.env*` (it does) and that no secret ever entered git history (`git log -p -- .env*` → empty).

See also: [`SECURITY.md`](../SECURITY.md), `.env.example` (the committed, value-less template).
