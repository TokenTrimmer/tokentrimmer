# Workflow contracts

The public generator owns three workflow artifacts:

- `workflow-definition.schema.json` comes from the exact persisted and returned
  Rust `WorkflowDefinition` type.
- `workflow-write.schema.json` comes from the exact `POST /v1/workflows`
  `CreateWorkflowRequest` parser, including its optional write-only identity
  fields and optional optimistic `expected_latest_version` precondition.
- `workflow-definition-v1.golden.json` is serialized from a real Rust workflow
  definition covering typed model selection, output-token and cost admission
  fields, edges, metadata, budget, and a schedule trigger.

The matching TypeScript is generated into
`bindings/product-contracts.generated.ts`. These artifacts describe the wire
shape; graph validation, egress policy, budget admission, authorization,
provider readiness, and execution remain authoritative runtime checks.

Consumers must vendor them from the same immutable public revision as the
gateway crates and make source drift a blocking CI failure.
