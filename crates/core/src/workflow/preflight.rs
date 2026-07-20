//! Bounded whole-tree workflow preparation before any node work begins.
//!
//! Nested workflow definitions are loaded in org-scoped depth batches, checked
//! for every HTTP secret reference, and retained for the run. Reusing those
//! exact definitions prevents a latest-version change between preflight and
//! execution from bypassing the check.

use std::collections::{BTreeSet, HashMap, HashSet};

use sqlx::{PgPool, Row as _};
use tt_shared::context::SecretString;
use uuid::Uuid;

use crate::error::ApiError;

use super::{
    executor::NodeExecutor,
    secrets::required_secret_names,
    types::{NodeKind, WorkflowDefinition},
};

/// Bound definition loading independently of the runtime node-execution cap.
/// A legitimate five-level tree should remain far below this, while an
/// adversarial fan-out cannot turn preflight into unbounded database work.
pub(crate) const MAX_PREFLIGHT_WORKFLOW_DEFINITIONS: usize = 256;

const GET_LATEST_DEFINITIONS_SQL: &str = "\
SELECT DISTINCT ON (id) id, definition \
FROM workflow_definitions \
WHERE org_id = $1 AND id = ANY($2) \
ORDER BY id, version DESC";

/// Exact nested definitions checked before this run. The map is immutable once
/// prepared and is shared by every recursive engine invocation.
pub(crate) struct PreparedWorkflowTree {
    definitions: HashMap<Uuid, WorkflowDefinition>,
}

impl PreparedWorkflowTree {
    pub(crate) fn definition(&self, id: &Uuid) -> Option<&WorkflowDefinition> {
        self.definitions.get(id)
    }
}

/// Load the latest definition for every requested id in one org-scoped query.
/// Missing ids are deliberately absent from the result so the caller can
/// report the same opaque not-found boundary as the single-definition loader.
pub(crate) async fn load_latest_definitions(
    pool: &PgPool,
    org_id: Uuid,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, WorkflowDefinition>, ApiError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(GET_LATEST_DEFINITIONS_SQL)
        .bind(org_id)
        .bind(ids)
        .fetch_all(pool)
        .await
        .map_err(|error| {
            tracing::warn!(%org_id, %error, "nested workflow preflight load failed");
            ApiError::ServiceUnavailable("workflow store unavailable".into())
        })?;

    let mut definitions = HashMap::with_capacity(rows.len());
    for row in rows {
        let id: Uuid = row
            .try_get("id")
            .map_err(|_| ApiError::Internal("nested workflow preflight id decode failed".into()))?;
        let value: serde_json::Value = row.try_get("definition").map_err(|_| {
            ApiError::Internal("nested workflow preflight definition decode failed".into())
        })?;
        let definition: WorkflowDefinition = serde_json::from_value(value).map_err(|_| {
            ApiError::Internal("nested workflow preflight definition is invalid".into())
        })?;
        if definition.id != id {
            return Err(ApiError::Internal(
                "nested workflow preflight definition id mismatch".into(),
            ));
        }
        definitions.insert(id, definition);
    }
    Ok(definitions)
}

/// Load and check every nested definition that the engine could enter within
/// its depth limit. All required names must exist in the already-decrypted
/// org-scoped map before the caller may execute the root Trigger or any node.
pub(crate) async fn prepare_workflow_tree(
    executor: &dyn NodeExecutor,
    root: &WorkflowDefinition,
    secrets: &HashMap<String, SecretString>,
    root_depth: u32,
    max_depth: u32,
) -> Result<PreparedWorkflowTree, String> {
    let mut required = BTreeSet::new();
    collect_required_names(root, &mut required)?;

    let mut definitions = HashMap::new();
    let mut seen = HashSet::from([root.id]);
    let mut frontier = if root_depth < max_depth {
        referenced_workflow_ids(root)
    } else {
        Vec::new()
    };
    let mut child_depth = root_depth.saturating_add(1);

    while !frontier.is_empty() {
        frontier.sort_unstable();
        frontier.dedup();
        let ids = frontier
            .drain(..)
            .filter(|id| seen.insert(*id))
            .collect::<Vec<_>>();
        if ids.is_empty() {
            break;
        }
        if definitions.len().saturating_add(ids.len()) > MAX_PREFLIGHT_WORKFLOW_DEFINITIONS {
            return Err(format!(
                "workflow secret preflight failed: nested definition limit exceeded ({MAX_PREFLIGHT_WORKFLOW_DEFINITIONS})"
            ));
        }

        let mut loaded = executor.load_subworkflows(&ids).await.map_err(|error| {
            format!("workflow secret preflight failed: nested workflow load failed: {error}")
        })?;
        let mut next = Vec::new();
        for id in ids {
            let definition = loaded.remove(&id).ok_or_else(|| {
                format!("workflow secret preflight failed: nested workflow {id} was not found")
            })?;
            if definition.id != id {
                return Err("workflow secret preflight failed: nested workflow id mismatch".into());
            }
            collect_required_names(&definition, &mut required)?;
            if child_depth < max_depth {
                next.extend(referenced_workflow_ids(&definition));
            }
            definitions.insert(id, definition);
        }
        frontier = next;
        child_depth = child_depth.saturating_add(1);
    }

    let missing = required
        .into_iter()
        .filter(|name| !secrets.contains_key(name))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!(
            "workflow secret preflight failed: missing or unusable secret(s): {}",
            missing.join(", ")
        ));
    }

    Ok(PreparedWorkflowTree { definitions })
}

