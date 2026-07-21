//! Exact workflow definition/release/configuration selection for execution.
//!
//! Current-environment requests resolve mutable pointers once. Frozen
//! schedule/webhook deliveries instead supply the complete tuple captured by
//! the durable Cloud queue; this module then reads only immutable ledger rows
//! so delayed dispatch and retries cannot drift after promotion or variable
//! replacement.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};

use super::{
    environment_variables::{get_current_variables, get_variables_revision},
    release_store::{self, WorkflowEnvironment},
    store::{self, WorkflowRunReleaseProvenance},
    types::WorkflowDefinition,
};

/// Closed environment selector for one explicit Estimate or Run request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowExecutionEnvironment {
    Development,
    Staging,
    Production,
}

impl WorkflowExecutionEnvironment {
    pub(crate) const fn as_store(self) -> WorkflowEnvironment {
        match self {
            Self::Development => WorkflowEnvironment::Development,
            Self::Staging => WorkflowEnvironment::Staging,
            Self::Production => WorkflowEnvironment::Production,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        self.as_store().as_str()
    }

    pub(crate) const fn from_store(environment: WorkflowEnvironment) -> Self {
        match environment {
            WorkflowEnvironment::Development => Self::Development,
            WorkflowEnvironment::Staging => Self::Staging,
            WorkflowEnvironment::Production => Self::Production,
        }
    }
}

impl From<tt_routing::RouteWorkflowEnvironment> for WorkflowExecutionEnvironment {
    fn from(environment: tt_routing::RouteWorkflowEnvironment) -> Self {
        match environment {
            tt_routing::RouteWorkflowEnvironment::Development => Self::Development,
            tt_routing::RouteWorkflowEnvironment::Staging => Self::Staging,
            tt_routing::RouteWorkflowEnvironment::Production => Self::Production,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResolvedWorkflowRelease {
    pub(crate) environment: WorkflowExecutionEnvironment,
    pub(crate) revision: i32,
    pub(crate) variables_revision: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkflowExecutionSelector {
    Latest,
    Version(i32),
    CurrentEnvironment(WorkflowExecutionEnvironment),
    FrozenRelease {
        workflow_version: i32,
        environment: WorkflowExecutionEnvironment,
        release_revision: i32,
        variables_revision: i32,
    },
}

impl WorkflowExecutionSelector {
    pub(crate) const fn requested_version(self) -> Option<i32> {
        match self {
            Self::Version(version)
            | Self::FrozenRelease {
                workflow_version: version,
                ..
            } => Some(version),
            Self::Latest | Self::CurrentEnvironment(_) => None,
        }
    }

    pub(crate) const fn frozen_release(self) -> Option<(i32, WorkflowRunReleaseProvenance)> {
        match self {
            Self::FrozenRelease {
                workflow_version,
                environment,
                release_revision,
                variables_revision,
            } => Some((
                workflow_version,
                WorkflowRunReleaseProvenance {
                    environment: environment.as_store(),
                    revision: release_revision,
                    variables_revision,
                },
            )),
            Self::Latest | Self::Version(_) | Self::CurrentEnvironment(_) => None,
        }
    }
}

fn validate_requested_workflow_version(version: Option<i32>) -> ApiResult<()> {
    if version.is_some_and(|version| version <= 0) {
        return Err(ApiError::InvalidRequest(
            "workflow_version must be a positive immutable definition version".into(),
        ));
    }
    Ok(())
}

pub(crate) fn workflow_execution_selector(
    version: Option<i32>,
    environment: Option<WorkflowExecutionEnvironment>,
    release_revision: Option<i32>,
    variables_revision: Option<i32>,
) -> ApiResult<WorkflowExecutionSelector> {
    validate_requested_workflow_version(version)?;
    match (
        version,
        environment,
        release_revision,
        variables_revision,
    ) {
        (None, None, None, None) => Ok(WorkflowExecutionSelector::Latest),
        (Some(version), None, None, None) => Ok(WorkflowExecutionSelector::Version(version)),
        (None, Some(environment), None, None) => {
            Ok(WorkflowExecutionSelector::CurrentEnvironment(environment))
        }
        (Some(workflow_version), Some(environment), Some(release_revision), Some(variables_revision))
            if release_revision > 0 && variables_revision >= 0 =>
        {
            Ok(WorkflowExecutionSelector::FrozenRelease {
                workflow_version,
                environment,
                release_revision,
                variables_revision,
            })
        }
        (Some(_), Some(_), Some(release_revision), Some(variables_revision)) => {
            Err(ApiError::InvalidRequest(format!(
                "frozen workflow release requires release_revision > 0 and variables_revision >= 0 (received {release_revision} and {variables_revision})"
            )))
        }
        _ => Err(ApiError::InvalidRequest(
            "workflow selectors must be latest, workflow_version only, workflow_environment only, or the complete workflow_version/workflow_environment/release_revision/variables_revision tuple".into(),
        )),
    }
}

pub(crate) async fn resolve_workflow_execution_definition(
    pool: &sqlx::PgPool,
    org: Uuid,
    workflow_id: Uuid,
    selector: WorkflowExecutionSelector,
) -> ApiResult<(
    WorkflowDefinition,
    i32,
    Option<ResolvedWorkflowRelease>,
    BTreeMap<String, String>,
)> {
    if let WorkflowExecutionSelector::FrozenRelease {
        workflow_version,
        environment,
        release_revision,
        variables_revision,
    } = selector
    {
        let release = release_store::get_release_revision(
            pool,
            org,
            workflow_id,
            environment.as_store(),
            release_revision,
        )
        .await
        .map_err(|error| {
            tracing::warn!(
                %workflow_id,
                workflow_environment = environment.as_str(),
                release_revision,
                %error,
                "frozen workflow release lookup failed"
            );
            ApiError::ServiceUnavailable(
                "workflow release history is temporarily unavailable".into(),
            )
        })?
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "no {} release revision {} for workflow {}",
                environment.as_str(),
                release_revision,
                workflow_id
            ))
        })?;
        if release.workflow_version != workflow_version {
            return Err(ApiError::Conflict(
                "frozen workflow release does not match workflow_version".into(),
            ));
        }
        let record = store::get_definition_version_record(pool, org, workflow_id, workflow_version)
            .await
            .map_err(|error| {
                tracing::warn!(
                    %workflow_id,
                    workflow_version,
                    %error,
                    "frozen workflow definition lookup failed"
                );
                ApiError::ServiceUnavailable(
                    "released workflow definition is temporarily unavailable".into(),
                )
            })?
            .ok_or_else(|| {
                ApiError::Internal("workflow release references a missing definition".into())
            })?;
        if record.content_hash != release.content_hash {
            return Err(ApiError::Internal(
                "workflow release definition metadata is inconsistent".into(),
            ));
        }
        let variables = get_variables_revision(
            pool,
            org,
            workflow_id,
            environment.as_store(),
            variables_revision,
        )
        .await
        .map_err(|error| {
            tracing::warn!(
                %workflow_id,
                workflow_environment = environment.as_str(),
                variables_revision,
                %error,
                "frozen workflow variable snapshot lookup failed"
            );
            ApiError::ServiceUnavailable(
                "workflow environment variable history is temporarily unavailable".into(),
            )
        })?
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "no {} variable revision {} for workflow {}",
                environment.as_str(),
                variables_revision,
                workflow_id
            ))
        })?;
        return Ok((
            record.definition,
            record.version,
            Some(ResolvedWorkflowRelease {
                environment,
                revision: release.revision,
                variables_revision: variables.revision,
            }),
            variables.variables,
        ));
    }

    if let WorkflowExecutionSelector::CurrentEnvironment(environment) = selector {
        let release =
            release_store::get_current_release(pool, org, workflow_id, environment.as_store())
                .await
                .map_err(|error| {
                    tracing::warn!(
                        %workflow_id,
                        workflow_environment = environment.as_str(),
                        %error,
                        "workflow environment execution selector lookup failed"
                    );
                    ApiError::ServiceUnavailable(
                        "workflow release state is temporarily unavailable".into(),
                    )
                })?
                .ok_or_else(|| {
                    ApiError::NotFound(format!(
                        "no {environment_name} release for workflow {workflow_id}",
                        environment_name = environment.as_str()
                    ))
                })?;
        let record =
            store::get_definition_version_record(pool, org, workflow_id, release.workflow_version)
                .await
                .map_err(|error| {
                    tracing::warn!(
                        %workflow_id,
                        workflow_environment = environment.as_str(),
                        workflow_version = release.workflow_version,
                        %error,
                        "released workflow definition lookup failed"
                    );
                    ApiError::ServiceUnavailable(
                        "released workflow definition is temporarily unavailable".into(),
                    )
                })?
                .ok_or_else(|| {
                    ApiError::Internal("workflow release references a missing definition".into())
                })?;
        if record.content_hash != release.content_hash {
            return Err(ApiError::Internal(
                "workflow release definition metadata is inconsistent".into(),
            ));
        }
        let variables = get_current_variables(pool, org, workflow_id, environment.as_store())
            .await
            .map_err(|error| {
                tracing::warn!(
                    %workflow_id,
                    workflow_environment = environment.as_str(),
                    %error,
                    "workflow environment variables execution selector lookup failed"
                );
                ApiError::ServiceUnavailable(
                    "workflow environment variables are temporarily unavailable".into(),
                )
            })?;
        return Ok((
            record.definition,
            record.version,
            Some(ResolvedWorkflowRelease {
                environment,
                revision: release.revision,
                variables_revision: variables.revision,
            }),
            variables.variables,
        ));
    }

    let selected = match selector {
        WorkflowExecutionSelector::Version(version) => {
            store::get_definition_version(pool, org, workflow_id, version)
                .await
                .ok_or_else(|| {
                    ApiError::NotFound(format!(
                        "no workflow with id {workflow_id} at version {version}"
                    ))
                })?
        }
        WorkflowExecutionSelector::Latest => store::get_definition(pool, org, workflow_id)
            .await
            .ok_or_else(|| ApiError::NotFound(format!("no workflow with id {workflow_id}")))?,
        WorkflowExecutionSelector::CurrentEnvironment(_)
        | WorkflowExecutionSelector::FrozenRelease { .. } => {
            return Err(ApiError::Internal(
                "workflow execution selector reached an invalid resolution state".into(),
            ));
        }
    };
    Ok((selected.0, selected.1, None, BTreeMap::new()))
}
