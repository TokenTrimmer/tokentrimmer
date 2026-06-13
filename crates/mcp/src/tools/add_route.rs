//! `add_route` — a **write** MCP tool that creates a routing rule for the
//! authenticated org by forwarding a [`tt_routing::NewRoute`] to the gateway's
//! user-facing `POST /v1/routes` endpoint.
//!
//! This tool is **off by default**. It is only registered when the server is
//! booted with write-tools enabled (`--allow-write` / `TT_MCP_ALLOW_WRITE`); a
//! read-only deployment never lists it and `tools/call` on it returns
//! `MethodNotFound` (-32601), not `Unauthorized`. See
//! [`crate::server::Server::register_write_tools`].
//!
//! ## Auth scoping (no cross-org mutation)
//!
//! The tool is constructed with the **bound** `org_id` — the org resolved from
//! the operator's verified key at boot (design §8). It NEVER trusts a
//! caller-supplied `org_id`: an `org_id` argument, if present, must equal the
//! bound org or the call fails closed with `unauthorized` (-32001), mirroring
//! [`crate::tools::cost_control::SetCostLimitTool`]. The gateway re-derives the
//! org from the forwarded bearer token, so the org is enforced twice.
//!
//! ## Validate-before-network
//!
//! [`validate_capability`] + [`validate_shadow_model`] run against the embedded
//! model catalog BEFORE any HTTP call, so an obviously-wrong route (a vision
//! condition pointed at a text-only target, or an unresolvable `shadow_model`)
//! is rejected with `InvalidParams` (-32602) without spamming the gateway. The
//! gateway remains the final authority and re-validates against its live
//! provider registry.
//!
//! ## Idempotency
//!
//! Re-creating a route is treated as success: the gateway upserts on the route
//! id, so an id-collision (HTTP 409 / a 2xx upsert) is reported as `created:
//! true` rather than a hard error — a retried `add_route` does not fail.

use async_trait::async_trait;
use reqwest::StatusCode;
use serde_json::{json, Value};
use tt_routing::{
    validate_agentic_budget, validate_capability, validate_output_shaping, validate_shadow_model,
    NewRoute,
};
use tt_shared::model_catalog::model_catalog;
use uuid::Uuid;

use crate::error::McpError;
use crate::protocol::ToolDef;
use crate::tools::Tool;

/// `add_route` — create a routing rule for the authenticated org.
pub struct AddRouteTool {
    /// Gateway base URL (e.g. `https://api.tokentrimmer.com`). The tool POSTs to
    /// `{gateway_base}/v1/routes`.
    pub gateway_base: String,
    /// The operator's bearer token, forwarded to the gateway. The gateway
    /// derives the org from THIS key — the only org that can be written.
    pub api_key: String,
    /// Outbound HTTP client (shared via the server state).
    pub http: reqwest::Client,
    /// The org bound from the operator's verified key. A caller-supplied
    /// `org_id` must match this or the call is rejected.
    pub org_id: Uuid,
}

impl AddRouteTool {
    /// Parse the `arguments` object into a [`NewRoute`], rejecting a
    /// caller-supplied `org_id` that does not match the bound org. The
    /// `org_id` field (if present) is stripped before deserialization — it is
    /// an auth assertion, not part of `NewRoute`.
    fn parse_new_route(&self, params: &Value) -> Result<NewRoute, McpError> {
        let mut obj = params
            .as_object()
            .cloned()
            .ok_or_else(|| McpError::InvalidParams("arguments must be an object".into()))?;

        // Auth scoping: if the caller named an org, it must be *their* org.
        if let Some(req_org) = obj.remove("org_id") {
            if !req_org.is_null() {
                let req_org = req_org
                    .as_str()
                    .ok_or_else(|| McpError::InvalidParams("org_id must be a string".into()))?;
                let req_org = Uuid::parse_str(req_org)
                    .map_err(|e| McpError::InvalidParams(format!("org_id not a UUID: {e}")))?;
                if req_org != self.org_id {
                    return Err(McpError::Unauthorized(
                        "cannot create a route for another organization".into(),
                    ));
                }
            }
        }

        serde_json::from_value::<NewRoute>(Value::Object(obj))
            .map_err(|e| McpError::InvalidParams(format!("invalid route spec: {e}")))
    }
}

