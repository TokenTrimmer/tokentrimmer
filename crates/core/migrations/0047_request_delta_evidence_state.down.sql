ALTER TABLE public.request_logs
    DROP COLUMN IF EXISTS request_delta_evidence_state;

ALTER TABLE public.cache_entries
    DROP COLUMN IF EXISTS request_delta_evidence_state;
