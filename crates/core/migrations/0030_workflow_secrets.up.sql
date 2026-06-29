CREATE TABLE IF NOT EXISTS workflow_secrets (
    org_id     UUID         NOT NULL,
    name       TEXT         NOT NULL,
    secret_enc BYTEA        NOT NULL,
    created_at TIMESTAMPTZ  NOT NULL DEFAULT now(),
    rotated_at TIMESTAMPTZ,
    PRIMARY KEY (org_id, name)
);