#[async_trait]
impl Tool for AddRouteTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "add_route",
            description: "Create a routing rule for YOUR authenticated organization \
                (write tool; available only when the server is booted with write \
                access). The route is always applied to your own org; an `org_id` \
                argument, if supplied, must match your authenticated org or the \
                call is rejected — you cannot target another organization. The \
                route is structurally validated locally (vision-capability of the \
                target, resolvability of any `shadow_model`) before it is sent; \
                the gateway re-validates against its live provider registry and \
                is the final authority. Re-creating an existing route id \
                idempotently succeeds.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Human-readable route name."
                    },
                    "priority": {
                        "type": "integer",
                        "default": 100,
                        "description": "Higher value wins on tie-break; evaluated descending."
                    },
                    "enabled": {
                        "type": "boolean",
                        "default": true,
                        "description": "Disabled routes never match."
                    },
                    "when": {
                        "type": "object",
                        "description": "AND-ed match conditions (model_in, input_tokens_lt/gt, tag_equals, has_images, has_audio, prompt_contains_any_of, estimated_cost_gt/lt, upstream_latency_ms_p95_gt). Empty matches everything."
                    },
                    "then": {
                        "type": "object",
                        "description": "Action on match. Requires target_model; optional fallbacks, disable_cache, max_cost_usd, flex, compress, redact, traffic_pct, shadow_model.",
                        "properties": {
                            "target_model": { "type": "string" }
                        },
                        "required": ["target_model"]
                    },
                    "org_id": {
                        "type": "string",
                        "description": "Optional; if present must equal your authenticated org. Provided only for explicitness — you cannot target another org."
                    }
                },
                "required": ["name", "then"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, params: Value) -> Result<Value, McpError> {
        let spec = self.parse_new_route(&params)?;

        // Validate-before-network: catch obviously-wrong routes locally so we
        // don't spam the gateway. The local catalog is a best-effort proxy for
        // the gateway's live registry, which remains the final authority.
        let catalog = model_catalog();
        validate_capability(&spec.when, &spec.then, |m| {
            catalog.all().iter().find(|x| x.id == m).cloned()
        })
        .map_err(|e| McpError::InvalidParams(e.to_string()))?;
        validate_shadow_model(&spec.then, |m| catalog.all().iter().any(|x| x.id == m))
            .map_err(|e| McpError::InvalidParams(e.to_string()))?;
        // Task 2: reject a misconfigured `agentic_budget` (unresolvable
        // `route_mechanical_to` / `keep_recent_pairs == 0`) locally too; the
        // gateway's POST /v1/routes re-validates as the final authority.
        validate_agentic_budget(&spec.then, |m| catalog.all().iter().any(|x| x.id == m))
            .map_err(|e| McpError::InvalidParams(e.to_string()))?;
        // Phase 3.1/3.2: reject malformed output-shaping caps locally (the
        // gateway's POST /v1/routes re-validates as the final authority).
        validate_output_shaping(&spec.then).map_err(|e| McpError::InvalidParams(e.to_string()))?;

        let url = format!("{}/v1/routes", self.gateway_base.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&spec)
            .send()
            .await
            .map_err(|e| McpError::Internal(format!("gateway request failed: {e}")))?;

        let status = resp.status();

        // Idempotent on id collision: a 409 (or any 2xx) means the route exists
        // / was upserted — report success rather than a hard error so a retried
        // add_route does not fail.
        if status == StatusCode::CONFLICT {
            return Ok(json!({
                "created": false,
                "idempotent": true,
                "message": "route already exists (id collision treated as success)"
            }));
        }

        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            // A 4xx is a client/validation fault (bad params); a 5xx is an
            // upstream fault. Map 4xx → InvalidParams (-32602), else Internal
            // (-32603). 401/403 collapse to Unauthorized (-32001).
            return Err(map_gateway_error(status, &body));
        }

        let route: Value = resp
            .json()
            .await
            .map_err(|e| McpError::Internal(format!("decode gateway response: {e}")))?;
        Ok(json!({ "created": true, "route": route }))
    }
}

