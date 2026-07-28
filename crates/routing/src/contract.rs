//! Canonical, versioned route-definition contract.
//!
//! Route JSON is persisted by more than one writer (the gateway API, the
//! hosted control plane, catalog materialisation, and Plan writeback). The
//! runtime must never be asked to guess what a successful save meant. This
//! module is the gateway-side contract for schema v1: it rejects unknown and
//! future fields, normalizes default values, validates non-runtime-dependent
//! invariants, and produces a stable SHA-256 identity for the exact definition
//! the gateway will evaluate.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    validate_agentic_budget, validate_auto_pause, validate_output_shaping, validate_panel,
    validate_route_has_effect, validate_workflow, NewRoute, RouteAction, RouteConditions,
    ValidationError,
};

/// Stable identifier for the route contract encoded by this module.
pub const ROUTE_SCHEMA_ID: &str = "tokentrimmer.route.v1";
/// The only route schema version accepted for new writes in this release.
pub const ROUTE_SCHEMA_VERSION: u32 = 1;

/// A field-addressable problem in a user-supplied route definition.
///
/// `field` uses the same dotted path notation as `serde_path_to_error`, for
/// example `then.panel.members[1]`. It is deliberately transport-neutral so
/// HTTP APIs can expose it as a 422 body and forms can attach it to a field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteValidationIssue {
    pub field: String,
    pub code: String,
    pub message: String,
}

impl RouteValidationIssue {
    fn new(field: impl Into<String>, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            code: code.into(),
            message: message.into(),
        }
    }
}

/// The only representation writers are allowed to persist for a route.
#[derive(Debug, Clone)]
pub struct CanonicalRoute {
    /// Always [`ROUTE_SCHEMA_VERSION`] for a successful canonicalization.
    pub schema_version: u32,
    /// `sha256:<hex>` over the canonical schema-id/version and definition.
    pub canonical_hash: String,
    /// Typed definition the gateway evaluates.
    pub route: NewRoute,
    /// Canonical JSON for the cloud-owned `routes.conditions` column.
    pub conditions: Value,
    /// Canonical JSON for the cloud-owned `routes.target` column.
    pub target: Value,
}

/// Version-aware gateway API envelope. `schema_version` is optional only for
/// v1 backward compatibility; absent is interpreted as v1. A writer that sends
/// any other explicit version receives a field-addressed validation error.
#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "contract-schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct RouteWriteRequest {
    #[serde(default)]
    pub schema_version: Option<u32>,
    pub name: String,
    #[serde(default = "default_priority")]
    pub priority: u32,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_conditions")]
    pub when: RouteConditions,
    pub then: RouteAction,
}

fn default_priority() -> u32 {
    100
}

fn default_enabled() -> bool {
    true
}

fn default_conditions() -> RouteConditions {
    RouteConditions::default()
}

/// Canonicalize a gateway-shaped route object:
/// `{schema_version?, name, priority?, enabled?, when?, then}`.
///
/// This is intentionally a `Value` entrypoint. It lets callers report a
/// precise field path for type, unknown-field, legacy-version, and semantic
/// errors instead of accepting a partially-deserialized object.
pub fn canonicalize_route_value(value: Value) -> Result<CanonicalRoute, Vec<RouteValidationIssue>> {
    let envelope: RouteWriteRequest = match deserialize_with_path(value) {
        Ok(envelope) => envelope,
        Err(issue) => return Err(vec![issue]),
    };

    let version = envelope.schema_version.unwrap_or(ROUTE_SCHEMA_VERSION);
    if version != ROUTE_SCHEMA_VERSION {
        let code = if version < ROUTE_SCHEMA_VERSION {
            "legacy_schema_version"
        } else {
            "future_schema_version"
        };
        return Err(vec![RouteValidationIssue::new(
            "schema_version",
            code,
            format!(
                "route schema version {version} is not accepted; this gateway accepts {ROUTE_SCHEMA_VERSION} ({ROUTE_SCHEMA_ID})"
            ),
        )]);
    }

    let route = NewRoute {
        name: envelope.name,
        priority: envelope.priority,
        enabled: envelope.enabled,
        when: envelope.when,
        then: envelope.then,
    };

    let issues = validate_route_semantics(&route);
    if !issues.is_empty() {
        return Err(issues);
    }

    // These types contain only JSON-compatible fields. Keep errors explicit
    // rather than silently serializing a broken definition should that change.
    let conditions = match serde_json::to_value(&route.when) {
        Ok(value) => value,
        Err(error) => {
            return Err(vec![RouteValidationIssue::new(
                "when",
                "canonicalization_failed",
                error.to_string(),
            )])
        }
    };
    let target = match serde_json::to_value(&route.then) {
        Ok(value) => value,
        Err(error) => {
            return Err(vec![RouteValidationIssue::new(
                "then",
                "canonicalization_failed",
                error.to_string(),
            )])
        }
    };

    let canonical_hash = match canonical_hash(&route) {
        Ok(hash) => hash,
        Err(issue) => return Err(vec![issue]),
    };
    Ok(CanonicalRoute {
        schema_version: ROUTE_SCHEMA_VERSION,
        canonical_hash,
        route,
        conditions,
        target,
    })
}

