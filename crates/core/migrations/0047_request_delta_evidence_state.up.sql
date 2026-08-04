-- Preserve request-delta provenance independently from the numeric cost tuple.
-- Historical and rolling-deploy writes default to missing_evidence: a stored
-- zero cannot prove whether pricing was genuinely zero or unavailable.
ALTER TABLE public.request_logs
    ADD COLUMN request_delta_evidence_state TEXT NOT NULL
        DEFAULT 'missing_evidence';

ALTER TABLE public.request_logs
    ADD CONSTRAINT request_logs_request_delta_evidence_state_check
        CHECK (
            request_delta_evidence_state IN (
                'measured',
                'unpriceable',
                'missing_evidence'
            )
        );

COMMENT ON COLUMN public.request_logs.request_delta_evidence_state IS
    'Closed write-time provenance for tt.request-delta-estimate.v1; historical/old-writer rows default to missing_evidence and are never inferred from numeric zero.';

-- L2 hits must carry the miss row's provenance too. A stored numeric baseline
-- alone cannot prove the originally-requested model was priceable.
ALTER TABLE public.cache_entries
    ADD COLUMN request_delta_evidence_state TEXT NOT NULL
        DEFAULT 'missing_evidence';

ALTER TABLE public.cache_entries
    ADD CONSTRAINT cache_entries_request_delta_evidence_state_check
        CHECK (
            request_delta_evidence_state IN (
                'measured',
                'unpriceable',
                'missing_evidence'
            )
        );

COMMENT ON COLUMN public.cache_entries.request_delta_evidence_state IS
    'Miss-row request-delta provenance carried into L2 hits; historical entries default to missing_evidence.';
