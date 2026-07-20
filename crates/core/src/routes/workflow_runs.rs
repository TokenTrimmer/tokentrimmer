//! Durable workflow-run history and the read-only node journal.
//!
//! The node journal is intentionally an inspection surface, not replay:
//! labels and node kinds come from the exact immutable definition version the
//! run executed. New rows carry gateway node-envelope timing; legacy rows carry
//! only post-run persistence time. Neither form is provider-attempt timing.

use axum::{
    extract::{Path, State},
    http::{header::CACHE_CONTROL, HeaderValue},
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::Serialize;
use tt_auth::ApiKeyContext;
use uuid::Uuid;

use crate::{
    error::{ApiError, ApiResult},
    workflow::{
        node_run_store::{self, WorkflowNodeRunRecord},
        store::{self, WorkflowRunRecord},
        types::{NodeKind, WorkflowDefinition},
    },
    AppState, DOGFOOD_ORG_ID,
};

const NODE_JOURNAL_LIMIT: usize = 500;

fn require_org(ctx: Option<Extension<ApiKeyContext>>) -> Result<Uuid, ApiError> {
    match ctx {
        Some(Extension(context)) if context.org_id != DOGFOOD_ORG_ID => Ok(context.org_id),
        _ => Err(ApiError::Unauthorized),
    }
}

fn db_pool(state: &AppState) -> ApiResult<&sqlx::PgPool> {
    state.db_pool.as_ref().ok_or_else(|| {
        ApiError::ServiceUnavailable(
            "workflow storage requires a Postgres pool (none configured)".into(),
        )
    })
}

/// A durable workflow-run summary. The immutable `version` is the definition
/// version that executed, not the latest definition at read time.
#[derive(Debug, Serialize)]
pub struct WorkflowRunView {
    pub id: Uuid,
    pub workflow_id: Uuid,
    pub version: i32,
    pub workflow_environment: Option<&'static str>,
    pub release_revision: Option<i32>,
    pub variables_revision: Option<i32>,
    pub status: String,
    pub inputs: Option<serde_json::Value>,
    pub cost_usd: f64,
    pub max_cost_usd: Option<f64>,
    pub baseline_cost_usd: f64,
    pub saved_usd: f64,
    pub error: Option<String>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl From<WorkflowRunRecord> for WorkflowRunView {
    fn from(record: WorkflowRunRecord) -> Self {
        let release = record.release;
        Self {
            id: record.id,
            workflow_id: record.workflow_id,
            version: record.version,
            workflow_environment: release.map(|release| release.environment.as_str()),
            release_revision: release.map(|release| release.revision),
            variables_revision: release.map(|release| release.variables_revision),
            status: record.status,
            inputs: record.inputs,
            cost_usd: record.cost_usd,
            max_cost_usd: record.max_cost_usd,
            baseline_cost_usd: record.baseline_cost_usd,
            saved_usd: record.saved_usd,
            error: record.error,
            started_at: record.started_at,
            finished_at: record.finished_at,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ListWorkflowRunsResponse {
    pub object: &'static str,
    pub data: Vec<WorkflowRunView>,
}

/// One durable node-journal entry decorated from the exact executed
/// definition. `journal_index` is stable for repeated reads of the persisted
/// rows. New timing is the gateway workflow-node envelope, not provider-attempt
/// timing; legacy rows expose only their post-run persistence timestamp.
#[derive(Debug, Serialize)]
pub struct WorkflowNodeRunView {
    pub id: Uuid,
    pub journal_index: usize,
    pub node_id: String,
    pub definition_position: Option<usize>,
    pub node_type: Option<&'static str>,
    pub label: String,
    pub attempt: i32,
    pub status: String,
    pub output: Option<serde_json::Value>,
    pub cost_usd: f64,
    pub model_used: Option<String>,
    pub error: Option<String>,
    pub timing_source: &'static str,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub duration_ms: Option<i64>,
    pub legacy_recorded_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Serialize)]
pub struct WorkflowNodeRunsResponse {
    pub object: &'static str,
    pub run_id: Uuid,
    pub workflow_id: Uuid,
    pub workflow_version: i32,
    pub workflow_name: String,
    pub workflow_inputs_schema: serde_json::Value,
    pub truncated: bool,
    pub data: Vec<WorkflowNodeRunView>,
}

fn node_kind_name(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::Trigger => "trigger",
        NodeKind::Model { .. } => "model",
        NodeKind::Agent { .. } => "agent",
        NodeKind::Transform { .. } => "transform",
        NodeKind::Branch { .. } => "branch",
        NodeKind::Output => "output",
        NodeKind::Http { .. } => "http",
        NodeKind::SubWorkflow { .. } => "sub_workflow",
        NodeKind::Loop { .. } => "loop",
        NodeKind::Document { .. } => "document",
    }
}

fn node_descriptor(
    definition: &WorkflowDefinition,
    node_id: &str,
) -> (Option<usize>, Option<&'static str>, String) {
    let Some((index, node)) = definition
        .nodes
        .iter()
        .enumerate()
        .find(|(_, node)| node.id == node_id)
    else {
        return (None, None, format!("unmapped journal node · {node_id}"));
    };
    let position = index + 1;
    let kind = node_kind_name(&node.kind);
    (
        Some(position),
        Some(kind),
        format!("{position:02}. {kind} · {node_id}"),
    )
}

/// `GET /v1/workflows/:id/runs` — recent runs for exactly one org-owned
/// workflow. A foreign workflow id returns 404.
pub async fn list_workflow_runs(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<ListWorkflowRunsResponse>> {
    let org = require_org(ctx)?;
    let pool = db_pool(&state)?;
    if store::get_definition(pool, org, id).await.is_none() {
        return Err(ApiError::NotFound(format!("no workflow with id {id}")));
    }
    let data = store::list_workflow_runs(pool, org, id, 50)
        .await
        .into_iter()
        .map(WorkflowRunView::from)
        .collect();
    Ok(Json(ListWorkflowRunsResponse {
        object: "list",
        data,
    }))
}

/// `GET /v1/workflows/runs/:run_id` — one org-scoped durable run.
pub async fn get_workflow_run(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(run_id): Path<Uuid>,
) -> ApiResult<Json<WorkflowRunView>> {
    let org = require_org(ctx)?;
    let pool = db_pool(&state)?;
    let run = store::get_run(pool, run_id, org)
        .await
        .ok_or_else(|| ApiError::NotFound(format!("no workflow run with id {run_id}")))?;
    Ok(Json(WorkflowRunView::from(run)))
}

/// `GET /v1/workflows/runs/:run_id/nodes` — a bounded node journal decorated
/// with the exact immutable definition version that executed.
pub async fn list_workflow_node_runs(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(run_id): Path<Uuid>,
) -> ApiResult<Response> {
    let org = require_org(ctx)?;
    let pool = db_pool(&state)?;
    let run = store::get_run_strict(pool, run_id, org)
        .await
        .map_err(|error| {
            tracing::warn!(%run_id, %error, "workflow run node-journal owner lookup failed");
            ApiError::ServiceUnavailable("workflow run journal is temporarily unavailable".into())
        })?
        .ok_or_else(|| ApiError::NotFound(format!("no workflow run with id {run_id}")))?;
    let (definition, version) =
        store::get_definition_version(pool, org, run.workflow_id, run.version)
            .await
            .ok_or_else(|| {
                ApiError::Internal(
                    "workflow run references an unavailable immutable definition".into(),
                )
            })?;

    let mut records =
        node_run_store::list_node_runs_for_run(pool, run_id, org, (NODE_JOURNAL_LIMIT + 1) as i64)
            .await
            .map_err(|error| {
                tracing::warn!(%run_id, %error, "workflow node-journal read failed");
                ApiError::ServiceUnavailable(
                    "workflow run journal is temporarily unavailable".into(),
                )
            })?;
    let truncated = records.len() > NODE_JOURNAL_LIMIT;
    records.truncate(NODE_JOURNAL_LIMIT);

    let data = records
        .into_iter()
        .enumerate()
        .map(|(index, record)| node_run_view(index + 1, record, &definition))
        .collect();

    let mut response = Json(WorkflowNodeRunsResponse {
        object: "list",
        run_id,
        workflow_id: run.workflow_id,
        workflow_version: version,
        workflow_name: definition.name,
        workflow_inputs_schema: definition.inputs,
        truncated,
        data,
    })
    .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    Ok(response)
}

fn node_run_view(
    journal_index: usize,
    record: WorkflowNodeRunRecord,
    definition: &WorkflowDefinition,
) -> WorkflowNodeRunView {
    let (definition_position, node_type, label) = node_descriptor(definition, &record.node_id);
    let (timing_source, started_at, finished_at, duration_ms, legacy_recorded_at) =
        match record.finished_at {
            Some(finished_at) => {
                let duration_ms = finished_at
                    .signed_duration_since(record.started_at)
                    .num_milliseconds()
                    .max(0);
                (
                    "gateway_node_envelope",
                    Some(record.started_at),
                    Some(finished_at),
                    Some(duration_ms),
                    None,
                )
            }
            None => (
                "legacy_post_run_persistence",
                None,
                None,
                None,
                Some(record.started_at),
            ),
        };
    WorkflowNodeRunView {
        id: record.id,
        journal_index,
        node_id: record.node_id,
        definition_position,
        node_type,
        label,
        attempt: record.attempt,
        status: record.status,
        output: record.output,
        cost_usd: record.cost_usd,
        model_used: record.model_used,
        error: record.error,
        timing_source,
        started_at,
        finished_at,
        duration_ms,
        legacy_recorded_at,
    }
}

#[cfg(test)]
mod tests {
    use super::{node_descriptor, node_run_view, WorkflowRunView};
    use crate::workflow::node_run_store::WorkflowNodeRunRecord;
    use crate::workflow::release_store::WorkflowEnvironment;
    use crate::workflow::store::{WorkflowRunRecord, WorkflowRunReleaseProvenance};
    use crate::workflow::types::{
        BudgetPolicy, ModelSelection, Node, NodeKind, WorkflowDefinition,
    };
    use chrono::{Duration, Utc};
    use uuid::Uuid;

    fn definition() -> WorkflowDefinition {
        WorkflowDefinition {
            id: Uuid::nil(),
            version: 7,
            name: "debug fixture".into(),
            nodes: vec![
                Node {
                    id: "start".into(),
                    kind: NodeKind::Trigger,
                },
                Node {
                    id: "answer".into(),
                    kind: NodeKind::Model {
                        selection: ModelSelection::Model {
                            model: "gpt-4o-mini".into(),
                        },
                        prompt: "{{input}}".into(),
                        max_output_tokens: Some(64),
                        max_cost_usd: None,
                    },
                },
            ],
            edges: vec![],
            inputs: serde_json::json!({"prompt": {"type": "string"}}),
            budget: BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
            triggers: vec![],
        }
    }

    #[test]
    fn labels_are_stable_and_derive_from_immutable_definition_order() {
        let definition = definition();
        assert_eq!(
            node_descriptor(&definition, "start"),
            (Some(1), Some("trigger"), "01. trigger · start".into())
        );
        assert_eq!(
            node_descriptor(&definition, "answer"),
            (Some(2), Some("model"), "02. model · answer".into())
        );
        assert_eq!(
            node_descriptor(&definition, "child-node"),
            (None, None, "unmapped journal node · child-node".into())
        );
    }

    #[test]
    fn timing_distinguishes_engine_envelopes_from_legacy_persistence() {
        let definition = definition();
        let started_at = Utc::now();
        let record = WorkflowNodeRunRecord {
            id: Uuid::new_v4(),
            node_id: "answer".into(),
            attempt: 1,
            status: "completed".into(),
            output: Some(serde_json::json!({"text": "ok"})),
            cost_usd: 0.01,
            model_used: Some("gpt-4o-mini".into()),
            error: None,
            started_at,
            finished_at: Some(started_at + Duration::milliseconds(125)),
        };

        let timed = node_run_view(1, record.clone(), &definition);
        assert_eq!(timed.timing_source, "gateway_node_envelope");
        assert_eq!(timed.started_at, Some(started_at));
        assert_eq!(timed.duration_ms, Some(125));
        assert_eq!(timed.legacy_recorded_at, None);

        let legacy = node_run_view(
            1,
            WorkflowNodeRunRecord {
                finished_at: None,
                ..record
            },
            &definition,
        );
        assert_eq!(legacy.timing_source, "legacy_post_run_persistence");
        assert_eq!(legacy.started_at, None);
        assert_eq!(legacy.finished_at, None);
        assert_eq!(legacy.duration_ms, None);
        assert_eq!(legacy.legacy_recorded_at, Some(started_at));
    }

    #[test]
    fn run_view_exposes_exact_optional_release_provenance() {
        let record = WorkflowRunRecord {
            id: Uuid::new_v4(),
            workflow_id: Uuid::new_v4(),
            version: 7,
            org_id: Uuid::new_v4(),
            release: Some(WorkflowRunReleaseProvenance {
                environment: WorkflowEnvironment::Production,
                revision: 3,
                variables_revision: 2,
            }),
            status: "completed".into(),
            inputs: Some(serde_json::json!({})),
            cost_usd: 0.01,
            max_cost_usd: None,
            baseline_cost_usd: 0.02,
            saved_usd: 0.01,
            error: None,
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
        };
        let view = WorkflowRunView::from(record);
        assert_eq!(view.version, 7);
        assert_eq!(view.workflow_environment, Some("production"));
        assert_eq!(view.release_revision, Some(3));
        assert_eq!(view.variables_revision, Some(2));
    }
}
