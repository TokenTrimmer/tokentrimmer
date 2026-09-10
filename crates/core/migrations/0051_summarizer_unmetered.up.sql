-- Distinguish "summarizer ran and cost was measured" from "summarizer ran but
-- cost was unmetered" from "no summarizer ran". A $0 summarizer_tax_usd is
-- ambiguous today: a timed-out, unmetered, or free-tier summarizer call all
-- look the same. This flag lets the savings SQL and dashboard reports see
-- incomplete cost coverage instead of a healthy zero.
--
-- No backfill: existing rows with summarizer_tax_usd = 0 have no durable
-- evidence that a summarizer ran (the $0/NULL distinction was not recorded).
-- They remain at summarizer_ran = FALSE (the conservative default).
ALTER TABLE request_logs
  ADD COLUMN IF NOT EXISTS summarizer_ran BOOLEAN NOT NULL DEFAULT FALSE;

-- Rows with a measured nonzero tax are definitionally summarizer-involved.
UPDATE request_logs
   SET summarizer_ran = TRUE
 WHERE summarizer_tax_usd > 0;
