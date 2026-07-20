//! Read-only immutable workflow-definition version history.
//!
//! These endpoints are an authoring/diff foundation only. They deliberately do
//! not create draft/published state, approve or promote a version, or mutate a
//! rollback. Every lookup is scoped to the authenticated org and every success
//! response is `private, no-store`.

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
    let record = store::get_definition_version_record(pool, org, id, version)
        .await
        .map_err(|error| {
            tracing::error!(
                %org,
                workflow_id = %id,
                workflow_version = version,
                %error,
                "workflow version GET failed"
            );
            ApiError::Internal("failed to read workflow definition version".into())
        })?
        .ok_or_else(|| {
            ApiError::NotFound(format!("no workflow with id {id} at version {version}"))
        })?;
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
}