/// Map a non-2xx gateway response to the right [`McpError`]. 401/403 →
/// `Unauthorized`; other 4xx → `InvalidParams` (client/validation fault); 5xx
/// and anything else → `Internal`.
fn map_gateway_error(status: StatusCode, body: &str) -> McpError {
    let trimmed = body.trim();
    let detail = if trimmed.is_empty() {
        String::new()
    } else {
        format!(": {trimmed}")
    };
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            McpError::Unauthorized(format!("gateway rejected the operator key ({status})"))
        }
        s if s.is_client_error() => {
            McpError::InvalidParams(format!("gateway rejected the route ({status}){detail}"))
        }
        s => McpError::Internal(format!("gateway error ({s}){detail}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn tool(base: String, org: Uuid) -> AddRouteTool {
        AddRouteTool {
            gateway_base: base,
            api_key: "tt_live_operator".into(),
            http: reqwest::Client::new(),
            org_id: org,
        }
    }

    fn valid_args() -> Value {
        json!({
            "name": "all->cheap",
            "then": { "target_model": "gpt-4o-mini" }
        })
    }

    // ── schema ───────────────────────────────────────────────────────────────

    #[test]
    fn def_is_object_schema_named_add_route_requiring_then() {
        let d = tool("http://unused".into(), Uuid::now_v7()).def();
        assert_eq!(d.name, "add_route");
        assert_eq!(
            d.input_schema.get("type").and_then(|v| v.as_str()),
            Some("object")
        );
        let req = d
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .unwrap();
        assert!(req.iter().any(|v| v.as_str() == Some("then")));
        assert!(req.iter().any(|v| v.as_str() == Some("name")));
    }

    // ── happy path: posts the right NewRoute body ────────────────────────────

    #[tokio::test]
    async fn posts_new_route_body_and_returns_created_route() {
        let server = MockServer::start_async().await;
        let route_id = Uuid::now_v7();
        let m = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/v1/routes")
                    .header("authorization", "Bearer tt_live_operator")
                    .json_body_includes(
                        r#"{ "name": "all->cheap", "then": { "target_model": "gpt-4o-mini" } }"#
                            .to_string(),
                    );
                then.status(201).json_body(json!({
                    "id": route_id.to_string(),
                    "name": "all->cheap",
                    "priority": 100,
                    "enabled": true,
                    "when": {},
                    "then": { "target_model": "gpt-4o-mini" }
                }));
            })
            .await;

        let out = tool(server.base_url(), Uuid::now_v7())
            .call(valid_args())
            .await
            .unwrap();
        m.assert_async().await;
        assert_eq!(out["created"], true);
        assert_eq!(out["route"]["id"], route_id.to_string());
        assert_eq!(out["route"]["name"], "all->cheap");
    }

    // ── validate-before-network: bad params never hit the gateway ────────────

    #[tokio::test]
    async fn unresolvable_shadow_model_rejected_before_network() {
        let server = MockServer::start_async().await;
        // Any call to the gateway would be a contract violation.
        let m = server
            .mock_async(|when, then| {
                when.method(POST).path("/v1/routes");
                then.status(201).body("{}");
            })
            .await;

        let err = tool(server.base_url(), Uuid::now_v7())
            .call(json!({
                "name": "shadow",
                "then": { "target_model": "gpt-4o-mini", "shadow_model": "does-not-exist-xyz" }
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::InvalidParams(_)));
        assert_eq!(err.code(), -32602);
        // The gateway must NOT have been called.
        assert_eq!(m.calls_async().await, 0);
    }

    #[tokio::test]
    async fn vision_condition_on_text_target_rejected_before_network() {
        let server = MockServer::start_async().await;
        let m = server
            .mock_async(|when, then| {
                when.method(POST).path("/v1/routes");
                then.status(201).body("{}");
            })
            .await;

        // `o3` is a real catalog model that is text-only (no Vision capability);
        // a `has_images` condition needs a vision-capable target, so local
        // validation rejects it BEFORE any network call.
        let err = tool(server.base_url(), Uuid::now_v7())
            .call(json!({
                "name": "imgs",
                "when": { "has_images": true },
                "then": { "target_model": "o3" }
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::InvalidParams(_)));
        assert_eq!(err.code(), -32602);
        // The gateway must NOT have been called.
        assert_eq!(m.calls_async().await, 0);
    }

    #[tokio::test]
    async fn malformed_spec_missing_target_model_rejected() {
        let err = tool("http://unused".into(), Uuid::now_v7())
            .call(json!({ "name": "x", "then": {} }))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::InvalidParams(_)));
    }

    // ── idempotency on id collision ──────────────────────────────────────────

    #[tokio::test]
    async fn id_collision_409_is_idempotent_success() {
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/v1/routes");
                then.status(409).body("route id already exists");
            })
            .await;
        let out = tool(server.base_url(), Uuid::now_v7())
            .call(valid_args())
            .await
            .unwrap();
        assert_eq!(out["created"], false);
        assert_eq!(out["idempotent"], true);
    }

    // ── error mapping ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn gateway_422_maps_to_invalid_params() {
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/v1/routes");
                then.status(422).body("validation failed");
            })
            .await;
        let err = tool(server.base_url(), Uuid::now_v7())
            .call(valid_args())
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::InvalidParams(_)));
        assert_eq!(err.code(), -32602);
    }

    #[tokio::test]
    async fn gateway_401_maps_to_unauthorized() {
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/v1/routes");
                then.status(401).body("bad key");
            })
            .await;
        let err = tool(server.base_url(), Uuid::now_v7())
            .call(valid_args())
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Unauthorized(_)));
        assert_eq!(err.code(), -32001);
    }

    #[tokio::test]
    async fn gateway_500_maps_to_internal() {
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/v1/routes");
                then.status(500).body("boom");
            })
            .await;
        let err = tool(server.base_url(), Uuid::now_v7())
            .call(valid_args())
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Internal(_)));
        assert_eq!(err.code(), -32603);
    }

    #[tokio::test]
    async fn connectivity_fault_maps_to_internal_not_panic() {
        // Point at a closed port: the connection fails, must map to Internal.
        let err = tool("http://127.0.0.1:1".into(), Uuid::now_v7())
            .call(valid_args())
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Internal(_)));
    }

    // ── org scoping ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn rejects_foreign_org_id_before_network() {
        let server = MockServer::start_async().await;
        let m = server
            .mock_async(|when, then| {
                when.method(POST).path("/v1/routes");
                then.status(201).body("{}");
            })
            .await;
        let bound = Uuid::now_v7();
        let other = Uuid::now_v7();
        let err = tool(server.base_url(), bound)
            .call(json!({
                "name": "x",
                "org_id": other.to_string(),
                "then": { "target_model": "gpt-4o-mini" }
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::Unauthorized(_)));
        assert_eq!(err.code(), -32001);
        // Must not have touched the gateway.
        assert_eq!(m.calls_async().await, 0);
    }

    #[tokio::test]
    async fn allows_matching_org_id() {
        let server = MockServer::start_async().await;
        let bound = Uuid::now_v7();
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/v1/routes");
                then.status(201).json_body(json!({
                    "id": Uuid::now_v7().to_string(),
                    "name": "x",
                    "priority": 100,
                    "enabled": true,
                    "when": {},
                    "then": { "target_model": "gpt-4o-mini" }
                }));
            })
            .await;
        let out = tool(server.base_url(), bound)
            .call(json!({
                "name": "x",
                "org_id": bound.to_string(),
                "then": { "target_model": "gpt-4o-mini" }
            }))
            .await
            .unwrap();
        assert_eq!(out["created"], true);
    }
}
