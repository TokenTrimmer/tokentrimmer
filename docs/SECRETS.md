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
| `TT_MASTER_KEY` | Shared root for provider/cleanup credentials, managed Chat keys, workflow secrets, body/pre-compression captures, retrieval-audit prompts, OTLP headers, cache ciphertext, keyed Stripe tombstones, and gateway purge capabilities. | Never swap in place. Use the hosted maintenance command and shared boot fence described below. |
| `TT_AUDIT_SIGNING_KEY` (Ed25519) | Forge audit-chain signatures. | Rotate the keypair; record the rotation as an audit event; publish the new verifying key. Old entries stay verifiable under the old public key. |
| `TT_ADMIN_TOKEN` (cloud) | Full admin API access (`/v1/admin/*`). | Rotate immediately; single bearer value, no migration needed. |
| `FLY_API_TOKEN` | Deploy/destroy infrastructure. | `fly tokens revoke` + issue a new scoped deploy token. |
| `STRIPE_SECRET_KEY` / webhook signing secret | Billing actions / forged webhooks. | Roll in the Stripe dashboard; update `fly secrets`; re-point the webhook endpoint secret. |
| Provider keys (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, …) | Spend on your provider bill. | Roll at each provider; update `fly secrets`. These are normally **per-tenant** in `provider_credentials`; a top-level key only exists for dogfooding and is only ever served when `TT_ALLOW_ENV_CREDENTIAL_FALLBACK=1` is set explicitly (the gateway is BYO-only by default). |

## `TT_MASTER_KEY` rotation (re-encryption)

The older two-table procedure was incomplete. The root also protects Cloud
ciphertext families, cache values, keyed Stripe deletion tombstones, and the
gateway's short-lived account-purge capability.

Public migration 0046 installs the same key-material-free singleton journal as
Cloud migration 0084. The gateway checks it at boot and readiness:

- `in_progress` always refuses serving;
- `awaiting_promotion` and `complete` accept only the journal's promoted-key
  fingerprint;
- an absent row preserves pre-first-rotation self-host behavior.

The hosted reviewed binary owns the coordinated maintenance surface:

```text
tt-api --rotate-master-key
tt-api --verify-master-key-rotation
```

It covers provider and cleanup credentials, managed Chat keys, workflow
secrets, request/response/pre-compression captures, retrieval-audit prompts,
OTLP headers, Postgres L2 invalidation, and a scoped Redis `tt:l1:*`
SCAN/UNLINK plus empty verification. Keyed Stripe tombstones cannot be
re-derived because the raw subscription id is intentionally absent, so the
command requires a hard-deletion freeze and an empty 35-day tombstone window.
Gateway/API workers must be at zero for at least the 120-second purge-capability
TTL. Post-promotion verification opens every retained ciphertext under the new
root before the journal completes.

Do not call the provider/body-capture primitives directly as an operational
rotation; they are building blocks, not complete coverage. Follow the full
preconditions, backup-key custody, failure boundaries, and rollout verification
in the Cloud repository's `docs/MASTER_KEY_ROTATION.md`. Source and local tests
do not prove that a deployed rotation has been exercised.

## Operator checklist (the human-gated part)

- [ ] Generate fresh **test** credentials; rewrite `./.env.development` to use only those.
- [ ] `fly secrets set` every production secret on the gateway and `tt-api` apps.
- [ ] Delete `./.env.production` from the workstation once `fly secrets` is the source of truth.
- [ ] Rotate the high-value keys above (for `TT_MASTER_KEY`, use the complete journaled maintenance command and deployed drill, never individual table helpers).
- [ ] Confirm `.gitignore` still covers `.env*` (it does) and that no secret ever entered git history (`git log -p -- .env*` → empty).

See also: [`SECURITY.md`](../SECURITY.md), `.env.example` (the committed, value-less template).
