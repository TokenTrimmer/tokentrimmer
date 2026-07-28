//! Versioned non-secret configuration for workflow release environments.
//!
//! Variables are replaced as a complete map under an optimistic revision and
//! are deliberately distinct from encrypted workflow secrets. Management reads
//! return values, so callers must never place credentials in this surface.

use std::collections::BTreeMap;

use axum::{
    extract::{Path, State},
    http::{header::CACHE_CONTROL, HeaderValue},
    response::{IntoResponse, Response},
    Extension, Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tt_auth::ApiKeyContext;
use uuid::Uuid;

use crate::{
    error::{ApiError, ApiResult},
    workflow::{
        environment_variables::{
            get_current_variables, replace_current_variables, validate_variable_set,
            workflow_exists, WorkflowEnvironmentVariables,
        },
        release_store::WorkflowEnvironment,
    },
    AppState, DOGFOOD_ORG_ID,
};

const MAX_EXPECTED_VARIABLE_REVISION: u32 = (i32::MAX - 1) as u32;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaceWorkflowEnvironmentVariablesRequest {
    pub expected_revision: u32,
    pub variables: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct WorkflowEnvironmentVariablesResponse {
    object: &'static str,
    workflow_id: Uuid,
    environment: &'static str,
    revision: i32,
    variables: BTreeMap<String, String>,
    updated_at: Option<DateTime<Utc>>,
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
            "workflow environment variable storage requires a Postgres pool".into(),
        )
    })
}

fn parse_environment(value: &str) -> ApiResult<WorkflowEnvironment> {
    match value {
        "development" => Ok(WorkflowEnvironment::Development),
        "staging" => Ok(WorkflowEnvironment::Staging),
        "production" => Ok(WorkflowEnvironment::Production),
        _ => Err(ApiError::NotFound("workflow environment not found".into())),
    }
}

fn response(
    workflow_id: Uuid,
    environment: WorkflowEnvironment,
    snapshot: WorkflowEnvironmentVariables,
) -> Response {
    let mut response = Json(WorkflowEnvironmentVariablesResponse {
        object: "workflow_environment_variables",
        workflow_id,
        environment: environment.as_str(),
        revision: snapshot.revision,
        variables: snapshot.variables,
        updated_at: snapshot.created_at,
    })
    .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response
}

async fn require_workflow(pool: &sqlx::PgPool, org_id: Uuid, workflow_id: Uuid) -> ApiResult<()> {
    workflow_exists(pool, org_id, workflow_id)
        .await
        .map_err(|error| {
            tracing::warn!(%org_id, %workflow_id, %error, "workflow ownership check failed");
            ApiError::ServiceUnavailable(
                "workflow environment variable storage is temporarily unavailable".into(),
            )
        })?
        .then_some(())
        .ok_or_else(|| ApiError::NotFound("workflow not found".into()))
}

/// `GET /v1/workflows/:id/environments/:environment/variables`
pub async fn get_environment_variables(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path((workflow_id, environment)): Path<(Uuid, String)>,
) -> ApiResult<Response> {
    let org_id = require_org(ctx)?;
    let environment = parse_environment(&environment)?;
    let pool = db_pool(&state)?;
    require_workflow(pool, org_id, workflow_id).await?;
    let snapshot = get_current_variables(pool, org_id, workflow_id, environment)
        .await
        .map_err(|error| {
            tracing::warn!(%org_id, %workflow_id, %error, "workflow environment variables read failed");
            ApiError::ServiceUnavailable(
                "workflow environment variables are temporarily unavailable".into(),
            )
        })?;
    Ok(response(workflow_id, environment, snapshot))
}

/// `PUT /v1/workflows/:id/environments/:environment/variables`
pub async fn replace_environment_variables(
    State(state): State<AppState>,
    ctx: Option<Extension<ApiKeyContext>>,
    Path((workflow_id, environment)): Path<(Uuid, String)>,
    Json(body): Json<ReplaceWorkflowEnvironmentVariablesRequest>,
) -> ApiResult<Response> {
    let org_id = require_org(ctx)?;
    let environment = parse_environment(&environment)?;
    if body.expected_revision > MAX_EXPECTED_VARIABLE_REVISION {
        return Err(ApiError::InvalidRequest(format!(
            "expected_revision must be at most {MAX_EXPECTED_VARIABLE_REVISION}"
        )));
    }
    validate_variable_set(&body.variables).map_err(ApiError::InvalidRequest)?;
    let pool = db_pool(&state)?;
    require_workflow(pool, org_id, workflow_id).await?;
    let snapshot = replace_current_variables(
        pool,
        org_id,
        workflow_id,
        environment,
        body.expected_revision as i32,
        &body.variables,
    )
    .await
    .map_err(|error| {
        tracing::error!(%org_id, %workflow_id, %error, "workflow environment variables replace failed");
        ApiError::Internal("failed to replace workflow environment variables".into())
    })?
    .ok_or_else(|| {
        ApiError::Conflict(
            "workflow environment variables changed; reload the current revision".into(),
        )
    })?;
    Ok(response(workflow_id, environment, snapshot))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_and_revision_contracts_are_closed() {
        assert_eq!(
            parse_environment("development").unwrap(),
            WorkflowEnvironment::Development
        );
        assert!(matches!(
            parse_environment("preview"),
            Err(ApiError::NotFound(_))
        ));
        let valid: ReplaceWorkflowEnvironmentVariablesRequest =
            serde_json::from_value(serde_json::json!({
                "expected_revision": 0,
                "variables": {"API_BASE": "https://api.example.com"}
            }))
            .unwrap();
        assert!(validate_variable_set(&valid.variables).is_ok());
        assert!(
            serde_json::from_value::<ReplaceWorkflowEnvironmentVariablesRequest>(
                serde_json::json!({
                    "expected_revision": 0,
                    "variables": {},
                    "secret": "must not be accepted"
                })
            )
            .is_err()
        );
    }

    #[test]
    fn response_is_explicitly_non_secret_and_revisioned() {
        let encoded = serde_json::to_value(WorkflowEnvironmentVariablesResponse {
            object: "workflow_environment_variables",
            workflow_id: Uuid::nil(),
            environment: "production",
            revision: 2,
            variables: BTreeMap::from([("REGION".into(), "us-east".into())]),
            updated_at: None,
        })
        .unwrap();
        assert_eq!(encoded["revision"], 2);
        assert_eq!(encoded["variables"]["REGION"], "us-east");
        assert!(encoded.get("secret_values").is_none());
    }
}