/// Canonicalize the split representation used by the cloud `routes` table.
///
/// The contract itself intentionally uses `when`/`then`, matching the gateway
/// API and Rust types. This adapter preserves control-plane terminology in
/// validation responses by translating paths to `conditions`/`target`.
pub fn canonicalize_route_parts(
    schema_version: Option<u32>,
    name: String,
    priority: i32,
    enabled: bool,
    conditions: Value,
    target: Value,
) -> Result<CanonicalRoute, Vec<RouteValidationIssue>> {
    let value = serde_json::json!({
        "schema_version": schema_version,
        "name": name,
        "priority": priority,
        "enabled": enabled,
        "when": conditions,
        "then": target,
    });
    canonicalize_route_value(value).map_err(|issues| {
        issues
            .into_iter()
            .map(|mut issue| {
                issue.field = control_plane_field(&issue.field);
                issue
            })
            .collect()
    })
}

fn control_plane_field(field: &str) -> String {
    if field == "when" {
        "conditions".into()
    } else if let Some(suffix) = field.strip_prefix("when.") {
        format!("conditions.{suffix}")
    } else if field == "then" {
        "target".into()
    } else if let Some(suffix) = field.strip_prefix("then.") {
        format!("target.{suffix}")
    } else {
        field.into()
    }
}

fn deserialize_with_path<T>(value: Value) -> Result<T, RouteValidationIssue>
where
    T: DeserializeOwned,
{
    let bytes = serde_json::to_vec(&value)
        .map_err(|error| RouteValidationIssue::new("$", "invalid_json", error.to_string()))?;
    let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
    serde_path_to_error::deserialize(&mut deserializer).map_err(|error| {
        let path = error.path().to_string();
        RouteValidationIssue::new(
            if path.is_empty() || path == "." {
                "$".to_string()
            } else {
                path
            },
            "invalid_field",
            error.into_inner().to_string(),
        )
    })
}

fn validate_route_semantics(route: &NewRoute) -> Vec<RouteValidationIssue> {
    let mut issues = Vec::new();
    if route.name.trim().is_empty() {
        issues.push(RouteValidationIssue::new(
            "name",
            "blank_name",
            "route name must contain at least one non-whitespace character",
        ));
    }
    // `routes.priority` is a shared control-plane `INT` column. Accepting the
    // wider wire `u32` range here would let the Postgres adapter silently
    // normalize a definition that the caller believed it saved.
    if route.priority > i32::MAX as u32 {
        issues.push(RouteValidationIssue::new(
            "priority",
            "invalid_field",
            "priority exceeds the database-supported range",
        ));
    }

    validate_conditions(&route.when, &mut issues);
    validate_action(&route.then, &mut issues);
    issues
}

