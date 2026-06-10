//! Cost-control MCP tools: `get_spend_today`, `check_budget_remaining`,
//! `set_cost_limit`.
//!
//! These expose an org's cost posture so an agent stack can wire cost control
//! programmatically (read spend, check headroom, tighten a cap) instead of
//! waiting for a 402 at request time. They dispatch to a
//! [`CostControlBackend`](crate::cost::CostControlBackend); the public-repo
//! default backend is unconfigured and clearly marks its responses as such (no
//! fabricated numbers).
//!
//! ## Auth scoping
//!
//! Each tool is constructed with the **bound** `org_id` (the org resolved from
//! the operator's verified key at boot; design §8). Reads are always scoped to
//! that org. `set_cost_limit` is a mutation, so it additionally **rejects** any
//! caller-supplied `org_id` that does not match the bound org — a caller cannot
//! set a cost limit for a *different* tenant. This is the security boundary the
//! tests assert.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::cost::{CostControlBackend, LimitScope};
use crate::error::McpError;
use crate::protocol::ToolDef;
use crate::tools::Tool;

/// `get_spend_today` — current-day spend for the authenticated org.
pub struct GetSpendTodayTool {
    pub backend: Arc<dyn CostControlBackend>,
    /// The org bound from the operator's verified key. Reads are scoped to it.
    pub org_id: Uuid,
}

#[async_trait]
impl Tool for GetSpendTodayTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "get_spend_today",
            description: "Return the authenticated organization's spend so far \
                for the current day, in USD. Scoped to your own org — you cannot \
                query another organization. The `configured` field is false when \
                no cost backend is wired (in which case `spend_usd` is a \
                placeholder, not a real figure).",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, _params: Value) -> Result<Value, McpError> {
        let s = self.backend.spend_today(self.org_id).await?;
        Ok(serde_json::to_value(s).expect("SpendToday serializes"))
    }
}

/// `check_budget_remaining` — remaining headroom vs the org's monthly cap.
pub struct CheckBudgetRemainingTool {
    pub backend: Arc<dyn CostControlBackend>,
    pub org_id: Uuid,
}

#[async_trait]
impl Tool for CheckBudgetRemainingTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "check_budget_remaining",
            description: "Return the authenticated organization's remaining \
                monthly budget headroom (cap minus month-to-date spend), in USD. \
                Scoped to your own org. `remaining_usd` is null when the org is \
                uncapped. The `configured` field is false when no cost backend \
                is wired (the numeric fields are then placeholders).",
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, _params: Value) -> Result<Value, McpError> {
        let b = self.backend.budget_remaining(self.org_id).await?;
        Ok(serde_json::to_value(b).expect("BudgetRemaining serializes"))
    }
}

/// `set_cost_limit` — set/adjust a per-key or org-level monthly cost limit.
///
/// Mutating + auth-scoped: an optional `org_id` argument, if present, **must**
/// equal the bound org or the call fails closed with `unauthorized` (-32001).
pub struct SetCostLimitTool {
    pub backend: Arc<dyn CostControlBackend>,
    pub org_id: Uuid,
}

