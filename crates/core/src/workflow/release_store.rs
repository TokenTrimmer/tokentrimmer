//! Concurrency-safe current workflow environment pointers and append-only
//! release history.
//!
//! The current-state row is the compare-and-swap boundary. Every successful
//! state transition inserts its immutable ledger row in the same statement, so
//! a pointer can never advance without matching history.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkflowEnvironment {
    Development,
    Staging,
    Production,
}

impl WorkflowEnvironment {
    pub(crate) const ALL: [Self; 3] = [Self::Development, Self::Staging, Self::Production];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Development => "development",
            Self::Staging => "staging",
            Self::Production => "production",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, sqlx::Error> {
        match value {
            "development" => Ok(Self::Development),
            "staging" => Ok(Self::Staging),
            "production" => Ok(Self::Production),
            other => Err(sqlx::Error::Protocol(format!(
                "workflow environment release has invalid environment {other:?}"
            ))),
        }
    }

    pub(crate) const fn promotion_source(self) -> Option<Self> {
        match self {
            Self::Development => None,
            Self::Staging => Some(Self::Development),
            Self::Production => Some(Self::Staging),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkflowReleaseAction {
    Publish,
    Promote,
    Rollback,
}

impl WorkflowReleaseAction {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Publish => "publish",
            Self::Promote => "promote",
            Self::Rollback => "rollback",
        }
    }

    fn parse(value: &str) -> Result<Self, sqlx::Error> {
        match value {
            "publish" => Ok(Self::Publish),
            "promote" => Ok(Self::Promote),
            "rollback" => Ok(Self::Rollback),
            other => Err(sqlx::Error::Protocol(format!(
                "workflow environment release has invalid action {other:?}"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct WorkflowEnvironmentRelease {
    pub environment: WorkflowEnvironment,
    pub revision: i32,
    pub workflow_version: i32,
    pub content_hash: String,
    pub action: WorkflowReleaseAction,
    pub source_environment: Option<WorkflowEnvironment>,
    pub source_revision: Option<i32>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkflowReleaseMutation {
    pub environment: WorkflowEnvironment,
    pub revision: i32,
    pub workflow_version: i32,
    pub content_hash: String,
    pub action: WorkflowReleaseAction,
    pub source_environment: Option<WorkflowEnvironment>,
    pub source_revision: Option<i32>,
    pub created_at: DateTime<Utc>,
}

pub(crate) const LIST_CURRENT_RELEASES_SQL: &str = "\
SELECT s.environment, s.revision, s.workflow_version, d.content_hash, \
       r.action, r.source_environment, r.source_revision, r.created_at \
FROM workflow_environment_state s \
JOIN workflow_definitions d \
  ON d.org_id = s.org_id AND d.id = s.workflow_id AND d.version = s.workflow_version \
JOIN workflow_environment_releases r \
  ON r.org_id = s.org_id AND r.workflow_id = s.workflow_id \
 AND r.environment = s.environment AND r.revision = s.revision \
WHERE s.org_id = $1 AND s.workflow_id = $2 \
ORDER BY s.environment";

pub(crate) const GET_CURRENT_RELEASE_SQL: &str = "\
SELECT s.environment, s.revision, s.workflow_version, d.content_hash, \
       r.action, r.source_environment, r.source_revision, r.created_at \
FROM workflow_environment_state s \
JOIN workflow_definitions d \
  ON d.org_id = s.org_id AND d.id = s.workflow_id AND d.version = s.workflow_version \
JOIN workflow_environment_releases r \
  ON r.org_id = s.org_id AND r.workflow_id = s.workflow_id \
 AND r.environment = s.environment AND r.revision = s.revision \
WHERE s.org_id = $1 AND s.workflow_id = $2 AND s.environment = $3";

pub(crate) const LIST_RELEASE_HISTORY_SQL: &str = "\
SELECT r.environment, r.revision, r.workflow_version, d.content_hash, \
       r.action, r.source_environment, r.source_revision, r.created_at \
FROM workflow_environment_releases r \
JOIN workflow_definitions d \
  ON d.org_id = r.org_id AND d.id = r.workflow_id AND d.version = r.workflow_version \
WHERE r.org_id = $1 AND r.workflow_id = $2 AND r.environment = $3 \
ORDER BY r.revision DESC \
LIMIT $4";

pub(crate) const PUBLISH_DEVELOPMENT_SQL: &str = "\
WITH updated AS ( \
  UPDATE workflow_environment_state \
  SET revision = revision + 1, workflow_version = $3, updated_at = now() \
  WHERE org_id = $1 AND workflow_id = $2 AND environment = 'development' \
    AND revision = $4 AND workflow_version < $3 \
  RETURNING revision, workflow_version \
), inserted AS ( \
  INSERT INTO workflow_environment_state \
    (org_id, workflow_id, environment, revision, workflow_version) \
  SELECT $1, $2, 'development', 1, $3 WHERE $4 = 0 \
  ON CONFLICT (org_id, workflow_id, environment) DO NOTHING \
  RETURNING revision, workflow_version \
), advanced AS ( \
  SELECT revision, workflow_version FROM updated \
  UNION ALL \
  SELECT revision, workflow_version FROM inserted \
), released AS ( \
  INSERT INTO workflow_environment_releases \
    (org_id, workflow_id, environment, revision, workflow_version, action) \
  SELECT $1, $2, 'development', revision, workflow_version, 'publish' \
  FROM advanced \
  RETURNING environment, revision, workflow_version, action, \
            source_environment, source_revision, created_at \
) \
SELECT released.*, d.content_hash \
FROM released \
JOIN workflow_definitions d \
  ON d.org_id = $1 AND d.id = $2 AND d.version = released.workflow_version";

pub(crate) const PROMOTE_ENVIRONMENT_SQL: &str = "\
WITH source AS ( \
  SELECT environment, revision, workflow_version \
  FROM workflow_environment_state \
  WHERE org_id = $1 AND workflow_id = $2 AND environment = $3 \
    AND revision = $6 \
  FOR SHARE \
), updated AS ( \
  UPDATE workflow_environment_state AS state \
  SET revision = state.revision + 1, \
      workflow_version = source.workflow_version, \
      updated_at = now() \
  FROM source \
  WHERE state.org_id = $1 AND state.workflow_id = $2 AND state.environment = $4 \
    AND state.revision = $5 \
    AND state.workflow_version <> source.workflow_version \
  RETURNING state.revision, state.workflow_version \
), inserted AS ( \
  INSERT INTO workflow_environment_state \
    (org_id, workflow_id, environment, revision, workflow_version) \
  SELECT $1, $2, $4, 1, workflow_version FROM source WHERE $5 = 0 \
  ON CONFLICT (org_id, workflow_id, environment) DO NOTHING \
  RETURNING revision, workflow_version \
), advanced AS ( \
  SELECT revision, workflow_version FROM updated \
  UNION ALL \
  SELECT revision, workflow_version FROM inserted \
), released AS ( \
  INSERT INTO workflow_environment_releases \
    (org_id, workflow_id, environment, revision, workflow_version, action, \
     source_environment, source_revision) \
  SELECT $1, $2, $4, advanced.revision, advanced.workflow_version, 'promote', \
         source.environment, source.revision \
  FROM advanced CROSS JOIN source \
  RETURNING environment, revision, workflow_version, action, \
            source_environment, source_revision, created_at \
) \
SELECT released.*, d.content_hash \
FROM released \
JOIN workflow_definitions d \
  ON d.org_id = $1 AND d.id = $2 AND d.version = released.workflow_version";

pub(crate) const ROLLBACK_ENVIRONMENT_SQL: &str = "\
WITH target AS ( \
  SELECT workflow_version, revision \
  FROM workflow_environment_releases \
  WHERE org_id = $1 AND workflow_id = $2 AND environment = $3 AND revision = $4 \
), advanced AS ( \
  UPDATE workflow_environment_state AS state \
  SET revision = state.revision + 1, \
      workflow_version = target.workflow_version, \
      updated_at = now() \
  FROM target \
  WHERE state.org_id = $1 AND state.workflow_id = $2 AND state.environment = $3 \
    AND state.revision = $5 \
    AND state.workflow_version <> target.workflow_version \
  RETURNING state.revision, state.workflow_version \
), released AS ( \
  INSERT INTO workflow_environment_releases \
    (org_id, workflow_id, environment, revision, workflow_version, action, \
     source_environment, source_revision) \
  SELECT $1, $2, $3, advanced.revision, advanced.workflow_version, 'rollback', \
         $3, target.revision \
  FROM advanced CROSS JOIN target \
  RETURNING environment, revision, workflow_version, action, \
            source_environment, source_revision, created_at \
) \
SELECT released.*, d.content_hash \
FROM released \
JOIN workflow_definitions d \
  ON d.org_id = $1 AND d.id = $2 AND d.version = released.workflow_version";

fn mutation_from_row(row: &sqlx::postgres::PgRow) -> Result<WorkflowReleaseMutation, sqlx::Error> {
    let environment = WorkflowEnvironment::parse(row.try_get("environment")?)?;
    let action = WorkflowReleaseAction::parse(row.try_get("action")?)?;
    let source_environment = row
        .try_get::<Option<String>, _>("source_environment")?
        .as_deref()
        .map(WorkflowEnvironment::parse)
        .transpose()?;
    Ok(WorkflowReleaseMutation {
        environment,
        revision: row.try_get("revision")?,
        workflow_version: row.try_get("workflow_version")?,
        content_hash: row.try_get("content_hash")?,
        action,
        source_environment,
        source_revision: row.try_get("source_revision")?,
        created_at: row.try_get("created_at")?,
    })
}

pub(crate) async fn list_current_releases(
    pool: &PgPool,
    org_id: Uuid,
    workflow_id: Uuid,
) -> Result<Vec<WorkflowEnvironmentRelease>, sqlx::Error> {
    sqlx::query(LIST_CURRENT_RELEASES_SQL)
        .bind(org_id)
        .bind(workflow_id)
        .fetch_all(pool)
        .await?
        .iter()
        .map(|row| {
            let mutation = mutation_from_row(row)?;
            Ok(WorkflowEnvironmentRelease {
                environment: mutation.environment,
                revision: mutation.revision,
                workflow_version: mutation.workflow_version,
                content_hash: mutation.content_hash,
                action: mutation.action,
                source_environment: mutation.source_environment,
                source_revision: mutation.source_revision,
                created_at: mutation.created_at,
            })
        })
        .collect()
}

pub(crate) async fn get_current_release(
    pool: &PgPool,
    org_id: Uuid,
    workflow_id: Uuid,
    environment: WorkflowEnvironment,
) -> Result<Option<WorkflowEnvironmentRelease>, sqlx::Error> {
    sqlx::query(GET_CURRENT_RELEASE_SQL)
        .bind(org_id)
        .bind(workflow_id)
        .bind(environment.as_str())
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(|row| {
            let mutation = mutation_from_row(row)?;
            Ok(WorkflowEnvironmentRelease {
                environment: mutation.environment,
                revision: mutation.revision,
                workflow_version: mutation.workflow_version,
                content_hash: mutation.content_hash,
                action: mutation.action,
                source_environment: mutation.source_environment,
                source_revision: mutation.source_revision,
                created_at: mutation.created_at,
            })
        })
        .transpose()
}

pub(crate) async fn list_release_history(
    pool: &PgPool,
    org_id: Uuid,
    workflow_id: Uuid,
    environment: WorkflowEnvironment,
    limit: i64,
) -> Result<Vec<WorkflowEnvironmentRelease>, sqlx::Error> {
    sqlx::query(LIST_RELEASE_HISTORY_SQL)
        .bind(org_id)
        .bind(workflow_id)
        .bind(environment.as_str())
        .bind(limit)
        .fetch_all(pool)
        .await?
        .iter()
        .map(|row| {
            let mutation = mutation_from_row(row)?;
            Ok(WorkflowEnvironmentRelease {
                environment: mutation.environment,
                revision: mutation.revision,
                workflow_version: mutation.workflow_version,
                content_hash: mutation.content_hash,
                action: mutation.action,
                source_environment: mutation.source_environment,
                source_revision: mutation.source_revision,
                created_at: mutation.created_at,
            })
        })
        .collect()
}

pub(crate) async fn publish_development(
    pool: &PgPool,
    org_id: Uuid,
    workflow_id: Uuid,
    workflow_version: i32,
    expected_revision: i32,
) -> Result<Option<WorkflowReleaseMutation>, sqlx::Error> {
    sqlx::query(PUBLISH_DEVELOPMENT_SQL)
        .bind(org_id)
        .bind(workflow_id)
        .bind(workflow_version)
        .bind(expected_revision)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(mutation_from_row)
        .transpose()
}

pub(crate) async fn promote_environment(
    pool: &PgPool,
    org_id: Uuid,
    workflow_id: Uuid,
    source: WorkflowEnvironment,
    target: WorkflowEnvironment,
    expected_target_revision: i32,
    expected_source_revision: i32,
) -> Result<Option<WorkflowReleaseMutation>, sqlx::Error> {
    sqlx::query(PROMOTE_ENVIRONMENT_SQL)
        .bind(org_id)
        .bind(workflow_id)
        .bind(source.as_str())
        .bind(target.as_str())
        .bind(expected_target_revision)
        .bind(expected_source_revision)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(mutation_from_row)
        .transpose()
}

pub(crate) async fn rollback_environment(
    pool: &PgPool,
    org_id: Uuid,
    workflow_id: Uuid,
    environment: WorkflowEnvironment,
    release_revision: i32,
    expected_current_revision: i32,
) -> Result<Option<WorkflowReleaseMutation>, sqlx::Error> {
    sqlx::query(ROLLBACK_ENVIRONMENT_SQL)
        .bind(org_id)
        .bind(workflow_id)
        .bind(environment.as_str())
        .bind(release_revision)
        .bind(expected_current_revision)
        .fetch_optional(pool)
        .await?
        .as_ref()
        .map(mutation_from_row)
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environments_have_one_closed_promotion_chain() {
        assert_eq!(WorkflowEnvironment::Development.promotion_source(), None);
        assert_eq!(
            WorkflowEnvironment::Staging.promotion_source(),
            Some(WorkflowEnvironment::Development)
        );
        assert_eq!(
            WorkflowEnvironment::Production.promotion_source(),
            Some(WorkflowEnvironment::Staging)
        );
        assert_eq!(
            WorkflowEnvironment::ALL.map(WorkflowEnvironment::as_str),
            ["development", "staging", "production"]
        );
    }

    #[test]
    fn mutations_are_atomic_and_org_scoped() {
        for sql in [
            PUBLISH_DEVELOPMENT_SQL,
            PROMOTE_ENVIRONMENT_SQL,
            ROLLBACK_ENVIRONMENT_SQL,
        ] {
            assert!(sql.contains("workflow_environment_state"));
            assert!(sql.contains("workflow_environment_releases"));
            assert!(sql.contains("org_id = $1") || sql.contains("VALUES ($1"));
            assert!(sql.contains("workflow_id"));
            assert!(sql.contains("RETURNING environment, revision, workflow_version"));
        }
        assert!(PROMOTE_ENVIRONMENT_SQL.contains("FOR SHARE"));
        assert!(ROLLBACK_ENVIRONMENT_SQL.contains("state.revision = $5"));
        assert!(LIST_RELEASE_HISTORY_SQL.contains("r.org_id = $1"));
        assert!(LIST_RELEASE_HISTORY_SQL.contains("r.workflow_id = $2"));
        assert!(LIST_RELEASE_HISTORY_SQL.contains("ORDER BY r.revision DESC"));
        assert!(LIST_RELEASE_HISTORY_SQL.contains("LIMIT $4"));
    }

    #[test]
    fn current_release_lookup_is_exact_and_tenant_scoped() {
        for fragment in [
            "s.org_id = $1",
            "s.workflow_id = $2",
            "s.environment = $3",
            "r.revision = s.revision",
            "d.version = s.workflow_version",
        ] {
            assert!(
                GET_CURRENT_RELEASE_SQL.contains(fragment),
                "current release lookup missing {fragment}"
            );
        }
        assert!(!GET_CURRENT_RELEASE_SQL.contains("ORDER BY"));
        assert!(!GET_CURRENT_RELEASE_SQL.contains("LIMIT"));
    }

    #[test]
    fn persisted_wire_values_parse_strictly() {
        for environment in WorkflowEnvironment::ALL {
            assert_eq!(
                WorkflowEnvironment::parse(environment.as_str()).expect("known environment"),
                environment
            );
        }
        assert!(WorkflowEnvironment::parse("preview").is_err());
        for action in [
            WorkflowReleaseAction::Publish,
            WorkflowReleaseAction::Promote,
            WorkflowReleaseAction::Rollback,
        ] {
            assert_eq!(
                WorkflowReleaseAction::parse(action.as_str()).expect("known action"),
                action
            );
        }
        assert!(WorkflowReleaseAction::parse("restore").is_err());
    }
}