fn validate_conditions(conditions: &RouteConditions, issues: &mut Vec<RouteValidationIssue>) {
    validate_nonempty_strings(&conditions.model_in, "when.model_in", issues);
    validate_no_duplicates(&conditions.model_in, "when.model_in", issues);
    validate_nonempty_strings(
        &conditions.prompt_contains_any_of,
        "when.prompt_contains_any_of",
        issues,
    );
    validate_no_duplicates(
        &conditions.prompt_contains_any_of,
        "when.prompt_contains_any_of",
        issues,
    );

    if let Some(tag) = conditions.tag_equals.as_deref() {
        validate_nonempty(tag, "when.tag_equals", issues);
    }
    if let Some(content_type) = conditions.content_type.as_deref() {
        const CONTENT_TYPES: [&str; 6] = ["json", "csv", "log", "code", "diff", "prose"];
        if !CONTENT_TYPES.contains(&content_type) {
            issues.push(RouteValidationIssue::new(
                "when.content_type",
                "unsupported_content_type",
                format!(
                    "content_type must be one of {}; got {content_type:?}",
                    CONTENT_TYPES.join(", ")
                ),
            ));
        }
    }
    validate_nonnegative_f64(
        conditions.estimated_cost_gt,
        "when.estimated_cost_gt",
        issues,
    );
    validate_nonnegative_f64(
        conditions.estimated_cost_lt,
        "when.estimated_cost_lt",
        issues,
    );
}

fn validate_action(action: &RouteAction, issues: &mut Vec<RouteValidationIssue>) {
    if let Some(target) = action.target_model.as_deref() {
        validate_nonempty(target, "then.target_model", issues);
    }
    validate_nonempty_strings(&action.fallbacks, "then.fallbacks", issues);
    validate_no_duplicates(&action.fallbacks, "then.fallbacks", issues);
    if let Some(primary) = action.target_model.as_deref() {
        for (index, fallback) in action.fallbacks.iter().enumerate() {
            if primary == fallback {
                issues.push(RouteValidationIssue::new(
                    format!("then.fallbacks[{index}]"),
                    "duplicate_primary_fallback",
                    "a fallback must not repeat target_model",
                ));
            }
        }
    }
    validate_positive_f64(action.max_cost_usd, "then.max_cost_usd", issues);
    if action.traffic_pct.is_some_and(|percent| percent > 100) {
        issues.push(RouteValidationIssue::new(
            "then.traffic_pct",
            "traffic_pct_out_of_range",
            "traffic_pct must be between 0 and 100; values above 100 would be silently clamped at runtime",
        ));
    }
    if let Some(shadow) = action.shadow_model.as_deref() {
        validate_nonempty(shadow, "then.shadow_model", issues);
    }

    validate_existing_rule(
        validate_auto_pause(action),
        "then.pause_floor_pass_rate",
        issues,
    );
    validate_existing_rule(validate_output_shaping(action), "then", issues);
    // The control plane cannot know a gateway's live provider registry. Passing
    // `true` here still validates bounded local fields such as
    // `keep_recent_pairs`; the gateway performs provider-resolution validation
    // immediately before its own write path.
    validate_existing_rule(
        validate_agentic_budget(action, |_| true),
        "then.agentic_budget",
        issues,
    );
    validate_existing_rule(validate_panel(action), "then.panel", issues);
    validate_existing_rule(validate_workflow(action), "then.workflow", issues);
    validate_existing_rule(validate_route_has_effect(action), "then", issues);

    if let Some(agentic) = action.agentic_budget.as_ref() {
        if let Some(model) = agentic.route_mechanical_to.as_deref() {
            validate_nonempty(model, "then.agentic_budget.route_mechanical_to", issues);
        }
    }
    if let Some(panel) = action.panel.as_ref() {
        validate_nonempty_strings(&panel.members, "then.panel.members", issues);
        validate_no_duplicates(&panel.members, "then.panel.members", issues);
        if let Some(arbiter) = panel.arbiter.as_deref() {
            validate_nonempty(arbiter, "then.panel.arbiter", issues);
        }
        validate_positive_f64(panel.max_cost_usd, "then.panel.max_cost_usd", issues);
        if let Some(quorum) = panel.quorum {
            if quorum == 0 {
                issues.push(RouteValidationIssue::new(
                    "then.panel.quorum",
                    "nonpositive_quorum",
                    "quorum must be at least one",
                ));
            } else if panel.members.is_empty() {
                issues.push(RouteValidationIssue::new(
                    "then.panel.quorum",
                    "unbounded_panel_quorum",
                    "quorum requires explicit panel members; an environment default cannot be validated at save time",
                ));
            } else if quorum > panel.members.len() {
                issues.push(RouteValidationIssue::new(
                    "then.panel.quorum",
                    "quorum_exceeds_members",
                    format!(
                        "quorum {quorum} exceeds the {} configured panel members",
                        panel.members.len()
                    ),
                ));
            }
        }
    }
    if let Some(workflow) = action.workflow.as_ref() {
        validate_nonempty(
            workflow.workflow_id.as_str(),
            "then.workflow.workflow_id",
            issues,
        );
        validate_positive_f64(workflow.max_cost_usd, "then.workflow.max_cost_usd", issues);
    }
}