fn collect_required_names(
    definition: &WorkflowDefinition,
    required: &mut BTreeSet<String>,
) -> Result<(), String> {
    match required_secret_names(definition) {
        Ok(names) => {
            required.extend(names);
            Ok(())
        }
        Err(errors) => Err(format!(
            "workflow secret preflight failed: {}",
            errors.join("; ")
        )),
    }
}

fn referenced_workflow_ids(definition: &WorkflowDefinition) -> Vec<Uuid> {
    definition
        .nodes
        .iter()
        .filter_map(|node| match &node.kind {
            NodeKind::SubWorkflow { workflow_id, .. } => Some(*workflow_id),
            NodeKind::Loop {
                body_workflow_id, ..
            } => Some(*body_workflow_id),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use serde_json::json;

    use crate::workflow::{
        engine::{run_workflow, NoCache, WfStatus},
        executor::IntelligenceSpec,
        types::{BudgetPolicy, Edge, ModelSelection, Node, NodeOutput},
    };

    struct RecordingExecutor {
        definitions: HashMap<Uuid, WorkflowDefinition>,
        loads: AtomicUsize,
        intelligence_calls: AtomicUsize,
    }

    #[async_trait]
    impl NodeExecutor for RecordingExecutor {
        async fn run_intelligence(
            &self,
            _node_id: &str,
            _spec: &IntelligenceSpec,
        ) -> Result<NodeOutput, ApiError> {
            self.intelligence_calls.fetch_add(1, Ordering::SeqCst);
            Ok(NodeOutput {
                content: json!("must-not-run"),
                cost_usd: 1.0,
                baseline_cost_usd: 1.0,
                model_used: Some("stub".into()),
                doc_vision_saved_est_usd: 0.0,
            })
        }

        async fn load_subworkflow(&self, id: Uuid) -> Result<WorkflowDefinition, ApiError> {
            self.loads.fetch_add(1, Ordering::SeqCst);
            self.definitions
                .get(&id)
                .cloned()
                .ok_or_else(|| ApiError::NotFound("nested workflow not found".into()))
        }
    }

    fn definition(id: Uuid, nodes: Vec<Node>, edges: Vec<Edge>) -> WorkflowDefinition {
        WorkflowDefinition {
            triggers: vec![],
            id,
            version: 1,
            name: format!("workflow-{id}"),
            nodes,
            edges,
            inputs: serde_json::Value::Null,
            budget: BudgetPolicy::default(),
            allowed_hosts: vec!["api.example.com".into()],
            metadata: serde_json::Value::Null,
        }
    }

    #[test]
    fn batch_query_is_latest_and_org_scoped() {
        assert!(GET_LATEST_DEFINITIONS_SQL.contains("WHERE org_id = $1"));
        assert!(GET_LATEST_DEFINITIONS_SQL.contains("id = ANY($2)"));
        assert!(GET_LATEST_DEFINITIONS_SQL.contains("DISTINCT ON (id)"));
        assert!(GET_LATEST_DEFINITIONS_SQL.contains("ORDER BY id, version DESC"));
    }

    #[tokio::test]
    async fn recursive_missing_secret_fails_before_parent_intelligence() {
        let root_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let grandchild_id = Uuid::new_v4();

        let grandchild = definition(
            grandchild_id,
            vec![
                Node {
                    id: "trigger".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "http".into(),
                    kind: NodeKind::Http {
                        method: "POST".into(),
                        url: "https://api.example.com/run".into(),
                        headers: vec![(
                            "authorization".into(),
                            "Bearer {{secrets.NESTED_API_KEY}}".into(),
                        )],
                        body: None,
                        max_response_bytes: None,
                    },
                },
            ],
            vec![Edge {
                from: "trigger".into(),
                to: "http".into(),
                map: None,
            }],
        );
        let child = definition(
            child_id,
            vec![
                Node {
                    id: "trigger".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "grandchild".into(),
                    kind: NodeKind::SubWorkflow {
                        workflow_id: grandchild_id,
                        version: None,
                    },
                },
            ],
            vec![Edge {
                from: "trigger".into(),
                to: "grandchild".into(),
                map: None,
            }],
        );
        let root = definition(
            root_id,
            vec![
                Node {
                    id: "trigger".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "parent_model".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "stub".into(),
                        },
                        prompt: "{{input}}".into(),
                        max_output_tokens: None,
                        max_cost_usd: None,
                    },
                },
                Node {
                    id: "child".into(),
                    kind: NodeKind::SubWorkflow {
                        workflow_id: child_id,
                        version: None,
                    },
                },
            ],
            vec![
                Edge {
                    from: "trigger".into(),
                    to: "parent_model".into(),
                    map: None,
                },
                Edge {
                    from: "parent_model".into(),
                    to: "child".into(),
                    map: None,
                },
            ],
        );
        let executor = RecordingExecutor {
            definitions: HashMap::from([(child_id, child), (grandchild_id, grandchild)]),
            loads: AtomicUsize::new(0),
            intelligence_calls: AtomicUsize::new(0),
        };

        let result = run_workflow(
            &executor,
            &root,
            &json!("input"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::Failed);
        assert_eq!(result.cost_usd, 0.0);
        assert_eq!(executor.intelligence_calls.load(Ordering::SeqCst), 0);
        assert_eq!(executor.loads.load(Ordering::SeqCst), 2);
        assert!(result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("NESTED_API_KEY")));
    }

    #[tokio::test]
    async fn prepared_child_definition_is_loaded_once_and_reused() {
        let root_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let child = definition(
            child_id,
            vec![
                Node {
                    id: "trigger".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "output".into(),
                    kind: NodeKind::Output,
                },
            ],
            vec![Edge {
                from: "trigger".into(),
                to: "output".into(),
                map: None,
            }],
        );
        let root = definition(
            root_id,
            vec![
                Node {
                    id: "trigger".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "child".into(),
                    kind: NodeKind::SubWorkflow {
                        workflow_id: child_id,
                        version: None,
                    },
                },
                Node {
                    id: "output".into(),
                    kind: NodeKind::Output,
                },
            ],
            vec![
                Edge {
                    from: "trigger".into(),
                    to: "child".into(),
                    map: None,
                },
                Edge {
                    from: "child".into(),
                    to: "output".into(),
                    map: None,
                },
            ],
        );
        let executor = RecordingExecutor {
            definitions: HashMap::from([(child_id, child)]),
            loads: AtomicUsize::new(0),
            intelligence_calls: AtomicUsize::new(0),
        };

        let result = run_workflow(
            &executor,
            &root,
            &json!("input"),
            None,
            |_| {},
            None,
            &HashMap::new(),
            0,
            &[],
            &NoCache,
        )
        .await;

        assert_eq!(result.status, WfStatus::Succeeded);
        assert_eq!(executor.loads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn nested_definition_fanout_is_bounded_before_loading() {
        let nodes = (0..=MAX_PREFLIGHT_WORKFLOW_DEFINITIONS)
            .map(|index| Node {
                id: format!("child-{index}"),
                kind: NodeKind::SubWorkflow {
                    workflow_id: Uuid::new_v4(),
                    version: None,
                },
            })
            .collect();
        let root = definition(Uuid::new_v4(), nodes, vec![]);
        let executor = RecordingExecutor {
            definitions: HashMap::new(),
            loads: AtomicUsize::new(0),
            intelligence_calls: AtomicUsize::new(0),
        };

        let error = match prepare_workflow_tree(&executor, &root, &HashMap::new(), 0, 5).await {
            Ok(_) => panic!("fan-out beyond the preparation bound must fail"),
            Err(error) => error,
        };

        assert!(error.contains("nested definition limit exceeded (256)"));
        assert_eq!(executor.loads.load(Ordering::SeqCst), 0);
    }
}
