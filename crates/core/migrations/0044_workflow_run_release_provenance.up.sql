-- Optional immutable release provenance for explicitly environment-bound
-- workflow runs. Legacy latest-version and exact-version runs keep both fields
-- NULL. Environment-bound runs retain the exact release revision and version
-- selected before execution, even if that environment later advances.

ALTER TABLE workflow_environment_releases
  ADD CONSTRAINT workflow_environment_releases_run_reference_key
  UNIQUE (org_id, workflow_id, environment, revision, workflow_version);

ALTER TABLE workflow_runs
  ADD COLUMN release_environment TEXT,
  ADD COLUMN release_revision INT,
  ADD CONSTRAINT workflow_runs_release_pair_check CHECK (
    (release_environment IS NULL AND release_revision IS NULL)
    OR (release_environment IS NOT NULL AND release_revision IS NOT NULL)
  ),
  ADD CONSTRAINT workflow_runs_release_environment_check CHECK (
    release_environment IS NULL
    OR release_environment IN ('development', 'staging', 'production')
  ),
  ADD CONSTRAINT workflow_runs_release_revision_check CHECK (
    release_revision IS NULL OR release_revision > 0
  ),
  ADD CONSTRAINT workflow_runs_release_provenance_fk
    FOREIGN KEY (
      org_id,
      workflow_id,
      release_environment,
      release_revision,
      version
    )
    REFERENCES workflow_environment_releases (
      org_id,
      workflow_id,
      environment,
      revision,
      workflow_version
    );

COMMENT ON COLUMN workflow_runs.release_environment IS
  'Closed environment selector used for this run, or NULL for legacy latest/exact-version execution.';
COMMENT ON COLUMN workflow_runs.release_revision IS
  'Exact immutable release revision resolved before execution, paired with release_environment.';