fn validate_existing_rule(
    result: Result<(), ValidationError>,
    default_field: &str,
    issues: &mut Vec<RouteValidationIssue>,
) {
    let Err(error) = result else {
        return;
    };
    let field = match &error {
        ValidationError::InvalidPauseFloor { .. } => "then.pause_floor_pass_rate",
        ValidationError::InvalidPauseMinVerdicts => "then.pause_min_verdicts",
        ValidationError::InvalidReasoningEffortCap { .. } => "then.reasoning_max_effort",
        ValidationError::InvalidThinkingBudgetCap { .. } => "then.reasoning_budget_tokens",
        ValidationError::InvalidFormatSwitch { .. } => "then.format_switch",
        ValidationError::OutputShapingConflict => "then",
        ValidationError::InvalidKeepRecentPairs => "then.agentic_budget.keep_recent_pairs",
        ValidationError::InvalidPanelStrategy(_) => "then.panel.strategy",
        ValidationError::InvalidWorkflowMode(_) => "then.workflow.mode",
        ValidationError::WorkflowConflict("panel") => "then.panel",
        ValidationError::WorkflowConflict("target_model") => "then.target_model",
        _ => default_field,
    };
    issues.push(RouteValidationIssue::new(
        field,
        "invalid_configuration",
        error.to_string(),
    ));
}

fn validate_nonempty(
    value: &str,
    field: impl Into<String>,
    issues: &mut Vec<RouteValidationIssue>,
) {
    if value.trim().is_empty() {
        issues.push(RouteValidationIssue::new(
            field,
            "blank_value",
            "value must contain at least one non-whitespace character",
        ));
    }
}

fn validate_nonempty_strings(
    values: &[String],
    prefix: &str,
    issues: &mut Vec<RouteValidationIssue>,
) {
    for (index, value) in values.iter().enumerate() {
        validate_nonempty(value, format!("{prefix}[{index}]"), issues);
    }
}

fn validate_no_duplicates(values: &[String], prefix: &str, issues: &mut Vec<RouteValidationIssue>) {
    let mut seen = std::collections::HashSet::new();
    for (index, value) in values.iter().enumerate() {
        if !seen.insert(value) {
            issues.push(RouteValidationIssue::new(
                format!("{prefix}[{index}]"),
                "duplicate_value",
                "duplicate values are not allowed because they obscure the evaluated route definition",
            ));
        }
    }
}

fn validate_nonnegative_f64(
    value: Option<f64>,
    field: &str,
    issues: &mut Vec<RouteValidationIssue>,
) {
    if value.is_some_and(|number| !number.is_finite() || number < 0.0) {
        issues.push(RouteValidationIssue::new(
            field,
            "negative_or_nonfinite_number",
            "value must be a finite number greater than or equal to zero",
        ));
    }
}

fn validate_positive_f64(value: Option<f64>, field: &str, issues: &mut Vec<RouteValidationIssue>) {
    if value.is_some_and(|number| !number.is_finite() || number <= 0.0) {
        issues.push(RouteValidationIssue::new(
            field,
            "nonpositive_or_nonfinite_number",
            "value must be a finite number greater than zero",
        ));
    }
}

