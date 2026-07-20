//! Read-only immutable workflow-definition version history.
//!
//! These endpoints are an authoring/diff foundation only. They deliberately do
//! not create draft/published state, approve or promote a version, or mutate a
//! rollback. Every lookup is scoped to the authenticated org and every success
//! response is `private, no-store`.

use std::collections::BTreeSet;

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
    workflow::{self, store},
    AppState, DOGFOOD_ORG_ID,
};

const MAX_WORKFLOW_DEFINITION_VERSIONS: usize = 100;
const MAX_WORKFLOW_DEFINITION_DIFF_ENTRIES: usize = 256;
const MAX_WORKFLOW_DEFINITION_DIFF_DEPTH: usize = 64;

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

fn private_no_store_json<T: Serialize>(value: T) -> Response {
    let mut response = Json(value).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response
}

#[derive(Debug, Serialize)]
pub struct ListWorkflowVersionsResponse {
    pub object: &'static str,
    pub workflow_id: Uuid,
    pub data: Vec<WorkflowVersionMetaView>,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct WorkflowVersionMetaView {
    pub version: i32,
    pub content_hash: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize)]
pub struct WorkflowVersionResponse {
    pub object: &'static str,
    pub workflow_id: Uuid,
    pub version: i32,
    pub content_hash: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub definition: workflow::WorkflowDefinition,
}

