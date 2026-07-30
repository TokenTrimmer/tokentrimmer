-- Shared with the hosted control plane. A TT_MASTER_KEY rotation spans
-- multiple persistent ciphertext/HMAC families, so normal serving must fail
-- closed while a bounded, resumable pass is incomplete.
CREATE TABLE IF NOT EXISTS public.master_key_rotation (
    singleton               BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    state                   TEXT NOT NULL CHECK (
                                state IN (
                                    'in_progress',
                                    'awaiting_promotion',
                                    'complete'
                                )
                            ),
    old_key_fingerprint     TEXT NOT NULL CHECK (
                                char_length(old_key_fingerprint) = 64
                            ),
    new_key_fingerprint     TEXT NOT NULL CHECK (
                                char_length(new_key_fingerprint) = 64
                                AND new_key_fingerprint <> old_key_fingerprint
                            ),
    phase                   TEXT NOT NULL CHECK (
                                phase IN (
                                    'preflight',
                                    'provider_credentials',
                                    'cleanup_credentials',
                                    'managed_chat_keys',
                                    'workflow_secrets',
                                    'body_captures',
                                    'retrieval_audit',
                                    'otlp_headers',
                                    'postgres_cache_invalidated',
                                    'awaiting_promotion',
                                    'verified'
                                )
                            ),
    started_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now(),
    rekeyed_at              TIMESTAMPTZ,
    completed_at            TIMESTAMPTZ,
    CHECK (
        (state = 'in_progress' AND completed_at IS NULL)
        OR
        (state = 'awaiting_promotion'
            AND phase = 'awaiting_promotion'
            AND rekeyed_at IS NOT NULL
            AND completed_at IS NULL)
        OR
        (state = 'complete'
            AND phase = 'verified'
            AND rekeyed_at IS NOT NULL
            AND completed_at IS NOT NULL)
    )
);

COMMENT ON TABLE public.master_key_rotation IS
    'Singleton, key-material-free journal/fail-closed boot fence for resumable TT_MASTER_KEY rotation.';
COMMENT ON COLUMN public.master_key_rotation.old_key_fingerprint IS
    'Domain-separated SHA-256 fingerprint; never the root key or a reversible derivative.';
COMMENT ON COLUMN public.master_key_rotation.new_key_fingerprint IS
    'Domain-separated SHA-256 fingerprint; never the root key or a reversible derivative.';
