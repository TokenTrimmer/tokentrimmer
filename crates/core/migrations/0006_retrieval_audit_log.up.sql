-- Encrypted-prompt audit log for retrieval substitutions (Track E spec §10).
--
-- Stores the ORIGINAL prompt (pre-substitution) encrypted with a key derived
-- from TT_MASTER_KEY, so a customer can run an offline quality audit of what a
-- retrieval substitution replaced — without TokenTrimmer staff (or anyone who
-- exfiltrates this table) being able to read the prompt. Mirrors the
-- provider-credentials crypto: XChaCha20-Poly1305, per-row key derived from
-- TT_MASTER_KEY + org_id, AAD bound to org_id.
--
-- Retention: 30 days, enforced by a periodic sweep
-- (DELETE WHERE created_at < now() - interval '30 days').

CREATE TABLE retrieval_audit_log (
  id            UUID         PRIMARY KEY,
  org_id        UUID         NOT NULL,
  -- `nonce (24 bytes) || ciphertext` of the original prompt JSON.
  prompt_enc    BYTEA        NOT NULL,
  substitutions INT          NOT NULL DEFAULT 0,
  tokens_saved  BIGINT       NOT NULL DEFAULT 0,
  created_at    TIMESTAMPTZ  NOT NULL DEFAULT now()
);

-- Per-org reads (the offline audit view), newest first.
CREATE INDEX retrieval_audit_log_org_created ON retrieval_audit_log (org_id, created_at DESC);

-- Supports the 30-day retention sweep without scanning the whole table.
CREATE INDEX retrieval_audit_log_created ON retrieval_audit_log (created_at);
