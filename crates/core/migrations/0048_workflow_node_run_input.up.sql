-- Per-node captured input for the workflow node journal (debugger/replay).
-- Bounded + value-free by construction (secrets redacted to "***"); Nullable so
-- existing rows and nodes with no template input remain valid.
ALTER TABLE workflow_node_runs ADD COLUMN input JSONB;
