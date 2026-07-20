//! Versioned, non-secret configuration for workflow release environments.
//!
//! Each write replaces the complete bounded map under an optimistic revision.
//! Snapshots are append-only, while a small current-state row selects the map
//! accepted by a new environment-bound execution. Revision `0` is the implicit
//! empty set and is never persisted.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row as _};
use uuid::Uuid;

use super::{
    release_store::WorkflowEnvironment,
    types::{NodeKind, WorkflowDefinition},
};

pub(crate) const MAX_WORKFLOW_ENVIRONMENT_VARIABLES: usize = 100;
pub(crate) const MAX_WORKFLOW_ENVIRONMENT_VARIABLE_VALUE_BYTES: usize = 4 * 1024;
pub(crate) const MAX_WORKFLOW_ENVIRONMENT_VARIABLE_SET_BYTES: usize = 64 * 1024;

const LOCK_WORKFLOW_SQL: &str = "\
SELECT version FROM workflow_definitions \
WHERE org_id = $1 AND id = $2 \
ORDER BY version DESC LIMIT 1 FOR UPDATE";

const WORKFLOW_EXISTS_SQL: &str = "\
SELECT EXISTS (\
  SELECT 1 FROM workflow_definitions WHERE org_id = $1 AND id = $2\
)";

const GET_CURRENT_VARIABLES_SQL: &str = "\
SELECT s.revision, v.variables, v.created_at \
FROM workflow_environment_variable_state s \
JOIN workflow_environment_variable_sets v \
  ON v.org_id = s.org_id AND v.workflow_id = s.workflow_id \
 AND v.environment = s.environment AND v.revision = s.revision \
WHERE s.org_id = $1 AND s.workflow_id = $2 AND s.environment = $3";

const INSERT_VARIABLE_SET_SQL: &str = "\
INSERT INTO workflow_environment_variable_sets \
  (org_id, workflow_id, environment, revision, variables) \
VALUES ($1, $2, $3, $4, $5) \
RETURNING created_at";

const UPSERT_VARIABLE_STATE_SQL: &str = "\
INSERT INTO workflow_environment_variable_state \
  (org_id, workflow_id, environment, revision) \
VALUES ($1, $2, $3, $4) \
ON CONFLICT (org_id, workflow_id, environment) DO UPDATE \
SET revision = EXCLUDED.revision, updated_at = now()";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkflowEnvironmentVariables {
    pub(crate) revision: i32,
    pub(crate) variables: BTreeMap<String, String>,
    pub(crate) created_at: Option<DateTime<Utc>>,
}

pub(crate) fn is_valid_variable_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

pub(crate) fn validate_variable_set(variables: &BTreeMap<String, String>) -> Result<(), String> {
    if variables.len() > MAX_WORKFLOW_ENVIRONMENT_VARIABLES {
        return Err(format!(
            "environment variables must contain at most {MAX_WORKFLOW_ENVIRONMENT_VARIABLES} entries"
        ));
    }
    for (name, value) in variables {
        if !is_valid_variable_name(name) {
            return Err("environment variable names must match ^[A-Z0-9_]{1,64}$".into());
        }
        if value.len() > MAX_WORKFLOW_ENVIRONMENT_VARIABLE_VALUE_BYTES {
            return Err(format!(
                "environment variable values must contain at most {MAX_WORKFLOW_ENVIRONMENT_VARIABLE_VALUE_BYTES} UTF-8 bytes"
            ));
        }
    }
    let encoded = serde_json::to_vec(variables)
        .map_err(|_| "environment variables could not be serialized".to_string())?;
    if encoded.len() > MAX_WORKFLOW_ENVIRONMENT_VARIABLE_SET_BYTES {
        return Err(format!(
            "environment variable set must encode to at most {MAX_WORKFLOW_ENVIRONMENT_VARIABLE_SET_BYTES} bytes"
        ));
    }
    Ok(())
}

/// Collect every exact `{{variables.NAME}}` binding used by executable
/// template fields. Invalid and unclosed references are rejected without
/// echoing their surrounding definition text.
pub(crate) fn required_variable_names(
    definition: &WorkflowDefinition,
) -> Result<BTreeSet<String>, Vec<String>> {
    let mut names = BTreeSet::new();
    let mut errors = Vec::new();

    for node in &definition.nodes {
        match &node.kind {
            NodeKind::Model { prompt, .. } | NodeKind::Agent { prompt, .. } => {
                scan_variable_references(prompt, &node.id, "prompt", &mut names, &mut errors);
            }
            NodeKind::Transform { expr } => {
                scan_variable_references(expr, &node.id, "expression", &mut names, &mut errors);
            }
            NodeKind::Branch { cond, .. } | NodeKind::Loop { cond, .. } => {
                scan_variable_references(cond, &node.id, "condition", &mut names, &mut errors);
            }
            NodeKind::Http {
                url, headers, body, ..
            } => {
                scan_variable_references(url, &node.id, "HTTP url", &mut names, &mut errors);
                for (_, value) in headers {
                    scan_variable_references(
                        value,
                        &node.id,
                        "HTTP header",
                        &mut names,
                        &mut errors,
                    );
                }
                if let Some(body) = body {
                    scan_variable_references(body, &node.id, "HTTP body", &mut names, &mut errors);
                }
            }
            NodeKind::Document {
                cache_key: Some(cache_key),
                ..
            } => {
                scan_variable_references(
                    cache_key,
                    &node.id,
                    "document cache_key",
                    &mut names,
                    &mut errors,
                );
            }
            NodeKind::Trigger
            | NodeKind::Output
            | NodeKind::SubWorkflow { .. }
            | NodeKind::Document {
                cache_key: None, ..
            } => {}
        }
    }
    for edge in &definition.edges {
        if let Some(map) = &edge.map {
            scan_variable_references(map, &edge.from, "edge map", &mut names, &mut errors);
        }
    }

    if errors.is_empty() {
        Ok(names)
    } else {
        Err(errors)
    }
}

