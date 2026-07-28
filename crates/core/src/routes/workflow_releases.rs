//! Explicit workflow draft/release-state transitions.
//!
//! Immutable definition saves remain drafts until development is published.
//! Staging can only copy the exact current development release, and production
//! can only copy the exact current staging release. Rollback can only restore a
//! version previously released in that same environment. Every transition is
//! optimistic and appends immutable metadata in the same SQL statement as its
//! current-pointer update.
//!
//! This module does not claim human approval or silently change the legacy
//! latest-version execution contract. Responses are value-free and
//! `private, no-store`.

use axum::{
    extract::{Path, State},
    http::{header::CACHE_CONTROL, HeaderValue},
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use tt_auth::ApiKeyContext;
use uuid::Uuid;

use crate::{
    error::{ApiError, ApiResult},
    workflow::{release_store, store},
    AppState, DOGFOOD_ORG_ID,
};

const MAX_WORKFLOW_RELEASE_HISTORY: usize = 100;

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PublishWorkflowRequest {
    pub workflow_version: u32,
    pub expected_release_revision: u32,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PromoteWorkflowRequest {
    pub expected_source_revision: u32,
    pub expected_release_revision: u32,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RollbackWorkflowRequest {
    pub release_revision: u32,
    pub expected_release_revision: u32,
}

#[derive(Debug, Serialize)]
pub struct WorkflowReleaseStateResponse {
    pub object: &'static str,
    pub workflow_id: Uuid,
    pub latest: WorkflowReleaseVersionView,
    pub data: Vec<WorkflowEnvironmentReleaseView>,
}

#[derive(Debug, Serialize)]
pub struct WorkflowReleaseResponse {
    pub object: &'static str,
    pub workflow_id: Uuid,
    pub release: WorkflowEnvironmentReleaseView,
}

#[derive(Debug, Serialize)]
pub struct WorkflowReleaseHistoryResponse {
    pub object: &'static str,
    pub workflow_id: Uuid,
    pub environment: &'static str,
    pub data: Vec<WorkflowEnvironmentReleaseView>,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct WorkflowReleaseVersionView {
    pub version: i32,
    pub content_hash: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize)]
pub struct WorkflowEnvironmentReleaseView {
    pub environment: &'static str,
    pub revision: i32,
    pub workflow_version: i32,
    pub content_hash: String,
    pub action: &'static str,
    pub source_environment: Option<&'static str>,
    pub source_revision: Option<i32>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

fn require_org(ctx: Option<Extension<ApiKeyContext>>) -> Result<Uuid, ApiError> {
    match ctx {
        Some(Extension(context)) if context.org_id != DOGFOOD_ORG_ID => Ok(context.org_id),
        _ => Err(ApiError::Unauthorized),
    }
}

fn db_pool(state: &AppState) -> ApiResult<&sqlx::PgPool> {
    state.db_pool.as_ref().ok_or_else(|| {
        ApiError::ServiceUnavailable(
            "workflow release storage requires a Postgres pool (none configured)".into(),
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

fn persisted_revision(value: u32, field: &str, allow_zero: bool) -> ApiResult<i32> {
    if !allow_zero && value == 0 {
        return Err(ApiError::InvalidRequest(format!(
            "{field} must be a positive integer"
        )));
    }
    i32::try_from(value)
        .map_err(|_| ApiError::InvalidRequest(format!("{field} must be at most {}", i32::MAX)))
}

fn environment_from_path(value: &str) -> ApiResult<release_store::WorkflowEnvironment> {
    release_store::WorkflowEnvironment::parse(value).map_err(|_| {
        ApiError::InvalidRequest(
            "workflow environment must be development, staging, or production".into(),
        )
    })
}

fn expected_release_revision(value: u32, allow_zero: bool) -> ApiResult<i32> {
    let revision = persisted_revision(value, "expected_release_revision", allow_zero)?;
    if revision == i32::MAX {
        return Err(ApiError::InvalidRequest(format!(
            "expected_release_revision must be at most {}",
            i32::MAX - 1
        )));
    }
    Ok(revision)
}

fn release_conflict(id: Uuid) -> ApiError {
    ApiError::Conflict(format!(
        "workflow {id} release state changed or the requested transition is no longer valid; reload before retrying"
    ))
}

fn release_view(
    release: release_store::WorkflowEnvironmentRelease,
) -> WorkflowEnvironmentReleaseView {
    WorkflowEnvironmentReleaseView {
        environment: release.environment.as_str(),
        revision: release.revision,
        workflow_version: release.workflow_version,
        content_hash: release.content_hash,
        action: release.action.as_str(),
        source_environment: release.source_environment.map(|value| value.as_str()),
        source_revision: release.source_revision,
        created_at: release.created_at,
    }
}

fn mutation_response(id: Uuid, mutation: release_store::WorkflowReleaseMutation) -> Response {
    private_no_store_json(WorkflowReleaseResponse {
        object: "workflow_environment_release",
        workflow_id: id,
        release: WorkflowEnvironmentReleaseView {
            environment: mutation.environment.as_str(),
            revision: mutation.revision,
            workflow_version: mutation.workflow_version,
            content_hash: mutation.content_hash,
            action: mutation.action.as_str(),
            source_environment: mutation.source_environment.map(|value| value.as_str()),
            source_revision: mutation.source_revision,
            created_at: mutation.created_at,
        },
    })
}

/// Return the latest immutable definition plus the current release pointer for
/// each environment that has been explicitly released.
pub async fn get_release_state(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Response> {
    let org = require_org(ctx)?;
    let pool = db_pool(&state)?;
    let latest = store::list_definition_versions(pool, org, id, 1)
        .await
        .map_err(|error| {
            tracing::error!(%org, workflow_id = %id, %error, "workflow release-state definition read failed");
            ApiError::Internal("failed to read workflow release state".into())
        })?
        .into_iter()
        .next()
        .ok_or_else(|| ApiError::NotFound(format!("no workflow with id {id}")))?;
    let current = release_store::list_current_releases(pool, org, id)
        .await
        .map_err(|error| {
            tracing::error!(%org, workflow_id = %id, %error, "workflow release-state read failed");
            ApiError::Internal("failed to read workflow release state".into())
        })?;

    let mut data = Vec::with_capacity(current.len());
    for environment in release_store::WorkflowEnvironment::ALL {
        if let Some(release) = current
            .iter()
            .find(|release| release.environment == environment)
            .cloned()
        {
            data.push(release_view(release));
        }
    }
    Ok(private_no_store_json(WorkflowReleaseStateResponse {
        object: "workflow_release_state",
        workflow_id: id,
        latest: WorkflowReleaseVersionView {
            version: latest.version,
            content_hash: latest.content_hash,
            created_at: latest.created_at,
        },
        data,
    }))
}

/// Return newest-first immutable release metadata for one environment. One
/// sentinel row is fetched only to report truncation.
pub async fn list_release_history(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path((id, environment)): Path<(Uuid, String)>,
) -> ApiResult<Response> {
    let org = require_org(ctx)?;
    let environment = environment_from_path(&environment)?;
    let pool = db_pool(&state)?;
    if store::list_definition_versions(pool, org, id, 1)
        .await
        .map_err(|error| {
            tracing::error!(%org, workflow_id = %id, %error, "workflow release-history definition read failed");
            ApiError::Internal("failed to read workflow release history".into())
        })?
        .is_empty()
    {
        return Err(ApiError::NotFound(format!("no workflow with id {id}")));
    }
    let mut releases = release_store::list_release_history(
        pool,
        org,
        id,
        environment,
        (MAX_WORKFLOW_RELEASE_HISTORY + 1) as i64,
    )
    .await
    .map_err(|error| {
        tracing::error!(%org, workflow_id = %id, environment = environment.as_str(), %error, "workflow release-history read failed");
        ApiError::Internal("failed to read workflow release history".into())
    })?;
    let truncated = releases.len() > MAX_WORKFLOW_RELEASE_HISTORY;
    releases.truncate(MAX_WORKFLOW_RELEASE_HISTORY);
    Ok(private_no_store_json(WorkflowReleaseHistoryResponse {
        object: "workflow_environment_release_list",
        workflow_id: id,
        environment: environment.as_str(),
        data: releases.into_iter().map(release_view).collect(),
        truncated,
    }))
}

/// Publish one exact retained version to development.
pub async fn publish_development(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path(id): Path<Uuid>,
    Json(body): Json<PublishWorkflowRequest>,
) -> ApiResult<Response> {
    let org = require_org(ctx)?;
    let version = persisted_revision(body.workflow_version, "workflow_version", false)?;
    let expected = expected_release_revision(body.expected_release_revision, true)?;
    let pool = db_pool(&state)?;
    store::get_definition_version_record(pool, org, id, version)
        .await
        .map_err(|error| {
            tracing::error!(%org, workflow_id = %id, workflow_version = version, %error, "publish target read failed");
            ApiError::Internal("failed to read workflow publish target".into())
        })?
        .ok_or_else(|| ApiError::NotFound(format!("no workflow with id {id} at version {version}")))?;
    let mutation = release_store::publish_development(pool, org, id, version, expected)
        .await
        .map_err(|error| {
            tracing::error!(%org, workflow_id = %id, workflow_version = version, %error, "development publish failed");
            ApiError::Internal("failed to publish workflow to development".into())
        })?
        .ok_or_else(|| release_conflict(id))?;
    Ok(mutation_response(id, mutation))
}

/// Promote the exact current lower-environment release into staging or
/// production. Both source and target revisions are optimistic preconditions.
pub async fn promote_environment(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path((id, environment)): Path<(Uuid, String)>,
    Json(body): Json<PromoteWorkflowRequest>,
) -> ApiResult<Response> {
    let org = require_org(ctx)?;
    let target = environment_from_path(&environment)?;
    let source = target.promotion_source().ok_or_else(|| {
        ApiError::InvalidRequest(
            "development must be published with the development publish endpoint".into(),
        )
    })?;
    let expected_source = persisted_revision(
        body.expected_source_revision,
        "expected_source_revision",
        false,
    )?;
    let expected_target = expected_release_revision(body.expected_release_revision, true)?;
    let pool = db_pool(&state)?;
    let mutation = release_store::promote_environment(
        pool,
        org,
        id,
        source,
        target,
        expected_target,
        expected_source,
    )
    .await
    .map_err(|error| {
        tracing::error!(%org, workflow_id = %id, environment = target.as_str(), %error, "workflow promotion failed");
        ApiError::Internal("failed to promote workflow environment".into())
    })?
    .ok_or_else(|| release_conflict(id))?;
    Ok(mutation_response(id, mutation))
}

/// Restore an exact version from one environment's own immutable release
/// history. The selected historical revision is recorded on the new row.
pub async fn rollback_environment(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path((id, environment)): Path<(Uuid, String)>,
    Json(body): Json<RollbackWorkflowRequest>,
) -> ApiResult<Response> {
    let org = require_org(ctx)?;
    let environment = environment_from_path(&environment)?;
    let release_revision = persisted_revision(body.release_revision, "release_revision", false)?;
    let expected = expected_release_revision(body.expected_release_revision, false)?;
    let pool = db_pool(&state)?;
    let mutation = release_store::rollback_environment(
        pool,
        org,
        id,
        environment,
        release_revision,
        expected,
    )
    .await
    .map_err(|error| {
        tracing::error!(%org, workflow_id = %id, environment = environment.as_str(), %error, "workflow rollback failed");
        ApiError::Internal("failed to roll back workflow environment".into())
    })?
    .ok_or_else(|| release_conflict(id))?;
    Ok(mutation_response(id, mutation))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ProviderRegistry;

    fn test_state() -> AppState {
        AppState::new(ProviderRegistry::new())
    }

    fn real_org_ctx() -> Option<Extension<ApiKeyContext>> {
        Some(Extension(ApiKeyContext {
            key_id: Uuid::new_v4(),
            org_id: Uuid::new_v4(),
            tier: None,
            skip_shadow: false,
        }))
    }

    #[tokio::test]
    async fn release_state_requires_auth_before_storage() {
        let result = get_release_state(State(test_state()), None, Path(Uuid::nil())).await;
        assert!(matches!(result, Err(ApiError::Unauthorized)));
    }

    #[tokio::test]
    async fn publish_rejects_invalid_version_before_storage() {
        let result = publish_development(
            State(test_state()),
            real_org_ctx(),
            Path(Uuid::new_v4()),
            Json(PublishWorkflowRequest {
                workflow_version: 0,
                expected_release_revision: 0,
            }),
        )
        .await;
        assert!(matches!(result, Err(ApiError::InvalidRequest(_))));
    }

    #[tokio::test]
    async fn release_history_rejects_unknown_environment_before_storage() {
        let result = list_release_history(
            State(test_state()),
            real_org_ctx(),
            Path((Uuid::new_v4(), "preview".to_owned())),
        )
        .await;
        assert!(matches!(result, Err(ApiError::InvalidRequest(_))));
    }

    #[tokio::test]
    async fn promote_rejects_development_and_unknown_environment_before_storage() {
        for environment in ["development", "preview"] {
            let result = promote_environment(
                State(test_state()),
                real_org_ctx(),
                Path((Uuid::new_v4(), environment.to_owned())),
                Json(PromoteWorkflowRequest {
                    expected_source_revision: 1,
                    expected_release_revision: 0,
                }),
            )
            .await;
            assert!(matches!(result, Err(ApiError::InvalidRequest(_))));
        }
    }

    #[tokio::test]
    async fn rollback_rejects_zero_revisions_before_storage() {
        for body in [
            RollbackWorkflowRequest {
                release_revision: 0,
                expected_release_revision: 1,
            },
            RollbackWorkflowRequest {
                release_revision: 1,
                expected_release_revision: 0,
            },
        ] {
            let result = rollback_environment(
                State(test_state()),
                real_org_ctx(),
                Path((Uuid::new_v4(), "production".to_owned())),
                Json(body),
            )
            .await;
            assert!(matches!(result, Err(ApiError::InvalidRequest(_))));
        }
    }

    #[test]
    fn release_revision_bounds_preserve_space_for_the_next_revision() {
        assert_eq!(
            expected_release_revision((i32::MAX - 1) as u32, false)
                .expect("penultimate release revision"),
            i32::MAX - 1
        );
        assert!(expected_release_revision(i32::MAX as u32, false).is_err());
        assert!(persisted_revision(i32::MAX as u32, "release_revision", false).is_ok());
    }

    #[test]
    fn release_responses_are_private_no_store() {
        let response = private_no_store_json(serde_json::json!({"ok": true}));
        assert_eq!(
            response.headers().get(CACHE_CONTROL),
            Some(&HeaderValue::from_static("private, no-store"))
        );
    }

    #[test]
    fn release_history_is_bounded_with_one_sentinel() {
        let mut rows = (1..=(MAX_WORKFLOW_RELEASE_HISTORY + 1))
            .map(|revision| release_store::WorkflowEnvironmentRelease {
                environment: release_store::WorkflowEnvironment::Development,
                revision: revision as i32,
                workflow_version: revision as i32,
                content_hash: format!("hash-{revision}"),
                action: release_store::WorkflowReleaseAction::Publish,
                source_environment: None,
                source_revision: None,
                created_at: chrono::DateTime::UNIX_EPOCH,
            })
            .collect::<Vec<_>>();
        let truncated = rows.len() > MAX_WORKFLOW_RELEASE_HISTORY;
        rows.truncate(MAX_WORKFLOW_RELEASE_HISTORY);
        assert!(truncated);
        assert_eq!(rows.len(), MAX_WORKFLOW_RELEASE_HISTORY);
    }

    #[test]
    fn request_contracts_reject_unknown_fields() {
        assert!(
            serde_json::from_value::<PublishWorkflowRequest>(serde_json::json!({
                "workflow_version": 1,
                "expected_release_revision": 0,
                "approve": true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<PromoteWorkflowRequest>(serde_json::json!({
                "expected_source_revision": 1,
                "expected_release_revision": 0,
                "environment": "production"
            }))
            .is_err()
        );
    }
}