#[derive(Debug, Serialize)]
pub struct WorkflowVersionDiffResponse {
    pub object: &'static str,
    pub workflow_id: Uuid,
    pub from: WorkflowVersionMetaView,
    pub to: WorkflowVersionMetaView,
    pub changed: bool,
    pub data: Vec<WorkflowVersionDiffEntry>,
    pub truncated: bool,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct WorkflowVersionDiffEntry {
    /// RFC 6901 JSON Pointer into the authored definition. The empty string is
    /// the root. Values are deliberately omitted from this response.
    pub path: String,
    pub kind: WorkflowVersionDiffKind,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowVersionDiffKind {
    Added,
    Removed,
    Modified,
}

#[derive(Debug)]
pub(crate) struct BoundedWorkflowVersionDiff {
    pub(crate) data: Vec<WorkflowVersionDiffEntry>,
    pub(crate) truncated: bool,
}

impl BoundedWorkflowVersionDiff {
    fn push(&mut self, path: String, kind: WorkflowVersionDiffKind) {
        if self.data.len() < MAX_WORKFLOW_DEFINITION_DIFF_ENTRIES {
            self.data.push(WorkflowVersionDiffEntry { path, kind });
        } else {
            self.truncated = true;
        }
    }
}

fn json_pointer_path(parent: &str, token: &str) -> String {
    let escaped = token.replace('~', "~0").replace('/', "~1");
    format!("{parent}/{escaped}")
}

fn collect_json_diff(
    from: &serde_json::Value,
    to: &serde_json::Value,
    path: &str,
    depth: usize,
    diff: &mut BoundedWorkflowVersionDiff,
) {
    if from == to || diff.truncated {
        return;
    }
    if depth >= MAX_WORKFLOW_DEFINITION_DIFF_DEPTH {
        diff.push(path.to_owned(), WorkflowVersionDiffKind::Modified);
        return;
    }

    match (from, to) {
        (serde_json::Value::Object(from), serde_json::Value::Object(to)) => {
            let keys = from
                .keys()
                .chain(to.keys())
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            for key in keys {
                let child_path = json_pointer_path(path, key);
                match (from.get(key), to.get(key)) {
                    (None, Some(_)) => diff.push(child_path, WorkflowVersionDiffKind::Added),
                    (Some(_), None) => diff.push(child_path, WorkflowVersionDiffKind::Removed),
                    (Some(from), Some(to)) => {
                        collect_json_diff(from, to, &child_path, depth + 1, diff);
                    }
                    (None, None) => unreachable!("key came from one of the compared objects"),
                }
                if diff.truncated {
                    break;
                }
            }
        }
        (serde_json::Value::Array(from), serde_json::Value::Array(to)) => {
            for index in 0..from.len().max(to.len()) {
                let child_path = json_pointer_path(path, &index.to_string());
                match (from.get(index), to.get(index)) {
                    (None, Some(_)) => diff.push(child_path, WorkflowVersionDiffKind::Added),
                    (Some(_), None) => diff.push(child_path, WorkflowVersionDiffKind::Removed),
                    (Some(from), Some(to)) => {
                        collect_json_diff(from, to, &child_path, depth + 1, diff);
                    }
                    (None, None) => unreachable!("index is below one compared array length"),
                }
                if diff.truncated {
                    break;
                }
            }
        }
        _ => diff.push(path.to_owned(), WorkflowVersionDiffKind::Modified),
    }
}

fn authored_definition_value(
    definition: &workflow::WorkflowDefinition,
) -> Result<serde_json::Value, serde_json::Error> {
    let mut value = serde_json::to_value(definition)?;
    if let serde_json::Value::Object(object) = &mut value {
        // The database row is authoritative for this server-owned field. A
        // comparison should describe authored changes, not its own endpoints.
        object.remove("version");
    }
    Ok(value)
}

pub(crate) fn diff_version_records(
    from: &store::WorkflowDefinitionVersionRecord,
    to: &store::WorkflowDefinitionVersionRecord,
) -> Result<BoundedWorkflowVersionDiff, serde_json::Error> {
    let from_value = authored_definition_value(&from.definition)?;
    let to_value = authored_definition_value(&to.definition)?;
    let mut diff = BoundedWorkflowVersionDiff {
        data: Vec::new(),
        truncated: false,
    };
    collect_json_diff(&from_value, &to_value, "", 0, &mut diff);
    Ok(diff)
}

fn version_meta(record: &store::WorkflowDefinitionVersionRecord) -> WorkflowVersionMetaView {
    WorkflowVersionMetaView {
        version: record.version,
        content_hash: record.content_hash.clone(),
        created_at: record.created_at,
    }
}

async fn load_version_record(
    pool: &sqlx::PgPool,
    org: Uuid,
    id: Uuid,
    version: i32,
) -> ApiResult<store::WorkflowDefinitionVersionRecord> {
    store::get_definition_version_record(pool, org, id, version)
        .await
        .map_err(|error| {
            tracing::error!(
                %org,
                workflow_id = %id,
                workflow_version = version,
                %error,
                "workflow version read failed"
            );
            ApiError::Internal("failed to read workflow definition version".into())
        })?
        .ok_or_else(|| ApiError::NotFound(format!("no workflow with id {id} at version {version}")))
}

fn bound_workflow_version_rows(
    mut rows: Vec<store::WorkflowDefinitionVersionMeta>,
) -> (Vec<store::WorkflowDefinitionVersionMeta>, bool) {
    let truncated = rows.len() > MAX_WORKFLOW_DEFINITION_VERSIONS;
    rows.truncate(MAX_WORKFLOW_DEFINITION_VERSIONS);
    (rows, truncated)
}

/// Return newest-first immutable version metadata for one org-owned workflow.
/// The extra fetched row is used only to report truncation; at most 100 rows
/// reach the caller.
pub async fn list_versions(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Response> {
    let org = require_org(ctx)?;
    let pool = db_pool(&state)?;
    let rows = store::list_definition_versions(
        pool,
        org,
        id,
        (MAX_WORKFLOW_DEFINITION_VERSIONS + 1) as i64,
    )
    .await
    .map_err(|error| {
        tracing::error!(%org, workflow_id = %id, %error, "workflow version LIST failed");
        ApiError::Internal("failed to list workflow definition versions".into())
    })?;
    if rows.is_empty() {
        return Err(ApiError::NotFound(format!("no workflow with id {id}")));
    }
    let (rows, truncated) = bound_workflow_version_rows(rows);
    let data = rows
        .into_iter()
        .map(|row| WorkflowVersionMetaView {
            version: row.version,
            content_hash: row.content_hash,
            created_at: row.created_at,
        })
        .collect();
    Ok(private_no_store_json(ListWorkflowVersionsResponse {
        object: "list",
        workflow_id: id,
        data,
        truncated,
    }))
}

/// Return one exact retained definition and its authoritative storage metadata.
pub async fn get_version(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path((id, version)): Path<(Uuid, i32)>,
) -> ApiResult<Response> {
    let org = require_org(ctx)?;
    if version <= 0 {
        return Err(ApiError::InvalidRequest(
            "workflow version must be a positive integer".into(),
        ));
    }
    let pool = db_pool(&state)?;
    let record = load_version_record(pool, org, id, version).await?;
    let mut definition = record.definition;
    definition.version = record.version as u32;
    Ok(private_no_store_json(WorkflowVersionResponse {
        object: "workflow_definition_version",
        workflow_id: id,
        version: record.version,
        content_hash: record.content_hash,
        created_at: record.created_at,
        definition,
    }))
}

/// Compare two exact immutable definitions without returning either side's
/// values. Paths are deterministic RFC 6901 pointers and bounded so a large
/// metadata/input shape cannot create an unbounded response.
pub async fn compare_versions(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path((id, from_version, to_version)): Path<(Uuid, i32, i32)>,
) -> ApiResult<Response> {
    let org = require_org(ctx)?;
    if from_version <= 0 || to_version <= 0 {
        return Err(ApiError::InvalidRequest(
            "workflow comparison versions must be positive integers".into(),
        ));
    }
    let pool = db_pool(&state)?;
    let from = load_version_record(pool, org, id, from_version).await?;
    let to = if from_version == to_version {
        from.clone()
    } else {
        load_version_record(pool, org, id, to_version).await?
    };
    let diff = diff_version_records(&from, &to).map_err(|error| {
        tracing::error!(
            %org,
            workflow_id = %id,
            from_version,
            to_version,
            %error,
            "workflow version comparison serialization failed"
        );
        ApiError::Internal("failed to compare workflow definition versions".into())
    })?;
    let changed = !diff.data.is_empty();
    Ok(private_no_store_json(WorkflowVersionDiffResponse {
        object: "workflow_definition_version_diff",
        workflow_id: id,
        from: version_meta(&from),
        to: version_meta(&to),
        changed,
        data: diff.data,
        truncated: diff.truncated,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ProviderRegistry;

    fn test_state() -> AppState {
        AppState::new(ProviderRegistry::new())
    }

    fn real_org_ctx(org: Uuid) -> Option<Extension<ApiKeyContext>> {
        Some(Extension(ApiKeyContext {
            key_id: Uuid::new_v4(),
            org_id: org,
            tier: None,
            skip_shadow: false,
        }))
    }

    #[tokio::test]
    async fn list_versions_anon_returns_unauthorized() {
        let result = list_versions(State(test_state()), None, Path(Uuid::nil())).await;
        assert!(
            matches!(result, Err(ApiError::Unauthorized)),
            "expected Unauthorized, got {result:?}"
        );
    }

    #[tokio::test]
    async fn get_version_anon_returns_unauthorized() {
        let result = get_version(State(test_state()), None, Path((Uuid::nil(), 1))).await;
        assert!(
            matches!(result, Err(ApiError::Unauthorized)),
            "expected Unauthorized, got {result:?}"
        );
    }

    #[tokio::test]
    async fn get_version_rejects_non_positive_version_before_db_lookup() {
        let result = get_version(
            State(test_state()),
            real_org_ctx(Uuid::new_v4()),
            Path((Uuid::new_v4(), 0)),
        )
        .await;
        assert!(
            matches!(result, Err(ApiError::InvalidRequest(_))),
            "expected InvalidRequest, got {result:?}"
        );
    }

    #[tokio::test]
    async fn compare_versions_anon_returns_unauthorized() {
        let result = compare_versions(State(test_state()), None, Path((Uuid::nil(), 1, 2))).await;
        assert!(
            matches!(result, Err(ApiError::Unauthorized)),
            "expected Unauthorized, got {result:?}"
        );
    }

    #[tokio::test]
    async fn compare_versions_rejects_non_positive_versions_before_db_lookup() {
        for versions in [(0, 1), (1, 0), (-1, 2), (2, -1)] {
            let result = compare_versions(
                State(test_state()),
                real_org_ctx(Uuid::new_v4()),
                Path((Uuid::new_v4(), versions.0, versions.1)),
            )
            .await;
            assert!(
                matches!(result, Err(ApiError::InvalidRequest(_))),
                "expected InvalidRequest for {versions:?}, got {result:?}"
            );
        }
    }

    #[test]
    fn workflow_version_history_is_capped_and_reports_truncation() {
        let rows = (1..=(MAX_WORKFLOW_DEFINITION_VERSIONS + 1))
            .rev()
            .map(|version| store::WorkflowDefinitionVersionMeta {
                version: version as i32,
                content_hash: format!("hash-{version}"),
                created_at: chrono::DateTime::UNIX_EPOCH,
            })
            .collect();
        let (rows, truncated) = bound_workflow_version_rows(rows);
        assert!(truncated);
        assert_eq!(rows.len(), MAX_WORKFLOW_DEFINITION_VERSIONS);
        assert_eq!(rows.first().map(|row| row.version), Some(101));
        assert_eq!(rows.last().map(|row| row.version), Some(2));
    }

    #[test]
    fn workflow_version_reads_are_private_no_store() {
        let response = private_no_store_json(serde_json::json!({"ok": true}));
        assert_eq!(
            response.headers().get(CACHE_CONTROL),
            Some(&HeaderValue::from_static("private, no-store"))
        );
    }

    #[test]
    fn workflow_version_diff_is_deterministic_value_free_and_pointer_escaped() {
        let from = serde_json::json!({
            "removed": true,
            "same": "kept",
            "a/b~c": {"value": 1},
        });
        let to = serde_json::json!({
            "added": true,
            "same": "kept",
            "a/b~c": {"value": 2},
        });
        let mut diff = BoundedWorkflowVersionDiff {
            data: Vec::new(),
            truncated: false,
        };
        collect_json_diff(&from, &to, "", 0, &mut diff);
        assert_eq!(
            diff.data,
            vec![
                WorkflowVersionDiffEntry {
                    path: "/a~1b~0c/value".into(),
                    kind: WorkflowVersionDiffKind::Modified,
                },
                WorkflowVersionDiffEntry {
                    path: "/added".into(),
                    kind: WorkflowVersionDiffKind::Added,
                },
                WorkflowVersionDiffEntry {
                    path: "/removed".into(),
                    kind: WorkflowVersionDiffKind::Removed,
                },
            ]
        );
        assert!(!diff.truncated);
        let wire = serde_json::to_value(&diff.data).expect("serialize value-free diff");
        assert_eq!(
            wire[0],
            serde_json::json!({"path": "/a~1b~0c/value", "kind": "modified"})
        );
    }

    #[test]
    fn workflow_version_diff_is_bounded_and_reports_truncation() {
        let from = serde_json::Value::Array(
            (0..=MAX_WORKFLOW_DEFINITION_DIFF_ENTRIES)
                .map(|index| serde_json::json!(index))
                .collect(),
        );
        let to = serde_json::Value::Array(
            (0..=MAX_WORKFLOW_DEFINITION_DIFF_ENTRIES)
                .map(|_| serde_json::Value::Null)
                .collect(),
        );
        let mut diff = BoundedWorkflowVersionDiff {
            data: Vec::new(),
            truncated: false,
        };
        collect_json_diff(&from, &to, "", 0, &mut diff);
        assert_eq!(diff.data.len(), MAX_WORKFLOW_DEFINITION_DIFF_ENTRIES);
        assert!(diff.truncated);
    }

    #[test]
    fn workflow_version_diff_ignores_server_owned_version() {
        let definition = |version| workflow::WorkflowDefinition {
            id: Uuid::nil(),
            version,
            name: "same".into(),
            nodes: vec![],
            edges: vec![],
            inputs: serde_json::Value::Null,
            budget: workflow::types::BudgetPolicy::default(),
            allowed_hosts: vec![],
            metadata: serde_json::Value::Null,
            triggers: vec![],
        };
        let record = |version| store::WorkflowDefinitionVersionRecord {
            definition: definition(version as u32),
            version,
            content_hash: format!("hash-{version}"),
            created_at: chrono::DateTime::UNIX_EPOCH,
        };
        let diff = diff_version_records(&record(1), &record(2)).expect("compare definitions");
        assert!(diff.data.is_empty());
        assert!(!diff.truncated);
    }
}
