ALTER TABLE workflow_runs
  DROP CONSTRAINT IF EXISTS workflow_runs_variables_scope_check,
  DROP CONSTRAINT IF EXISTS workflow_runs_variables_revision_check,
  DROP COLUMN IF EXISTS variables_revision;

DROP TABLE IF EXISTS workflow_environment_variable_state;
DROP TABLE IF EXISTS workflow_environment_variable_sets;
