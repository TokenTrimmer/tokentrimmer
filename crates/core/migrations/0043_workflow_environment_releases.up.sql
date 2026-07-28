-- Immutable workflow environment-release ledger plus one concurrency-safe
-- current pointer per environment.
--
-- Saving a workflow continues to append workflow_definitions. A version is a
-- draft until an explicit release operation points an environment at it.
-- Release pointers do not silently change legacy latest-version execution;
-- callers must opt into an environment-aware execution contract separately.

CREATE TABLE IF NOT EXISTS workflow_environment_state (
  org_id            UUID NOT NULL,
  workflow_id       UUID NOT NULL,
  environment       TEXT NOT NULL
                    CHECK (environment IN ('development', 'staging', 'production')),
  revision          INT NOT NULL CHECK (revision > 0),
  workflow_version  INT NOT NULL CHECK (workflow_version > 0),
  updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),

  CONSTRAINT workflow_environment_state_pkey
    PRIMARY KEY (org_id, workflow_id, environment),
  CONSTRAINT workflow_environment_state_definition_fk
    FOREIGN KEY (org_id, workflow_id, workflow_version)
    REFERENCES workflow_definitions (org_id, id, version)
    ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS workflow_environment_releases (
  org_id              UUID NOT NULL,
  workflow_id         UUID NOT NULL,
  environment         TEXT NOT NULL
                      CHECK (environment IN ('development', 'staging', 'production')),
  revision            INT NOT NULL CHECK (revision > 0),
  workflow_version    INT NOT NULL CHECK (workflow_version > 0),
  action              TEXT NOT NULL CHECK (action IN ('publish', 'promote', 'rollback')),
  source_environment  TEXT
                      CHECK (source_environment IS NULL OR source_environment IN ('development', 'staging', 'production')),
  source_revision     INT CHECK (source_revision IS NULL OR source_revision > 0),
  created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),

  CONSTRAINT workflow_environment_releases_pkey
    PRIMARY KEY (org_id, workflow_id, environment, revision),
  CONSTRAINT workflow_environment_releases_definition_fk
    FOREIGN KEY (org_id, workflow_id, workflow_version)
    REFERENCES workflow_definitions (org_id, id, version)
    ON DELETE CASCADE,
  CONSTRAINT workflow_environment_releases_transition_check CHECK (
    (
      action = 'publish'
      AND environment = 'development'
      AND source_environment IS NULL
      AND source_revision IS NULL
    )
    OR (
      action = 'promote'
      AND source_revision IS NOT NULL
      AND (
        (environment = 'staging' AND source_environment = 'development')
        OR (environment = 'production' AND source_environment = 'staging')
      )
    )
    OR (
      action = 'rollback'
      AND source_environment = environment
      AND source_revision IS NOT NULL
    )
  )
);

COMMENT ON TABLE workflow_environment_state IS
  'Concurrency-safe current immutable workflow version per development/staging/production environment.';
COMMENT ON TABLE workflow_environment_releases IS
  'Append-only workflow environment release history; contains version metadata only, never definition values or secrets.';
