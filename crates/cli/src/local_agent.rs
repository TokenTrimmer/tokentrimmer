//! Local official-SDK coding-agent supervisor.
//!
//! The vendor runtime owns its local authentication and talks directly to its
//! vendor. TokenTrimmer supplies only a loopback MCP capability broker, applies
//! the resolved repository policy outside the model process, and emits complete
//! run evidence. No vendor credential value is read or serialized here.

use std::{
    collections::BTreeMap,
    io::{self, Read as _},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tt_client::ToolExecutor as _;
use tt_mcp::{
    protocol::ToolDef, tools::Tool as McpTool, transport::http::run_on_listener, McpError, Server,
};
use tt_shared::agent_cost::{
    AgentCostBasis, AgentCostComponent, AgentCostPurpose, AgentRunCostEvidence,
    ExpectedAgentCostBasis, SubscriptionQuotaEvidence, SubscriptionQuotaUnit as CostQuotaUnit,
    UnmeasuredCostReason, UnmeasuredCostReasonCode, AGENT_COST_SCHEMA_VERSION,
};
use uuid::Uuid;

use crate::{
    agent_policy::{
        AgentRunner, ApprovalGate, PolicyCostBasis, ResolvedAgentPolicy, SubscriptionQuotaUnit,
    },
    execution_broker::{BrokerEvidence, LocalExecutionBroker},
};

pub const LOCAL_AGENT_RUN_SCHEMA_VERSION: u32 = 1;
const BRIDGE_OUTPUT_HARD_CAP: u64 = 16 * 1024 * 1024;
const BRIDGE_PROBE_OUTPUT_CAP: usize = 64 * 1024;
const MAX_HASHED_EXECUTABLE_BYTES: u64 = 512 * 1024 * 1024;
const CODEX_SDK_VERSION: &str = "0.147.0";
const CODEX_RUNTIME_VERSION: &str = "0.147.0";
const CLAUDE_SDK_VERSION: &str = "0.3.233";
const CLAUDE_RUNTIME_VERSION: &str = "2.1.233";

#[derive(Debug, Clone)]
pub struct LocalAgentRequest {
    pub repository: PathBuf,
    pub prompt: String,
    pub runner: AgentRunner,
    /// Exact provider-qualified model id admitted by policy, for example
    /// `openai/gpt-5.3-codex` or `anthropic/claude-sonnet-4-5`.
    pub model: String,
    pub session_id: Option<String>,
    pub plan_reference: String,
    pub marginal_cash_micros: u64,
    pub allocated_plan_micros: Option<u64>,
    pub bridge_path: Option<PathBuf>,
    pub vendor_executable: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalRunStatus {
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalStopReason {
    Completed,
    Cancelled,
    PolicyDenied,
    VendorError,
    WallTimeExceeded,
    QuotaExceeded,
    ValidationFailed,
    EvidenceIncomplete,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActionEvidence {
    pub model_actions_requested: u32,
    pub model_actions_completed: u32,
    pub model_actions_denied: u32,
    pub validation_actions_completed: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApprovalEvidence {
    pub destructive_operations: ApprovalGate,
    pub rollback: ApprovalGate,
    pub one_use_approvals_consumed: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackStatus {
    NotRequiredSourceNeverMutated,
}

#[derive(Debug, Clone, Serialize)]
pub struct CleanupEvidence {
    pub source_checkout_modified: bool,
    pub isolated_workspace_cleanup_completed_on_return: bool,
    pub rollback: RollbackStatus,
}

#[derive(Debug, Clone, Serialize)]
pub struct CostCoverageEvidence {
    pub total_components: usize,
    pub measured_components: usize,
    pub unmeasured_components: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptIneligibilityReason {
    LocalSigningUnavailable,
    SubscriptionNotInvoiceReconciled,
    RunFailed,
    CostEvidenceUnmeasured,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReceiptEvidence {
    pub eligible: bool,
    pub minted: bool,
    pub reasons: Vec<ReceiptIneligibilityReason>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalAgentReport {
    pub schema_version: u32,
    pub run_id: String,
    pub status: LocalRunStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub runner: AgentRunner,
    pub provider: String,
    pub model: String,
    pub vendor_session_id: Option<String>,
    pub response: String,
    pub failure: Option<String>,
    pub stop_reason: LocalStopReason,
    pub actions: ActionEvidence,
    pub approvals: ApprovalEvidence,
    pub cleanup: CleanupEvidence,
    pub runtime: VendorRuntimeEvidence,
    pub usage: VendorUsage,
    pub quota_checks: Vec<QuotaCheck>,
    pub cost: AgentRunCostEvidence,
    pub cost_coverage: CostCoverageEvidence,
    pub receipt: ReceiptEvidence,
    pub policy_sha256: String,
    pub policy_layers: Vec<crate::agent_policy::PolicyLayerEvidence>,
    pub validation: Vec<ValidationEvidence>,
    pub broker: BrokerEvidence,
}

#[derive(Debug, Clone, Serialize)]
pub struct VendorRuntimeEvidence {
    pub sdk_package: String,
    pub sdk_version: String,
    pub adapter_transport: &'static str,
    pub runtime_name: String,
    pub runtime_version: String,
    pub runtime_source: &'static str,
    pub runtime_launcher: String,
    pub runtime_launcher_sha256: String,
    pub runtime_executable: String,
    pub runtime_executable_sha256: String,
    pub node_version: String,
    pub node_executable: String,
    pub node_sha256: String,
    pub bridge_sha256: String,
    pub dependency_lock_sha256: String,
    pub authentication_method: String,
    pub credentials_observed_by_tokentrimmer: bool,
    pub model_tool_boundary: &'static str,
    pub max_turn_enforcement: &'static str,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VendorUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub cache_write_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_output_tokens: u64,
}

impl VendorUsage {
    fn total_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_write_input_tokens)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct QuotaCheck {
    pub unit: SubscriptionQuotaUnit,
    pub used: Option<u64>,
    pub limit: u64,
    pub source: &'static str,
    pub within_limit: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ValidationEvidence {
    pub executable: String,
    pub args: Vec<String>,
    pub passed: bool,
    pub result: Value,
}

#[derive(Debug, Error)]
pub enum LocalAgentError {
    #[error("local agent policy denied {field}: {detail}")]
    PolicyDenied { field: &'static str, detail: String },
    #[error("local execution broker failed: {0}")]
    Broker(#[from] crate::execution_broker::BrokerError),
    #[error("MCP broker server failed: {0}")]
    Mcp(#[from] McpError),
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("vendor SDK bridge probe failed: {0}")]
    BridgeProbe(String),
    #[error("vendor SDK bridge protocol failed: {0}")]
    BridgeProtocol(String),
    #[error("failed to serialize local agent request: {0}")]
    Serialize(#[from] serde_json::Error),
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BridgeRequest<'a> {
    runner: &'static str,
    prompt: &'a str,
    cwd: &'a Path,
    model: &'a str,
    max_turns: u32,
    max_output_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_budget_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    executable_path: Option<&'a Path>,
    mcp: BridgeMcpRequest<'a>,
}

#[derive(Debug, Serialize)]
struct BridgeMcpRequest<'a> {
    url: String,
    token: &'a str,
    tools: Vec<&'static str>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeResponse {
    ok: bool,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    response: String,
    #[serde(default)]
    usage: Option<VendorUsage>,
    #[serde(default)]
    total_cost_usd: Option<f64>,
    #[serde(default)]
    tool_calls: u64,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeProbe {
    ok: bool,
    node_version: String,
    compatibility: BTreeMap<String, BridgeCompatibility>,
    runtime: BridgeRuntimeProbe,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BridgeRuntimeProbe {
    launcher_path: PathBuf,
    executable_path: PathBuf,
    version: String,
    authenticated: bool,
    authentication_method: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BridgeCompatibility {
    sdk_version: String,
    runtime: String,
    runtime_version: String,
}

struct BrokerMcpTool {
    name: &'static str,
    description: &'static str,
    input_schema: Value,
    broker: Arc<LocalExecutionBroker>,
    calls: Arc<AtomicU32>,
    successful_calls: Arc<AtomicU32>,
    max_calls: u32,
    failure_tx: mpsc::UnboundedSender<String>,
}

#[tt_client::async_trait]
impl McpTool for BrokerMcpTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: self.name,
            description: self.description,
            input_schema: self.input_schema.clone(),
        }
    }

    async fn call(&self, params: Value) -> Result<Value, McpError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst).saturating_add(1);
        if call > self.max_calls {
            let detail = format!(
                "observed MCP calls reached {call}; conservative agent-turn ceiling is {}",
                self.max_calls
            );
            let _ = self.failure_tx.send(detail.clone());
            return Err(McpError::InvalidParams(detail));
        }

        let arguments = serde_json::to_string(&params)
            .map_err(|error| McpError::InvalidParams(error.to_string()))?;
        match self.broker.call(self.name, &arguments).await {
            Ok(text) => {
                self.successful_calls.fetch_add(1, Ordering::SeqCst);
                let structured = serde_json::from_str(&text).unwrap_or(Value::String(text.clone()));
                Ok(json!({
                    "content": [{ "type": "text", "text": text }],
                    "structuredContent": structured,
                    "isError": false
                }))
            }
            Err(error) => {
                let detail = error.to_string();
                let _ = self.failure_tx.send(detail.clone());
                Err(McpError::InvalidParams(detail))
            }
        }
    }
}

pub async fn run_local_agent(
    request: LocalAgentRequest,
    policy: &ResolvedAgentPolicy,
) -> Result<LocalAgentReport, LocalAgentError> {
    let started_at = Utc::now();
    let started = Instant::now();
    let admitted = admit(&request, policy)?;
    let bridge_path = resolve_bridge_path(request.bridge_path.as_deref())?;
    let lock_path = bridge_path
        .parent()
        .expect("canonical bridge has a parent")
        .join("package-lock.json");
    let lock_path = canonical_regular_file(&lock_path, "open dependency lock")?;
    let node_path = resolve_node()?;
    let bridge_sha256 = hash_regular_file(&bridge_path, 2 * 1024 * 1024)?;
    let dependency_lock_sha256 = hash_regular_file(&lock_path, 8 * 1024 * 1024)?;
    let node_sha256 = hash_regular_file(&node_path, MAX_HASHED_EXECUTABLE_BYTES)?;
    let probe = probe_bridge(
        &node_path,
        &bridge_path,
        admitted.bridge_runner,
        request.vendor_executable.as_deref(),
    )
    .await?;
    let runtime = verify_probe(
        request.runner,
        admitted.bridge_runner,
        probe,
        &node_path,
        node_sha256,
        bridge_sha256,
        dependency_lock_sha256,
        request.vendor_executable.is_some(),
    )?;

    let run_id = Uuid::now_v7().to_string();
    let broker = Arc::new(LocalExecutionBroker::new(
        &request.repository,
        &run_id,
        policy,
    )?);
    let workspace = broker.workspace_path().await;
    let definitions = LocalExecutionBroker::tool_definitions(policy);
    if definitions.is_empty() {
        return Err(denied(
            "tools",
            "resolved policy exposes no repository capability",
        ));
    }
    let max_tool_calls = admitted.max_observed_tool_calls;
    if max_tool_calls == 0 {
        return Err(denied(
            "limits.max_model_turns",
            "at least two admitted model/API turns are required when tools are exposed",
        ));
    }

    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|source| LocalAgentError::Io {
            operation: "bind loopback MCP broker",
            path: "127.0.0.1:0".into(),
            source,
        })?;
    let address = listener
        .local_addr()
        .map_err(|source| LocalAgentError::Io {
            operation: "inspect loopback MCP broker",
            path: "127.0.0.1:0".into(),
            source,
        })?;
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let (failure_tx, mut failure_rx) = mpsc::unbounded_channel();
    let calls = Arc::new(AtomicU32::new(0));
    let successful_calls = Arc::new(AtomicU32::new(0));
    let mut server = Server::new();
    let mut tool_names = Vec::with_capacity(definitions.len());
    for definition in definitions {
        let (name, description) = broker_tool_metadata(&definition.function.name)
            .ok_or_else(|| denied("tools", "broker exposed an unknown tool definition"))?;
        tool_names.push(name);
        server.tools.register(Box::new(BrokerMcpTool {
            name,
            description,
            input_schema: definition.function.parameters,
            broker: Arc::clone(&broker),
            calls: Arc::clone(&calls),
            successful_calls: Arc::clone(&successful_calls),
            max_calls: max_tool_calls,
            failure_tx: failure_tx.clone(),
        }));
    }
    drop(failure_tx);

    let shutdown = CancellationToken::new();
    let shutdown_server = shutdown.clone();
    let mut server_task = tokio::spawn(run_on_listener(
        server,
        listener,
        token.clone(),
        async move { shutdown_server.cancelled().await },
    ));

    let output_cap = policy
        .policy
        .process
        .max_output_bytes
        .clamp(1, BRIDGE_OUTPUT_HARD_CAP);
    let bridge_request = BridgeRequest {
        runner: admitted.bridge_runner,
        prompt: &request.prompt,
        cwd: &workspace,
        model: admitted.vendor_model,
        max_turns: policy.policy.limits.max_model_turns,
        max_output_bytes: output_cap,
        max_budget_usd: admitted.max_budget_usd,
        session_id: request.session_id.as_deref(),
        executable_path: request.vendor_executable.as_deref(),
        mcp: BridgeMcpRequest {
            url: format!("http://127.0.0.1:{}/mcp", address.port()),
            token: &token,
            tools: tool_names,
        },
    };
    let request_bytes = serde_json::to_vec(&bridge_request)?;
    let remaining = Duration::from_secs(policy.policy.limits.max_wall_time_seconds)
        .checked_sub(started.elapsed())
        .filter(|value| !value.is_zero())
        .ok_or_else(|| {
            denied(
                "limits.max_wall_time_seconds",
                "expired before vendor launch",
            )
        })?;

    let bridge_execution = run_bridge_process(
        &node_path,
        &bridge_path,
        &workspace,
        &request_bytes,
        remaining,
        output_cap,
        &mut failure_rx,
        &mut server_task,
    )
    .await;
    shutdown.cancel();
    if !server_task.is_finished() {
        let _ = server_task.await;
    }

    let mut failure = None;
    let mut stop_reason = LocalStopReason::Completed;
    let mut response = None;
    match bridge_execution {
        Ok(value) => {
            if !value.ok {
                failure = Some(
                    value
                        .error
                        .clone()
                        .unwrap_or_else(|| "vendor SDK reported failure".into()),
                );
                stop_reason = LocalStopReason::VendorError;
            }
            response = Some(value);
        }
        Err(error) => {
            stop_reason = classify_terminal_error(&error);
            failure = Some(error);
        }
    }

    let mut validation = Vec::new();
    if failure.is_none() {
        for command in &policy.policy.validation.required_commands {
            let arguments = serde_json::to_string(&json!({
                "executable": command.executable,
                "args": command.args,
            }))?;
            let result = broker
                .call(crate::execution_broker::RUN_COMMAND_TOOL, &arguments)
                .await;
            let (passed, value) = match result {
                Ok(raw) => {
                    let value: Value = serde_json::from_str(&raw)?;
                    let passed = value.get("exit_code").and_then(Value::as_i64) == Some(0)
                        && value.get("timed_out").and_then(Value::as_bool) == Some(false)
                        && value.get("output_limit_exceeded").and_then(Value::as_bool)
                            == Some(false);
                    (passed, value)
                }
                Err(error) => (false, json!({ "error": error.to_string() })),
            };
            validation.push(ValidationEvidence {
                executable: command.executable.clone(),
                args: command.args.clone(),
                passed,
                result: value,
            });
            if !passed {
                failure = Some(format!(
                    "required validation failed: {} {}",
                    command.executable,
                    command.args.join(" ")
                ));
                stop_reason = LocalStopReason::ValidationFailed;
                if policy.policy.validation.stop_on_regression {
                    break;
                }
            }
        }
    }

    let usage = response
        .as_ref()
        .and_then(|value| value.usage.clone())
        .unwrap_or_default();
    let attempted_tool_calls = u64::from(calls.load(Ordering::SeqCst));
    let quota_checks = settle_quotas(policy, &usage, attempted_tool_calls);
    if let Some(exceeded) = quota_checks.iter().find(|check| !check.within_limit) {
        failure = Some(format!(
            "subscription quota exceeded: {:?} used {:?}, limit {}",
            exceeded.unit, exceeded.used, exceeded.limit
        ));
        stop_reason = LocalStopReason::QuotaExceeded;
    }
    if let Some(value) = &response {
        if value.tool_calls > attempted_tool_calls {
            failure = Some(format!(
                "vendor reported {} tool calls but broker observed only {attempted_tool_calls}",
                value.tool_calls
            ));
            stop_reason = LocalStopReason::EvidenceIncomplete;
        }
    }
    if response.as_ref().is_some_and(|value| value.usage.is_none())
        && !policy.policy.budgets.allow_unmeasured
    {
        failure = Some("vendor SDK omitted usage while unmeasured cost is prohibited".into());
        stop_reason = LocalStopReason::EvidenceIncomplete;
    }

    let cost = build_cost_evidence(
        &run_id,
        &request,
        admitted.vendor,
        response.as_ref(),
        &quota_checks,
    )?;
    let broker_evidence = broker.evidence().await?;
    let status = if failure.is_none() {
        LocalRunStatus::Completed
    } else {
        LocalRunStatus::Failed
    };
    let cost_coverage = CostCoverageEvidence {
        total_components: cost.components.len(),
        measured_components: cost.measured_components(),
        unmeasured_components: cost.unmeasured_components(),
    };
    let receipt = receipt_evidence(status, &cost);
    let model_actions_requested = calls.load(Ordering::SeqCst);
    let model_actions_completed = successful_calls.load(Ordering::SeqCst);
    let actions = ActionEvidence {
        model_actions_requested,
        model_actions_completed,
        model_actions_denied: model_actions_requested.saturating_sub(model_actions_completed),
        validation_actions_completed: validation.len(),
    };
    let approvals = ApprovalEvidence {
        destructive_operations: policy.policy.approvals.destructive_operations,
        rollback: policy.policy.approvals.rollback,
        one_use_approvals_consumed: 0,
    };
    let cleanup = CleanupEvidence {
        source_checkout_modified: broker_evidence.source_checkout_modified,
        isolated_workspace_cleanup_completed_on_return: true,
        rollback: RollbackStatus::NotRequiredSourceNeverMutated,
    };
    let finished_at = Utc::now();
    Ok(LocalAgentReport {
        schema_version: LOCAL_AGENT_RUN_SCHEMA_VERSION,
        run_id,
        status,
        started_at,
        finished_at,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        runner: request.runner,
        provider: admitted.vendor.into(),
        model: request.model,
        vendor_session_id: response.as_ref().and_then(|value| value.session_id.clone()),
        response: response.map(|value| value.response).unwrap_or_default(),
        failure,
        stop_reason,
        actions,
        approvals,
        cleanup,
        runtime,
        usage,
        quota_checks,
        cost,
        cost_coverage,
        receipt,
        policy_sha256: policy.effective_sha256.clone(),
        policy_layers: policy.layers.clone(),
        validation,
        broker: broker_evidence,
    })
}

fn classify_terminal_error(error: &str) -> LocalStopReason {
    if error.contains("cancelled by signal") {
        LocalStopReason::Cancelled
    } else if error.contains("wall-time ceiling") {
        LocalStopReason::WallTimeExceeded
    } else if error.starts_with("MCP broker stopped the run:") {
        LocalStopReason::PolicyDenied
    } else {
        LocalStopReason::VendorError
    }
}

fn receipt_evidence(status: LocalRunStatus, cost: &AgentRunCostEvidence) -> ReceiptEvidence {
    let mut reasons = vec![
        ReceiptIneligibilityReason::LocalSigningUnavailable,
        ReceiptIneligibilityReason::SubscriptionNotInvoiceReconciled,
    ];
    if status == LocalRunStatus::Failed {
        reasons.push(ReceiptIneligibilityReason::RunFailed);
    }
    if cost.unmeasured_components() > 0 {
        reasons.push(ReceiptIneligibilityReason::CostEvidenceUnmeasured);
    }
    ReceiptEvidence {
        eligible: false,
        minted: false,
        reasons,
    }
}

struct Admitted<'a> {
    bridge_runner: &'static str,
    vendor: &'static str,
    vendor_model: &'a str,
    max_budget_usd: Option<f64>,
    max_observed_tool_calls: u32,
}

fn admit<'a>(
    request: &'a LocalAgentRequest,
    policy: &ResolvedAgentPolicy,
) -> Result<Admitted<'a>, LocalAgentError> {
    policy
        .policy
        .validate()
        .map_err(crate::execution_broker::BrokerError::from)?;
    if request.prompt.trim().is_empty() {
        return Err(denied("prompt", "must be nonempty"));
    }
    if request.plan_reference.is_empty()
        || request.plan_reference.chars().count() > 256
        || request.plan_reference.chars().any(char::is_control)
    {
        return Err(denied(
            "plan_reference",
            "must contain 1..=256 visible characters",
        ));
    }
    let (bridge_runner, vendor, expected_prefix) = match request.runner {
        AgentRunner::CodexSdk => ("codex-sdk", "openai", "openai/"),
        AgentRunner::ClaudeAgentSdk => ("claude-agent-sdk", "anthropic", "anthropic/"),
        AgentRunner::CliSubprocess if request.model.starts_with("openai/") => {
            ("codex-cli", "openai", "openai/")
        }
        AgentRunner::CliSubprocess if request.model.starts_with("anthropic/") => {
            ("claude-cli", "anthropic", "anthropic/")
        }
        _ => {
            return Err(denied(
                "inference.allowed_runners",
                "local vendor supervisor accepts codex_sdk, claude_agent_sdk, or a provider-qualified cli_subprocess",
            ))
        }
    };
    if !policy
        .policy
        .inference
        .allowed_runners
        .contains(&request.runner)
    {
        return Err(denied(
            "inference.allowed_runners",
            format!("{:?} is not admitted", request.runner),
        ));
    }
    if !policy
        .policy
        .inference
        .allowed_providers
        .iter()
        .any(|allowed| allowed == vendor)
    {
        return Err(denied(
            "inference.allowed_providers",
            format!("{vendor} is not admitted"),
        ));
    }
    if !policy
        .policy
        .inference
        .allowed_models
        .iter()
        .any(|allowed| allowed == &request.model)
    {
        return Err(denied(
            "inference.allowed_models",
            format!("{} is not admitted", request.model),
        ));
    }
    let vendor_model = request.model.strip_prefix(expected_prefix).ok_or_else(|| {
        denied(
            "model",
            format!("must use the provider-qualified prefix {expected_prefix}"),
        )
    })?;
    if vendor_model.is_empty() {
        return Err(denied("model", "provider-qualified model is incomplete"));
    }
    if !policy
        .policy
        .inference
        .allowed_cost_bases
        .contains(&PolicyCostBasis::Subscription)
    {
        return Err(denied(
            "inference.allowed_cost_bases",
            "subscription is not admitted",
        ));
    }
    if request.marginal_cash_micros > policy.policy.budgets.max_subscription_marginal_cash_micros {
        return Err(denied(
            "budgets.max_subscription_marginal_cash_micros",
            "requested marginal cash exceeds policy",
        ));
    }
    if request.allocated_plan_micros.unwrap_or(0)
        > policy.policy.budgets.max_subscription_allocated_micros
    {
        return Err(denied(
            "budgets.max_subscription_allocated_micros",
            "requested plan allocation exceeds policy",
        ));
    }
    if policy.policy.limits.max_api_calls == 0
        || policy.policy.limits.max_model_turns == 0
        || policy.policy.limits.max_wall_time_seconds == 0
    {
        return Err(denied(
            "limits",
            "API-call, model-turn, and wall-time ceilings must be nonzero",
        ));
    }
    if policy
        .policy
        .budgets
        .subscription_quota_caps
        .iter()
        .any(|cap| cap.unit == SubscriptionQuotaUnit::VendorUnits)
    {
        return Err(denied(
            "budgets.subscription_quota_caps",
            "vendor_units are not observable through the pinned SDK or structured CLI adapters",
        ));
    }

    let mut max_observed_tool_calls = policy
        .policy
        .limits
        .max_api_calls
        .saturating_sub(1)
        .min(policy.policy.limits.max_model_turns.saturating_sub(1));
    if let Some(cap) = policy
        .policy
        .budgets
        .subscription_quota_caps
        .iter()
        .find(|cap| cap.unit == SubscriptionQuotaUnit::ToolCalls)
    {
        max_observed_tool_calls =
            max_observed_tool_calls.min(u32::try_from(cap.max_units).unwrap_or(u32::MAX));
    }
    let max_budget_usd = bridge_runner
        .starts_with("claude-")
        .then(|| policy.policy.budgets.max_subscription_marginal_cash_micros as f64 / 1_000_000.0);
    let max_budget_usd = max_budget_usd.filter(|value| *value > 0.0);
    Ok(Admitted {
        bridge_runner,
        vendor,
        vendor_model,
        max_budget_usd,
        max_observed_tool_calls,
    })
}

fn broker_tool_metadata(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        crate::execution_broker::READ_FILE_TOOL => Some((
            crate::execution_broker::READ_FILE_TOOL,
            "Read one authorized UTF-8 repository file from the isolated run snapshot.",
        )),
        crate::execution_broker::LIST_FILES_TOOL => Some((
            crate::execution_broker::LIST_FILES_TOOL,
            "List authorized files beneath one repository-relative path in the isolated snapshot.",
        )),
        crate::execution_broker::WRITE_FILE_TOOL => Some((
            crate::execution_broker::WRITE_FILE_TOOL,
            "Stage one complete UTF-8 file in the isolated run workspace; never write the source checkout.",
        )),
        crate::execution_broker::RUN_COMMAND_TOOL => Some((
            crate::execution_broker::RUN_COMMAND_TOOL,
            "Run one policy-allowlisted executable and argv in an isolated no-network workspace. No shell interpolation.",
        )),
        crate::execution_broker::FETCH_URL_TOOL => Some((
            crate::execution_broker::FETCH_URL_TOOL,
            "GET one exact policy-authorized HTTP(S) URL through pinned DNS and bounded output.",
        )),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)] // bridge plumbing — see failover.rs precedent
async fn run_bridge_process(
    node: &Path,
    bridge: &Path,
    workspace: &Path,
    request: &[u8],
    timeout: Duration,
    output_cap: u64,
    failure_rx: &mut mpsc::UnboundedReceiver<String>,
    server_task: &mut tokio::task::JoinHandle<Result<(), McpError>>,
) -> Result<BridgeResponse, String> {
    let mut command = tokio::process::Command::new(node);
    command
        .arg(bridge)
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.as_std_mut().process_group(0);
    }
    sanitize_vendor_environment(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("start vendor adapter bridge: {error}"))?;
    let child_pid = child.id();
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "vendor adapter bridge stdin was unavailable".to_string())?;
    stdin
        .write_all(request)
        .await
        .map_err(|error| format!("write vendor adapter request: {error}"))?;
    stdin
        .shutdown()
        .await
        .map_err(|error| format!("close vendor adapter request: {error}"))?;
    drop(stdin);

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "vendor adapter bridge stdout was unavailable".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "vendor adapter bridge stderr was unavailable".to_string())?;
    let stdout_task = tokio::spawn(read_bounded(stdout, output_cap));
    let stderr_task = tokio::spawn(read_bounded(stderr, output_cap));
    let status = tokio::select! {
        result = child.wait() => result.map_err(|error| format!("wait for vendor adapter bridge: {error}"))?,
        denial = failure_rx.recv() => {
            let detail = denial.unwrap_or_else(|| "MCP broker stopped unexpectedly".into());
            terminate_bridge_process_group(&mut child, child_pid).await;
            let _ = child.wait().await;
            return Err(format!("MCP broker stopped the run: {detail}"));
        }
        server = &mut *server_task => {
            let detail = match server {
                Ok(Ok(())) => "MCP broker exited before the vendor runtime".into(),
                Ok(Err(error)) => format!("MCP broker failed: {error}"),
                Err(error) => format!("MCP broker task failed: {error}"),
            };
            terminate_bridge_process_group(&mut child, child_pid).await;
            let _ = child.wait().await;
            return Err(detail);
        }
        () = cancellation_signal() => {
            terminate_bridge_process_group(&mut child, child_pid).await;
            let _ = child.wait().await;
            return Err("run cancelled by signal".into());
        }
        () = tokio::time::sleep(timeout) => {
            terminate_bridge_process_group(&mut child, child_pid).await;
            let _ = child.wait().await;
            return Err("run wall-time ceiling reached".into());
        }
    };
    let stdout = stdout_task
        .await
        .map_err(|error| format!("join bridge stdout reader: {error}"))??;
    let stderr = stderr_task
        .await
        .map_err(|error| format!("join bridge stderr reader: {error}"))??;
    if stdout.len() as u64 > output_cap || stderr.len() as u64 > output_cap {
        return Err(format!(
            "vendor adapter bridge output exceeded {output_cap} bytes"
        ));
    }
    let parsed: BridgeResponse = serde_json::from_slice(&stdout).map_err(|error| {
        format!(
            "parse vendor adapter bridge response: {error}; stderr={}",
            bounded_terminal(&stderr)
        )
    })?;
    if !status.success() && parsed.ok {
        return Err(format!(
            "vendor adapter bridge exited with {status} despite a successful response"
        ));
    }
    Ok(parsed)
}
async fn terminate_bridge_process_group(child: &mut tokio::process::Child, pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        let mut kill = tokio::process::Command::new("/bin/kill");
        kill.env_clear()
            .arg("-KILL")
            .arg(format!("-{pid}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = tokio::time::timeout(Duration::from_secs(1), kill.status()).await;
    }
    #[cfg(not(unix))]
    let _ = pid;
    let _ = child.start_kill();
}

async fn cancellation_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

async fn read_bounded(reader: impl AsyncRead + Unpin, cap: u64) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    reader
        .take(cap.saturating_add(1))
        .read_to_end(&mut output)
        .await
        .map_err(|error| format!("read vendor SDK bridge output: {error}"))?;
    Ok(output)
}

async fn probe_bridge(
    node: &Path,
    bridge: &Path,
    runner: &str,
    explicit_executable: Option<&Path>,
) -> Result<BridgeProbe, LocalAgentError> {
    let mut command = tokio::process::Command::new(node);
    command.arg(bridge).arg("--probe").arg(runner);
    if let Some(path) = explicit_executable {
        command.arg(path);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    sanitize_vendor_environment(&mut command);
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .map_err(|_| LocalAgentError::BridgeProbe("probe timed out".into()))?
        .map_err(|error| LocalAgentError::BridgeProbe(error.to_string()))?;
    if output.stdout.len() > BRIDGE_PROBE_OUTPUT_CAP
        || output.stderr.len() > BRIDGE_PROBE_OUTPUT_CAP
    {
        return Err(LocalAgentError::BridgeProbe(
            "probe output exceeded 64 KiB".into(),
        ));
    }
    if !output.status.success() {
        return Err(LocalAgentError::BridgeProbe(format!(
            "probe exited with {}; stderr={}",
            output.status,
            bounded_terminal(&output.stderr)
        )));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| LocalAgentError::BridgeProbe(format!("invalid probe JSON: {error}")))
}

#[allow(clippy::too_many_arguments)] // probe-evidence tuple — see failover.rs precedent
fn verify_probe(
    runner: AgentRunner,
    bridge_runner: &str,
    probe: BridgeProbe,
    node: &Path,
    node_sha256: String,
    bridge_sha256: String,
    dependency_lock_sha256: String,
    explicit_runtime: bool,
) -> Result<VendorRuntimeEvidence, LocalAgentError> {
    if !probe.ok {
        return Err(LocalAgentError::BridgeProbe(
            "bridge reported an unsuccessful probe".into(),
        ));
    }
    let node_major = probe
        .node_version
        .split('.')
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| LocalAgentError::BridgeProbe("invalid Node.js version".into()))?;
    if node_major < 18 {
        return Err(LocalAgentError::BridgeProbe(format!(
            "Node.js {} is unsupported; version 18 or newer is required",
            probe.node_version
        )));
    }
    let (
        package,
        sdk_version,
        runtime_name,
        runtime_version,
        version_output,
        adapter_transport,
        turn_enforcement,
    ) = match (runner, bridge_runner) {
        (AgentRunner::CodexSdk, "codex-sdk") => (
            "@openai/codex-sdk",
            CODEX_SDK_VERSION,
            "codex-cli",
            CODEX_RUNTIME_VERSION,
            format!("codex-cli {CODEX_RUNTIME_VERSION}"),
            "official_sdk",
            "wall-time plus conservative MCP-round-trip cap; vendor-internal calls are settled after completion",
        ),
        (AgentRunner::ClaudeAgentSdk, "claude-agent-sdk") => (
            "@anthropic-ai/claude-agent-sdk",
            CLAUDE_SDK_VERSION,
            "claude-code",
            CLAUDE_RUNTIME_VERSION,
            format!("{CLAUDE_RUNTIME_VERSION} (Claude Code)"),
            "official_sdk",
            "native maxTurns plus wall-time and conservative MCP-round-trip cap",
        ),
        (AgentRunner::CliSubprocess, "codex-cli") => (
            "@openai/codex-sdk",
            CODEX_SDK_VERSION,
            "codex-cli",
            CODEX_RUNTIME_VERSION,
            format!("codex-cli {CODEX_RUNTIME_VERSION}"),
            "structured_cli_fallback",
            "wall-time plus conservative MCP-round-trip cap; vendor-internal calls are settled after completion",
        ),
        (AgentRunner::CliSubprocess, "claude-cli") => (
            "@anthropic-ai/claude-agent-sdk",
            CLAUDE_SDK_VERSION,
            "claude-code",
            CLAUDE_RUNTIME_VERSION,
            format!("{CLAUDE_RUNTIME_VERSION} (Claude Code)"),
            "structured_cli_fallback",
            "native maxTurns plus wall-time and conservative MCP-round-trip cap",
        ),
        _ => return Err(denied("runner", "unsupported local vendor adapter tuple")),
    };
    let actual = probe
        .compatibility
        .get(package)
        .ok_or_else(|| LocalAgentError::BridgeProbe(format!("probe omitted {package}")))?;
    if actual.sdk_version != sdk_version
        || actual.runtime != runtime_name
        || actual.runtime_version != runtime_version
        || probe.runtime.version != version_output
    {
        return Err(LocalAgentError::BridgeProbe(format!(
            "unsupported {package} compatibility tuple: sdk={}, runtime={} {}, executable={}",
            actual.sdk_version, actual.runtime, actual.runtime_version, probe.runtime.version
        )));
    }
    if !probe.runtime.authenticated {
        return Err(LocalAgentError::BridgeProbe(format!(
            "{runtime_name} {runtime_version} has no authenticated local vendor session"
        )));
    }
    let launcher_path =
        canonical_regular_file(&probe.runtime.launcher_path, "open vendor runtime launcher")?;
    let runtime_path =
        canonical_regular_file(&probe.runtime.executable_path, "open vendor runtime")?;
    let runtime_sha256 = hash_regular_file(&runtime_path, MAX_HASHED_EXECUTABLE_BYTES)?;
    let launcher_sha256 = if launcher_path == runtime_path {
        runtime_sha256.clone()
    } else {
        hash_regular_file(&launcher_path, MAX_HASHED_EXECUTABLE_BYTES)?
    };
    Ok(VendorRuntimeEvidence {
        sdk_package: package.into(),
        sdk_version: sdk_version.into(),
        adapter_transport,
        runtime_name: runtime_name.into(),
        runtime_version: runtime_version.into(),
        runtime_source: if explicit_runtime {
            "user_installed_supported_cli"
        } else if adapter_transport == "structured_cli_fallback" {
            "bridge_bundled_supported_cli"
        } else {
            "sdk_bundled"
        },
        runtime_launcher: launcher_path.display().to_string(),
        runtime_launcher_sha256: launcher_sha256,
        runtime_executable: runtime_path.display().to_string(),
        runtime_executable_sha256: runtime_sha256,
        node_version: probe.node_version,
        node_executable: node.display().to_string(),
        node_sha256,
        bridge_sha256,
        dependency_lock_sha256,
        authentication_method: probe.runtime.authentication_method,
        credentials_observed_by_tokentrimmer: false,
        model_tool_boundary:
            "loopback bearer-authenticated MCP; built-in mutation/network tools disabled",
        max_turn_enforcement: turn_enforcement,
    })
}

fn settle_quotas(
    policy: &ResolvedAgentPolicy,
    usage: &VendorUsage,
    tool_calls: u64,
) -> Vec<QuotaCheck> {
    policy
        .policy
        .budgets
        .subscription_quota_caps
        .iter()
        .map(|cap| {
            let (used, source) = match cap.unit {
                SubscriptionQuotaUnit::Requests => (Some(1), "tokentrimmer-runner"),
                SubscriptionQuotaUnit::Tokens => (Some(usage.total_tokens()), "vendor-sdk-usage"),
                SubscriptionQuotaUnit::ToolCalls => (Some(tool_calls), "tokentrimmer-mcp-broker"),
                SubscriptionQuotaUnit::VendorUnits => (None, "vendor-signal-unavailable"),
            };
            QuotaCheck {
                unit: cap.unit,
                used,
                limit: cap.max_units,
                source,
                within_limit: used.is_some_and(|value| value <= cap.max_units),
            }
        })
        .collect()
}

fn build_cost_evidence(
    run_id: &str,
    request: &LocalAgentRequest,
    vendor: &str,
    response: Option<&BridgeResponse>,
    quota_checks: &[QuotaCheck],
) -> Result<AgentRunCostEvidence, LocalAgentError> {
    let cost = if let Some(response) = response.filter(|value| value.usage.is_some()) {
        let quota = quota_checks
            .iter()
            .find(|check| check.used.is_some())
            .map(|check| SubscriptionQuotaEvidence {
                unit: match check.unit {
                    SubscriptionQuotaUnit::Requests => CostQuotaUnit::Requests,
                    SubscriptionQuotaUnit::Tokens => CostQuotaUnit::Tokens,
                    SubscriptionQuotaUnit::ToolCalls => CostQuotaUnit::ToolCalls,
                    SubscriptionQuotaUnit::VendorUnits => CostQuotaUnit::VendorUnits,
                },
                used: check.used.unwrap_or(0),
                limit: Some(check.limit),
                window_ends_at: None,
                source: check.source.into(),
            });
        AgentCostBasis::Subscription {
            vendor: vendor.into(),
            plan_reference: request.plan_reference.clone(),
            marginal_cash_micros: i64::try_from(request.marginal_cash_micros)
                .map_err(|_| denied("marginal_cash_micros", "value exceeds i64"))?,
            allocated_plan_micros: request
                .allocated_plan_micros
                .map(i64::try_from)
                .transpose()
                .map_err(|_| denied("allocated_plan_micros", "value exceeds i64"))?,
            api_equivalent_micros: response.total_cost_usd.and_then(usd_to_micros),
            quota,
        }
    } else {
        AgentCostBasis::Unmeasured {
            expected_basis: ExpectedAgentCostBasis::Subscription,
            reasons: vec![UnmeasuredCostReason {
                code: UnmeasuredCostReasonCode::VendorSignalUnavailable,
                detail: Some("vendor SDK ended without structured usage evidence".into()),
            }],
        }
    };
    let evidence = AgentRunCostEvidence {
        schema_version: AGENT_COST_SCHEMA_VERSION,
        run_id: run_id.into(),
        components: vec![AgentCostComponent {
            component_id: "primary-vendor-run".into(),
            purpose: AgentCostPurpose::PrimaryTurn,
            attempt: 1,
            cost,
        }],
    };
    evidence.validate().map_err(|error| {
        LocalAgentError::BridgeProtocol(format!("invalid cost evidence: {error}"))
    })?;
    Ok(evidence)
}

fn usd_to_micros(value: f64) -> Option<i64> {
    if !value.is_finite() || value < 0.0 || value > i64::MAX as f64 / 1_000_000.0 {
        return None;
    }
    Some((value * 1_000_000.0).round() as i64)
}

fn resolve_bridge_path(explicit: Option<&Path>) -> Result<PathBuf, LocalAgentError> {
    if let Some(path) = explicit {
        return canonical_regular_file(path, "open vendor SDK bridge");
    }
    if let Some(path) = std::env::var_os("TT_VENDOR_SDK_BRIDGE") {
        return canonical_regular_file(Path::new(&path), "open vendor SDK bridge");
    }
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("vendor-sdk-bridge")
        .join("bridge.mjs");
    canonical_regular_file(&source, "open vendor SDK bridge")
}

fn resolve_node() -> Result<PathBuf, LocalAgentError> {
    if let Some(path) = std::env::var_os("TT_NODE_PATH") {
        return canonical_regular_file(Path::new(&path), "resolve Node.js");
    }
    let path =
        std::env::var_os("PATH").ok_or_else(|| denied("runtime.node", "PATH is unavailable"))?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(if cfg!(windows) { "node.exe" } else { "node" });
        if candidate.is_file() {
            return canonical_regular_file(&candidate, "resolve Node.js");
        }
    }
    Err(denied(
        "runtime.node",
        "Node.js 18 or newer was not found on PATH",
    ))
}

fn canonical_regular_file(
    path: &Path,
    operation: &'static str,
) -> Result<PathBuf, LocalAgentError> {
    let canonical = std::fs::canonicalize(path).map_err(|source| LocalAgentError::Io {
        operation,
        path: path.display().to_string(),
        source,
    })?;
    let metadata = std::fs::metadata(&canonical).map_err(|source| LocalAgentError::Io {
        operation,
        path: canonical.display().to_string(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(denied(
            "runtime.path",
            format!("{} is not a regular file", canonical.display()),
        ));
    }
    Ok(canonical)
}

fn hash_regular_file(path: &Path, max_bytes: u64) -> Result<String, LocalAgentError> {
    let mut file = std::fs::File::open(path).map_err(|source| LocalAgentError::Io {
        operation: "hash runtime file",
        path: path.display().to_string(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| LocalAgentError::Io {
        operation: "inspect runtime file",
        path: path.display().to_string(),
        source,
    })?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return Err(denied(
            "runtime.path",
            format!(
                "{} must be a regular file no larger than {max_bytes} bytes",
                path.display()
            ),
        ));
    }
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| LocalAgentError::Io {
                operation: "hash runtime file",
                path: path.display().to_string(),
                source,
            })?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn sanitize_vendor_environment(command: &mut tokio::process::Command) {
    for key in [
        "OPENAI_API_KEY",
        "CODEX_API_KEY",
        "AZURE_OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_BEARER_TOKEN_BEDROCK",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "NODE_OPTIONS",
        "NODE_PATH",
    ] {
        command.env_remove(key);
    }
}

fn bounded_terminal(bytes: &[u8]) -> String {
    const LIMIT: usize = 8 * 1024;
    let slice = &bytes[..bytes.len().min(LIMIT)];
    String::from_utf8_lossy(slice)
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
        .collect()
}

fn denied(field: &'static str, detail: impl Into<String>) -> LocalAgentError {
    LocalAgentError::PolicyDenied {
        field,
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_policy::{
        parse_repository_policy, resolve_agent_policy, OrganizationPolicyMode,
    };

    fn policy() -> ResolvedAgentPolicy {
        let repository = parse_repository_policy(
            r#"schema_version = 1
[filesystem]
readable_roots = ["."]
writable_roots = ["src"]
max_files = 20
max_file_bytes = 100000
max_total_read_bytes = 500000
max_total_write_bytes = 500000
allow_symlinks = false
excluded_paths = [".git/**", ".env*"]
[process]
allowed_commands = []
max_subprocesses = 0
max_duration_seconds = 0
max_output_bytes = 1000000
allow_shell = false
[network]
default = "deny"
allowed_destinations = []
allow_redirects = false
inherit_proxy_env = false
[inference]
allowed_runners = ["codex_sdk"]
allowed_providers = ["openai"]
allowed_models = ["openai/gpt-test"]
allowed_cost_bases = ["subscription"]
[limits]
max_api_calls = 8
max_model_turns = 6
max_retries = 0
max_wall_time_seconds = 60
max_diff_bytes = 100000
max_changed_files = 5
[budgets]
max_api_cash_micros = 0
max_subscription_marginal_cash_micros = 0
max_subscription_allocated_micros = 1000
max_self_hosted_tco_micros = 0
subscription_quota_caps = [{ unit = "tool_calls", max_units = 3 }]
allow_unmeasured = true
[approvals]
destructive_operations = "deny"
rollback = "deny"
[validation]
required_commands = []
stop_on_regression = true
"#,
        )
        .unwrap();
        resolve_agent_policy(
            OrganizationPolicyMode::NotConfigured,
            &repository,
            None,
            None,
        )
        .unwrap()
    }

    fn request() -> LocalAgentRequest {
        LocalAgentRequest {
            repository: PathBuf::from("/tmp/repo"),
            prompt: "Fix it".into(),
            runner: AgentRunner::CodexSdk,
            model: "openai/gpt-test".into(),
            session_id: None,
            plan_reference: "local-codex-session".into(),
            marginal_cash_micros: 0,
            allocated_plan_micros: None,
            bridge_path: None,
            vendor_executable: None,
        }
    }

    #[test]
    fn admits_only_exact_runner_provider_model_and_subscription_basis() {
        let policy = policy();
        let run_request = request();
        let admitted = admit(&run_request, &policy).unwrap();
        assert_eq!(admitted.vendor_model, "gpt-test");
        assert_eq!(admitted.max_observed_tool_calls, 3);

        let mut wrong = request();
        wrong.model = "openai/other".into();
        assert!(matches!(
            admit(&wrong, &policy),
            Err(LocalAgentError::PolicyDenied {
                field: "inference.allowed_models",
                ..
            })
        ));
    }

    #[test]
    fn admits_provider_qualified_structured_cli_fallbacks() {
        let mut cli_policy = policy();
        cli_policy.policy.inference.allowed_runners = vec![AgentRunner::CliSubprocess];
        let mut cli_request = request();
        cli_request.runner = AgentRunner::CliSubprocess;
        let codex = admit(&cli_request, &cli_policy).unwrap();
        assert_eq!(codex.bridge_runner, "codex-cli");
        assert_eq!(codex.vendor, "openai");

        cli_policy.policy.inference.allowed_providers = vec!["anthropic".into()];
        cli_policy.policy.inference.allowed_models = vec!["anthropic/claude-test".into()];
        cli_request.model = "anthropic/claude-test".into();
        let claude = admit(&cli_request, &cli_policy).unwrap();
        assert_eq!(claude.bridge_runner, "claude-cli");
        assert_eq!(claude.vendor, "anthropic");
    }

    #[test]
    fn settles_observable_subscription_quota_without_inventing_vendor_units() {
        let checks = settle_quotas(&policy(), &VendorUsage::default(), 2);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].used, Some(2));
        assert!(checks[0].within_limit);
    }

    #[test]
    fn preserves_subscription_cash_and_counterfactual_as_distinct_fields() {
        let response = BridgeResponse {
            ok: true,
            session_id: Some("session".into()),
            response: "done".into(),
            usage: Some(VendorUsage {
                input_tokens: 10,
                output_tokens: 4,
                ..VendorUsage::default()
            }),
            total_cost_usd: Some(0.0125),
            tool_calls: 0,
            error: None,
        };
        let evidence =
            build_cost_evidence("run", &request(), "openai", Some(&response), &[]).unwrap();
        match &evidence.components[0].cost {
            AgentCostBasis::Subscription {
                marginal_cash_micros,
                api_equivalent_micros,
                ..
            } => {
                assert_eq!(*marginal_cash_micros, 0);
                assert_eq!(*api_equivalent_micros, Some(12_500));
            }
            other => panic!("unexpected cost basis: {other:?}"),
        }
    }

    #[test]
    fn classifies_terminal_paths_and_explains_receipt_ineligibility() {
        assert_eq!(
            classify_terminal_error("run cancelled by signal"),
            LocalStopReason::Cancelled
        );
        assert_eq!(
            classify_terminal_error("run wall-time ceiling reached"),
            LocalStopReason::WallTimeExceeded
        );
        assert_eq!(
            classify_terminal_error("MCP broker stopped the run: denied"),
            LocalStopReason::PolicyDenied
        );

        let cost = build_cost_evidence("run", &request(), "openai", None, &[]).unwrap();
        let receipt = receipt_evidence(LocalRunStatus::Failed, &cost);
        assert!(!receipt.eligible);
        assert!(!receipt.minted);
        assert!(receipt
            .reasons
            .contains(&ReceiptIneligibilityReason::RunFailed));
        assert!(receipt
            .reasons
            .contains(&ReceiptIneligibilityReason::CostEvidenceUnmeasured));
    }
}
