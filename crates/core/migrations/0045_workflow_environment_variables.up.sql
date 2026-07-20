-- Versioned, non-secret workflow configuration scoped to one release
-- environment. Values are replaced as a complete bounded set; immutable
-- snapshots make the accepted configuration revision durable run provenance.

CREATE TABLE IF NOT EXISTS workflow_environment_variable_sets (
  org_id       UUID NOT NULL,
  workflow_id  UUID NOT NULL,
  environment  TEXT NOT NULL
               CHECK (environment IN ('development', 'staging', 'production')),
  revision     INT NOT NULL CHECK (revision > 0),
  variables    JSONB NOT NULL CHECK (jsonb_typeof(variables) = 'object'),
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),

  CONSTRAINT workflow_environment_variable_sets_pkey
    PRIMARY KEY (org_id, workflow_id, environment, revision)
);

CREATE TABLE IF NOT EXISTS workflow_environment_variable_state (
  org_id       UUID NOT NULL,
  workflow_id  UUID NOT NULL,
  environment  TEXT NOT NULL
               CHECK (environment IN ('development', 'staging', 'production')),
  revision     INT NOT NULL CHECK (revision > 0),
  updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),

  CONSTRAINT workflow_environment_variable_state_pkey
    PRIMARY KEY (org_id, workflow_id, environment),
  CONSTRAINT workflow_environment_variable_state_set_fk
    FOREIGN KEY (org_id, workflow_id, environment, revision)
    REFERENCES workflow_environment_variable_sets
      (org_id, workflow_id, environment, revision)
);

ALTER TABLE workflow_runs
  ADD COLUMN variables_revision INT;

-- Environment-bound rows accepted before this surface existed used the
-- implicit empty set. Backfill that exact revision before enforcing pairing.
UPDATE workflow_runs
SET variables_revision = 0
WHERE release_environment IS NOT NULL;

ALTER TABLE workflow_runs
  ADD CONSTRAINT workflow_runs_variables_revision_check CHECK (
    variables_revision IS NULL OR variables_revision >= 0
  ),
  ADD CONSTRAINT workflow_runs_variables_scope_check CHECK (
    (variables_revision IS NULL) = (release_environment IS NULL)
  );

COMMENT ON TABLE workflow_environment_variable_sets IS
  'Append-only non-secret workflow environment variable snapshots; revision 0 is the implicit empty set.';
COMMENT ON TABLE workflow_environment_variable_state IS
  'Current non-secret workflow variable snapshot per development/staging/production environment.';
COMMENT ON COLUMN workflow_runs.variables_revision IS
  'Exact accepted environment-variable revision; 0 means the implicit empty set and NULL means non-environment execution.';