fn scan_variable_references(
    value: &str,
    owner_id: &str,
    field: &str,
    names: &mut BTreeSet<String>,
    errors: &mut Vec<String>,
) {
    let mut remaining = value;
    while let Some(start) = remaining.find("{{") {
        let after_open = &remaining[start + 2..];
        let Some(end) = after_open.find("}}") else {
            if after_open.trim_start().starts_with("variables") {
                errors.push(format!(
                    "node or edge \"{owner_id}\": {field} contains an unclosed \
                     {{{{variables.NAME}}}} reference"
                ));
            }
            return;
        };
        let reference = after_open[..end].trim();
        if reference == "variables" || reference.starts_with("variables.") {
            let name = reference.strip_prefix("variables.").unwrap_or_default();
            if is_valid_variable_name(name) {
                names.insert(name.to_string());
            } else {
                errors.push(format!(
                    "node or edge \"{owner_id}\": {field} contains an invalid variable \
                     reference; names must match ^[A-Z0-9_]{{1,64}}$"
                ));
            }
        }
        remaining = &after_open[end + 2..];
    }
}

fn variables_from_value(value: serde_json::Value) -> Result<BTreeMap<String, String>, sqlx::Error> {
    let variables = serde_json::from_value::<BTreeMap<String, String>>(value)
        .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
    validate_variable_set(&variables).map_err(sqlx::Error::Protocol)?;
    Ok(variables)
}

fn snapshot_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<WorkflowEnvironmentVariables, sqlx::Error> {
    Ok(WorkflowEnvironmentVariables {
        revision: row.try_get("revision")?,
        variables: variables_from_value(row.try_get("variables")?)?,
        created_at: Some(row.try_get("created_at")?),
    })
}

pub(crate) async fn get_current_variables(
    pool: &PgPool,
    org_id: Uuid,
    workflow_id: Uuid,
    environment: WorkflowEnvironment,
) -> Result<WorkflowEnvironmentVariables, sqlx::Error> {
    let row = sqlx::query(GET_CURRENT_VARIABLES_SQL)
        .bind(org_id)
        .bind(workflow_id)
        .bind(environment.as_str())
        .fetch_optional(pool)
        .await?;
    row.as_ref()
        .map(snapshot_from_row)
        .transpose()
        .map(|value| {
            value.unwrap_or(WorkflowEnvironmentVariables {
                revision: 0,
                variables: BTreeMap::new(),
                created_at: None,
            })
        })
}

pub(crate) async fn workflow_exists(
    pool: &PgPool,
    org_id: Uuid,
    workflow_id: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(WORKFLOW_EXISTS_SQL)
        .bind(org_id)
        .bind(workflow_id)
        .fetch_one(pool)
        .await
}

