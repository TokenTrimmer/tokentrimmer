ALTER TABLE workflow_runs
  DROP CONSTRAINT IF EXISTS workflow_runs_release_provenance_fk,
  DROP CONSTRAINT IF EXISTS workflow_runs_release_revision_check,
  DROP CONSTRAINT IF EXISTS workflow_runs_release_environment_check,
  DROP CONSTRAINT IF EXISTS workflow_runs_release_pair_check,
  DROP COLUMN IF EXISTS release_revision,
  DROP COLUMN IF EXISTS release_environment;

ALTER TABLE workflow_environment_releases
  DROP CONSTRAINT IF EXISTS workflow_environment_releases_run_reference_key;