#[async_trait]
impl Tool for SetCostLimitTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "set_cost_limit",
            description: "Set or adjust a monthly cost limit (USD) for the \
                authenticated organization, either org-wide or for a specific \
                API key. Pass `monthly_cap_usd: null` to clear the cap. The \
                limit is always applied to YOUR org; an `org_id` argument, if \
                supplied, must match your own org or the call is rejected. \
                `applied` is false when no cost backend is wired (the change is \
                not persisted).",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "monthly_cap_usd": {
                        "type": ["number", "null"],
                        "description": "New monthly USD cap; null clears it."
                    },
                    "key_id": {
                        "type": "string",
                        "description": "Optional API key UUID to scope the cap to. Omit for an org-wide cap."
                    },
                    "org_id": {
                        "type": "string",
                        "description": "Optional; if present must equal your authenticated org. Provided only for explicitness — you cannot target another org."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, params: Value) -> Result<Value, McpError> {
        // Auth scoping: if the caller named an org, it must be *their* org.
        if let Some(req_org) = params.get("org_id") {
            // A non-null org_id must parse and match the bound org. A null is
            // treated as "unspecified" (allowed).
            if !req_org.is_null() {
                let req_org = req_org
                    .as_str()
                    .ok_or_else(|| McpError::InvalidParams("org_id must be a string".into()))?;
                let req_org = Uuid::parse_str(req_org)
                    .map_err(|e| McpError::InvalidParams(format!("org_id not a UUID: {e}")))?;
                if req_org != self.org_id {
                    return Err(McpError::Unauthorized(
                        "cannot set a cost limit for another organization".into(),
                    ));
                }
            }
        }

        // Optional per-key scope.
        let scope = match params.get("key_id") {
            Some(v) if !v.is_null() => {
                let k = v
                    .as_str()
                    .ok_or_else(|| McpError::InvalidParams("key_id must be a string".into()))?;
                let k = Uuid::parse_str(k)
                    .map_err(|e| McpError::InvalidParams(format!("key_id not a UUID: {e}")))?;
                LimitScope::Key(k)
            }
            _ => LimitScope::Org,
        };

        // The cap. Absent or explicit null = clear the cap.
        let monthly_cap_usd = match params.get("monthly_cap_usd") {
            Some(v) if !v.is_null() => Some(v.as_f64().ok_or_else(|| {
                McpError::InvalidParams("monthly_cap_usd must be a number".into())
            })?),
            _ => None,
        };
        if let Some(cap) = monthly_cap_usd {
            if !cap.is_finite() || cap < 0.0 {
                return Err(McpError::InvalidParams(
                    "monthly_cap_usd must be a finite, non-negative number".into(),
                ));
            }
        }

        let r = self
            .backend
            .set_cost_limit(self.org_id, scope, monthly_cap_usd)
            .await?;
        Ok(serde_json::to_value(r).expect("CostLimitSet serializes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::UnconfiguredBackend;

    fn backend() -> Arc<dyn CostControlBackend> {
        Arc::new(UnconfiguredBackend)
    }

    // ── schemas ──────────────────────────────────────────────────────────────

    #[test]
    fn all_three_schemas_are_valid_object_schemas() {
        let org = Uuid::now_v7();
        let defs = [
            GetSpendTodayTool {
                backend: backend(),
                org_id: org,
            }
            .def(),
            CheckBudgetRemainingTool {
                backend: backend(),
                org_id: org,
            }
            .def(),
            SetCostLimitTool {
                backend: backend(),
                org_id: org,
            }
            .def(),
        ];
        let names: Vec<_> = defs.iter().map(|d| d.name).collect();
        assert_eq!(
            names,
            [
                "get_spend_today",
                "check_budget_remaining",
                "set_cost_limit"
            ]
        );
        for d in &defs {
            assert_eq!(
                d.input_schema.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "{} inputSchema must be an object schema",
                d.name
            );
            assert!(
                d.input_schema.get("properties").is_some(),
                "{} must declare properties",
                d.name
            );
        }
    }

    #[test]
    fn set_cost_limit_schema_exposes_cap_key_and_org() {
        let tool = SetCostLimitTool {
            backend: backend(),
            org_id: Uuid::now_v7(),
        };
        let props = tool.def().input_schema["properties"].clone();
        assert!(props.get("monthly_cap_usd").is_some());
        assert!(props.get("key_id").is_some());
        assert!(props.get("org_id").is_some());
    }

    // ── dispatch to the backend trait ────────────────────────────────────────

    #[tokio::test]
    async fn get_spend_today_dispatches_to_backend_and_is_org_scoped() {
        let org = Uuid::now_v7();
        let tool = GetSpendTodayTool {
            backend: backend(),
            org_id: org,
        };
        let out = tool.call(json!({})).await.unwrap();
        assert_eq!(out["org_id"], org.to_string());
        assert_eq!(out["configured"], false);
    }

    #[tokio::test]
    async fn check_budget_remaining_dispatches_to_backend() {
        let org = Uuid::now_v7();
        let tool = CheckBudgetRemainingTool {
            backend: backend(),
            org_id: org,
        };
        let out = tool.call(json!({})).await.unwrap();
        assert_eq!(out["org_id"], org.to_string());
        assert_eq!(out["configured"], false);
        assert!(out["remaining_usd"].is_null());
    }

    #[tokio::test]
    async fn set_cost_limit_org_wide_dispatches() {
        let org = Uuid::now_v7();
        let tool = SetCostLimitTool {
            backend: backend(),
            org_id: org,
        };
        let out = tool.call(json!({ "monthly_cap_usd": 50.0 })).await.unwrap();
        assert_eq!(out["org_id"], org.to_string());
        assert!(out["key_id"].is_null(), "org-wide cap has no key_id");
        assert_eq!(out["monthly_cap_usd"], 50.0);
        assert_eq!(out["applied"], false);
    }

    #[tokio::test]
    async fn set_cost_limit_per_key_carries_key_scope() {
        let org = Uuid::now_v7();
        let key = Uuid::now_v7();
        let tool = SetCostLimitTool {
            backend: backend(),
            org_id: org,
        };
        let out = tool
            .call(json!({ "monthly_cap_usd": 10.0, "key_id": key.to_string() }))
            .await
            .unwrap();
        assert_eq!(out["key_id"], key.to_string());
    }

    #[tokio::test]
    async fn set_cost_limit_null_cap_clears() {
        let org = Uuid::now_v7();
        let tool = SetCostLimitTool {
            backend: backend(),
            org_id: org,
        };
        let out = tool.call(json!({ "monthly_cap_usd": null })).await.unwrap();
        assert!(out["monthly_cap_usd"].is_null());
    }

    // ── auth scoping: cannot target another org ──────────────────────────────

    #[tokio::test]
    async fn set_cost_limit_rejects_foreign_org() {
        let bound = Uuid::now_v7();
        let other = Uuid::now_v7();
        let tool = SetCostLimitTool {
            backend: backend(),
            org_id: bound,
        };
        let err = tool
            .call(json!({ "org_id": other.to_string(), "monthly_cap_usd": 1.0 }))
            .await
            .unwrap_err();
        assert!(
            matches!(err, McpError::Unauthorized(_)),
            "a caller must not set a limit for another org"
        );
        assert_eq!(err.code(), -32001);
    }

    #[tokio::test]
    async fn set_cost_limit_allows_matching_org() {
        let bound = Uuid::now_v7();
        let tool = SetCostLimitTool {
            backend: backend(),
            org_id: bound,
        };
        let out = tool
            .call(json!({ "org_id": bound.to_string(), "monthly_cap_usd": 1.0 }))
            .await
            .unwrap();
        assert_eq!(out["org_id"], bound.to_string());
    }

    #[tokio::test]
    async fn set_cost_limit_rejects_negative_cap() {
        let tool = SetCostLimitTool {
            backend: backend(),
            org_id: Uuid::now_v7(),
        };
        let err = tool
            .call(json!({ "monthly_cap_usd": -5.0 }))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::InvalidParams(_)));
    }

    #[tokio::test]
    async fn set_cost_limit_rejects_non_uuid_key() {
        let tool = SetCostLimitTool {
            backend: backend(),
            org_id: Uuid::now_v7(),
        };
        let err = tool
            .call(json!({ "key_id": "not-a-uuid", "monthly_cap_usd": 1.0 }))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::InvalidParams(_)));
    }
}
