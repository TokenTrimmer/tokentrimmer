-- TR-3: before/after prompt diff (the "single highest-trust artifact for a
-- skeptical engineer" — show exactly what the compress pass deleted).
--
-- `pre_compression_request_enc` is the encrypted (XChaCha20-Poly1305) JSON of
-- the request as it was BEFORE the conservative `compress` pass
-- (`RouteAction::compress`) ran. Snapshot + stored ONLY when body capture is
-- armed + enabled AND a compress pass committed (capture-gated by design —
-- prompt diffs contain PII, so they MUST live encrypted on the capture table,
-- never unencrypted on request_logs, and they inherit the capture's retention
-- bound / org opt-in). NULL when capture was off, no route opted into `compress`,
-- or the row predates migration 0038.
--
-- `pre_compression_request_bytes` is the ORIGINAL (pre-truncation) byte length,
-- recorded honestly even when the body was truncated to the codec's max before
-- encryption (mirrors `request_bytes`).
--
-- The unified-diff rendering lives on /logs/[trace] (cloud), computed server-side
-- from this + the existing `request_enc` (the post-compression body).
ALTER TABLE request_body_captures
  ADD COLUMN IF NOT EXISTS pre_compression_request_enc BYTEA;
ALTER TABLE request_body_captures
  ADD COLUMN IF NOT EXISTS pre_compression_request_bytes INT;
