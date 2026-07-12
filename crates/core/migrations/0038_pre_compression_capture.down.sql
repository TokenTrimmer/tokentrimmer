-- Revert: drop the pre-compression prompt capture columns.
ALTER TABLE request_body_captures
  DROP COLUMN pre_compression_request_bytes;
ALTER TABLE request_body_captures
  DROP COLUMN pre_compression_request_enc;
