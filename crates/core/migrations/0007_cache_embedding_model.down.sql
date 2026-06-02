-- Revert migration 0007: remove embedding_model column and its index.
DROP INDEX IF EXISTS cache_entries_model_idx;
ALTER TABLE cache_entries DROP COLUMN IF EXISTS embedding_model;