fn canonical_hash(route: &NewRoute) -> Result<String, RouteValidationIssue> {
    #[derive(Serialize)]
    struct HashMaterial<'a> {
        schema: &'static str,
        schema_version: u32,
        name: &'a str,
        priority: u32,
        enabled: bool,
        when: &'a RouteConditions,
        then: &'a RouteAction,
    }

    let material = HashMaterial {
        schema: ROUTE_SCHEMA_ID,
        schema_version: ROUTE_SCHEMA_VERSION,
        name: &route.name,
        priority: route.priority,
        enabled: route.enabled,
        when: &route.when,
        then: &route.then,
    };
    let bytes = serde_json::to_vec(&material).map_err(|error| {
        RouteValidationIssue::new("$", "canonicalization_failed", error.to_string())
    })?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn canonicalizes_implicit_v1_and_explicit_defaults_to_one_identity() {
        let implicit = canonicalize_route_value(json!({
            "name": "downroute",
            "when": { "model_in": ["gpt-4o"] },
            "then": { "target_model": "gpt-4o-mini" }
        }))
        .expect("implicit v1 route is valid");
        let explicit = canonicalize_route_value(json!({
            "schema_version": 1,
            "name": "downroute",
            "priority": 100,
            "enabled": true,
            "when": { "model_in": ["gpt-4o"] },
            "then": { "target_model": "gpt-4o-mini", "fallbacks": [] }
        }))
        .expect("explicit v1 route is valid");

        assert_eq!(implicit.schema_version, ROUTE_SCHEMA_VERSION);
        assert_eq!(implicit.conditions, explicit.conditions);
        assert_eq!(implicit.target, explicit.target);
        assert_eq!(implicit.canonical_hash, explicit.canonical_hash);
    }

    #[test]
    fn rejects_unknown_nested_field_with_an_addressable_path() {
        let errors = canonicalize_route_value(json!({
            "name": "bad",
            "when": { "model_in": ["gpt-4o"], "future_match": true },
            "then": { "target_model": "gpt-4o-mini" }
        }))
        .expect_err("unknown fields cannot be silently ignored");

        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field, "when.future_match");
        assert_eq!(errors[0].code, "invalid_field");
    }

    #[test]
    fn rejects_legacy_and_future_schema_versions_explicitly() {
        for (version, code) in [(0, "legacy_schema_version"), (2, "future_schema_version")] {
            let errors = canonicalize_route_value(json!({
                "schema_version": version,
                "name": "bad-version",
                "then": { "target_model": "gpt-4o-mini" }
            }))
            .expect_err("non-v1 route is not silently coerced");
            assert_eq!(errors[0].field, "schema_version");
            assert_eq!(errors[0].code, code);
        }
    }

    #[test]
    fn rejects_runtime_clamps_and_unmatchable_condition_values() {
        let errors = canonicalize_route_value(json!({
            "name": "bad-semantics",
            "when": { "content_type": "binary" },
            "then": {
                "target_model": "gpt-4o-mini",
                "traffic_pct": 101
            }
        }))
        .expect_err("runtime clamp/no-match values are rejected at write time");

        assert!(errors
            .iter()
            .any(|error| error.field == "when.content_type"));
        assert!(errors.iter().any(|error| error.field == "then.traffic_pct"));
    }

    #[test]
    fn rejects_priorities_the_shared_postgres_column_cannot_represent() {
        let errors = canonicalize_route_value(json!({
            "name": "out-of-range-priority",
            "priority": i32::MAX as u64 + 1,
            "then": { "target_model": "gpt-4o-mini" }
        }))
        .expect_err("the canonical write must not rely on a storage clamp");

        assert!(errors.iter().any(|error| {
            error.field == "priority"
                && error.code == "invalid_field"
                && error.message == "priority exceeds the database-supported range"
        }));
    }

    #[test]
    fn split_control_plane_paths_remain_addressable() {
        let errors = canonicalize_route_parts(
            Some(ROUTE_SCHEMA_VERSION),
            "bad".into(),
            100,
            true,
            json!({ "unknown_condition": true }),
            json!({ "target_model": "gpt-4o-mini" }),
        )
        .expect_err("unknown cloud condition is rejected");

        assert_eq!(errors[0].field, "conditions.unknown_condition");
    }

    #[test]
    fn rejects_nonpositive_and_excessive_explicit_panel_quorum() {
        for (quorum, code) in [(0, "nonpositive_quorum"), (3, "quorum_exceeds_members")] {
            let errors = canonicalize_route_value(json!({
                "name": "unsafe-panel-quorum",
                "then": {
                    "panel": {
                        "strategy": "majority",
                        "members": ["member-a", "member-b"],
                        "quorum": quorum
                    }
                }
            }))
            .expect_err("unhonorable panel quorum must fail route validation");

            assert!(errors
                .iter()
                .any(|error| { error.field == "then.panel.quorum" && error.code == code }));
        }
    }
}