/// Replace the complete current set when `expected_revision` matches.
/// Returning `None` means either the workflow is absent in this org or the
/// optimistic revision is stale. Callers that already proved ownership map it
/// to a conflict. Repeating the exact current set is idempotent.
pub(crate) async fn replace_current_variables(
    pool: &PgPool,
    org_id: Uuid,
    workflow_id: Uuid,
    environment: WorkflowEnvironment,
    expected_revision: i32,
    variables: &BTreeMap<String, String>,
) -> Result<Option<WorkflowEnvironmentVariables>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let workflow_exists = sqlx::query_scalar::<_, i32>(LOCK_WORKFLOW_SQL)
        .bind(org_id)
        .bind(workflow_id)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
    if !workflow_exists {
        tx.rollback().await?;
        return Ok(None);
    }

    let current = sqlx::query(GET_CURRENT_VARIABLES_SQL)
        .bind(org_id)
        .bind(workflow_id)
        .bind(environment.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .as_ref()
        .map(snapshot_from_row)
        .transpose()?;
    let current_revision = current.as_ref().map_or(0, |current| current.revision);
    let current_variables = current
        .as_ref()
        .map_or_else(BTreeMap::new, |current| current.variables.clone());
    if current_variables == *variables
        && (expected_revision == current_revision
            || expected_revision.checked_add(1) == Some(current_revision))
    {
        tx.commit().await?;
        return Ok(Some(current.unwrap_or(WorkflowEnvironmentVariables {
            revision: 0,
            variables: BTreeMap::new(),
            created_at: None,
        })));
    }
    if expected_revision != current_revision || current_revision >= i32::MAX - 1 {
        tx.rollback().await?;
        return Ok(None);
    }
    let next_revision = current_revision + 1;
    let encoded =
        serde_json::to_value(variables).map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
    let created_at: DateTime<Utc> = sqlx::query_scalar(INSERT_VARIABLE_SET_SQL)
        .bind(org_id)
        .bind(workflow_id)
        .bind(environment.as_str())
        .bind(next_revision)
        .bind(encoded)
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query(UPSERT_VARIABLE_STATE_SQL)
        .bind(org_id)
        .bind(workflow_id)
        .bind(environment.as_str())
        .bind(next_revision)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(WorkflowEnvironmentVariables {
        revision: next_revision,
        variables: variables.clone(),
        created_at: Some(created_at),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variable_sets_are_bounded_and_names_are_closed() {
        let mut variables = BTreeMap::from([
            ("API_BASE".into(), "https://api.example.com".into()),
            ("EMPTY".into(), String::new()),
        ]);
        assert!(validate_variable_set(&variables).is_ok());
        variables.insert("bad-name".into(), "value".into());
        assert!(validate_variable_set(&variables).is_err());

        let oversized = BTreeMap::from([(
            "VALUE".into(),
            "x".repeat(MAX_WORKFLOW_ENVIRONMENT_VARIABLE_VALUE_BYTES + 1),
        )]);
        assert!(validate_variable_set(&oversized).is_err());
    }

    #[test]
    fn queries_are_tenant_workflow_environment_scoped_and_append_only() {
        for fragment in ["s.org_id = $1", "s.workflow_id = $2", "s.environment = $3"] {
            assert!(GET_CURRENT_VARIABLES_SQL.contains(fragment));
        }
        assert!(LOCK_WORKFLOW_SQL.contains("org_id = $1 AND id = $2"));
        assert!(WORKFLOW_EXISTS_SQL.contains("org_id = $1 AND id = $2"));
        assert!(INSERT_VARIABLE_SET_SQL.contains("INSERT INTO workflow_environment_variable_sets"));
        assert!(!INSERT_VARIABLE_SET_SQL.contains("UPDATE workflow_environment_variable_sets"));
        assert!(UPSERT_VARIABLE_STATE_SQL.contains("updated_at = now()"));
    }

    #[test]
    fn variable_references_cover_every_executable_template_surface() {
        let definition: WorkflowDefinition = serde_json::from_value(serde_json::json!({
            "id": Uuid::nil(),
            "version": 1,
            "name": "variables",
            "nodes": [
                {"id": "trigger", "type": "trigger"},
                {"id": "model", "type": "model", "selection": {"type": "model", "model": "m"}, "prompt": "{{variables.MODEL}}"},
                {"id": "transform", "type": "transform", "expr": "{{ variables.TRANSFORM }}"},
                {"id": "branch", "type": "branch", "cond": "{{variables.BRANCH}}", "when_true": "http", "when_false": "http"},
                {"id": "http", "type": "http", "method": "POST", "url": "https://example.com/{{variables.URL}}", "headers": [["x-region", "{{variables.HEADER}}"]], "body": "{{variables.BODY}}"},
                {"id": "loop", "type": "loop", "body_workflow_id": Uuid::nil(), "cond": "{{variables.LOOP}}", "max_iters": 1},
                {"id": "document", "type": "document", "source": {"type": "base64", "media_type": "text/plain", "data": "YQ=="}, "cache_key": "{{variables.CACHE}}"},
                {"id": "output", "type": "output"}
            ],
            "edges": [{"from": "model", "to": "output", "map": "{{variables.EDGE}}"}],
            "allowed_hosts": ["example.com"]
        }))
        .unwrap();
        assert_eq!(
            required_variable_names(&definition).unwrap(),
            BTreeSet::from([
                "BODY".into(),
                "BRANCH".into(),
                "CACHE".into(),
                "EDGE".into(),
                "HEADER".into(),
                "LOOP".into(),
                "MODEL".into(),
                "TRANSFORM".into(),
                "URL".into(),
            ])
        );
    }

    #[test]
    fn malformed_variable_references_fail_closed() {
        let mut definition: WorkflowDefinition = serde_json::from_value(serde_json::json!({
            "id": Uuid::nil(), "version": 1, "name": "bad",
            "nodes": [
                {"id": "trigger", "type": "trigger"},
                {"id": "transform", "type": "transform", "expr": "{{variables.bad-name}}"},
                {"id": "output", "type": "output"}
            ],
            "edges": [{"from": "trigger", "to": "transform"}, {"from": "transform", "to": "output"}]
        }))
        .unwrap();
        assert!(required_variable_names(&definition).is_err());
        if let NodeKind::Transform { expr } = &mut definition.nodes[1].kind {
            *expr = "{{variables.MISSING".into();
        }
        assert!(required_variable_names(&definition).is_err());
    }
}
