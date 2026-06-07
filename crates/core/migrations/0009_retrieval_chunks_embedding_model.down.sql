-- Revert migration 0009: remove embedding_model column and its index.
DROP INDEX IF EXISTS retrieval_chunks_model_idx;
ALTER TABLE retrieval_chunks DROP COLUMN IF EXISTS embedding_model;
