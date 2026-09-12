# Cached-response encryption and legacy-read rollout

Scope: the gateway's response caches and the `tt-cache` codec. This is a source/local-test contract, not evidence of a deployed cache posture, completed purge or key-rotation drill.

## Configuration and authority

The gateway's existing L1/L2 construction paths both use `ResponseCodec::from_env()`.

| Configuration | Behavior |
| --- | --- |
| Valid `TT_MASTER_KEY`, `TT_REQUIRE_ENCRYPTED_CACHE=1` or case-insensitive `true` | Encrypt writes; refuse legacy plaintext reads as misses |
| Valid key, required-encryption flag unset/false | Encrypt writes; retain explicit self-hosted legacy-read compatibility |
| No key, required-encryption flag enabled | Existing gateway guard disables keyless response caches; this does not necessarily stop the whole gateway |
| Malformed key, including a non-Unicode environment value | Codec loading fails; existing gateway wiring disables the affected cache, never silently treats the malformed key as absent |
| No key, required-encryption flag unset | Explicit self-hosted plaintext mode remains available |

Programmatic `ResponseCodec::new(key)` keeps legacy-read compatibility. Use `.reject_legacy_plaintext()` to require authenticated encrypted reads. These low-level constructors do not infer deployment authority from the environment. Compatibility mode accepts unauthenticated legacy plaintext: do not describe every cache read as integrity-protected just because new writes are encrypted.

Use the deployment's securely supplied, approved key. **Do not regenerate or rotate `TT_MASTER_KEY` to adopt this change**: it is shared with other encrypted families. This patch introduces no key-generation overlap, backfill or rotation coordinator.

## Encoding and refusal

- Ciphertext bytes, XChaCha20-Poly1305, the cache-specific KDF/AAD, and per-org/context bindings are unchanged.
- L2's supported stored envelope has exactly `__tt_cache_enc__: 1` (an integer) and a string `blob`. Presence of the marker establishes an envelope before validation. Missing/non-string blobs, unsupported versions, extra fields, invalid hex and failed authentication are misses, not legacy plaintext.
- L1 reserves the `\0tt-l1-enc:` namespace. Only the complete v1 header is decrypted; unknown/truncated reserved versions are misses. Codec-disabled memory/Redis readers do not return recognized ciphertext bytes as cache values.
- Strict in-memory L2 also refuses unsealed non-JSON bytes. The memory backend's permissive opaque-byte fixtures must not bypass required encryption.
- The existing `Undecryptable` outcome also represents policy rejection; it does not always mean an AEAD operation failed.

This encrypts L1 value envelopes and L2 response payloads, not all stored information. L2 embeddings, model identifiers, usage/cost metadata and lexical sketches remain plaintext and can be sensitive. Encryption is not anonymization or evidence of financial correctness.

## Legacy data is not erased by refusing reads

This change performs **no automatic purge, backfill or live data migration**. Legacy values remain at rest until expiry, approved eviction/purge or an independently reviewed migration. A miss can increase provider work, latency and spend; honor normal routing/budget/privacy guards and approve the expected refill allowance.

L2 still selects one nearest candidate. If that candidate is rejected, the lookup misses; it does not automatically search a second candidate or delete the first. A surviving legacy/malformed candidate can continue masking a newly encrypted row until expiry or approved eviction. Plan removal before assuming a warm encrypted refill.

L1 may also contain negative responses or retained agent transcripts (`tt:runs:…`, including older recognized key shapes). **Do not assume every L1 value is disposable completion cache data.** Inventory active-run dependencies and arrange a verified drain/migration/recovery plan before making legacy transcripts unreadable or purging them.

An operator-controlled rollout should:

1. Inventory the actual namespaces/tables, key configuration, entry ages/TTLs, plaintext/marked-envelope counts and active consumers without exporting raw customer content. Include historical/unindexed L1 namespaces.
2. Retain an approved existing key and verify decrypt/tenant/context isolation on representative encrypted fixtures. Do not enable plaintext fallback on hosted traffic as a migration shortcut.
3. Decide per family whether to wait for expiry, drain, migrate or purge. Use approved tenant-scoped mechanisms; never substitute a global Redis flush or an unscoped SQL delete. This batch provides no production backfill utility or all-family purge acceptance.
4. Exercise the transition at candidate volume, with cost/latency headroom, interrupted rollout and compatible rollback. Old replicas can still accept plaintext; do not claim fleet-wide strict reads until version convergence or approved legacy removal is evidenced.
5. Verify raw encrypted writes and strict reads on the deployed candidate, retained-data deletion and key lifecycle separately. Local refusal/refill tests do not close those gates.

## Local reproduction

Use only disposable loopback PostgreSQL 18 with pgvector and a disposable Redis. The new PostgreSQL test applies the actual public core migrations; never point it at a customer/staging/production database. Environment-policy cases run in child processes, avoiding global environment races in parallel tests.

Set **all four** database/Redis variables explicitly: some older tests silently return if `DATABASE_URL` or `REDIS_URL` is absent. Such a return is not acceptance.

```sh
# All URLs must identify the disposable fixtures, not a deployed service.
export DATABASE_URL="$DISPOSABLE_POSTGRES_URL"
export TEST_DATABASE_URL="$DISPOSABLE_POSTGRES_URL"
export REDIS_URL="$DISPOSABLE_REDIS_URL"
export TEST_REDIS_URL="$DISPOSABLE_REDIS_URL"
cargo test -p tt-cache --locked --offline -- --include-ignored --test-threads=1
cargo clippy -p tt-cache --all-targets --locked --offline -- -D warnings
cargo fmt --check -p tt-cache
```

The tests cover environment policy, malformed/future envelopes, non-Unicode key refusal, both in-memory caches, actual Redis/PostgreSQL writes and reads, explicit fixture eviction/refill, wrong keys and tenant/row binding. They do not exercise a deployed gateway, mixed fleet, real provider inference, active customer-run migration or production deletion/key rotation.
