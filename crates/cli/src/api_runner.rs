//! Policy-bound runner for the hosted TokenTrimmer agent contract.
//!
//! This is the only local coding-agent path allowed to call
//! `POST /v1/agent/runs`: it admits an exact provider-qualified model, a live
//! TokenTrimmer principal, an exact gateway destination, finite turn/time/cash
//! ceilings, and then delegates local tool calls to an external broker. The
//! runner never interprets or executes a tool call itself.

use std::{net::IpAddr, time::Duration};

use thiserror::Error;
use tt_client::{AgentOutcome, Message, RunStatus, Tool, ToolExecutor};

use crate::agent_policy::{
    AgentPolicyError, AgentRunner, NetworkDestination, NetworkScheme, PolicyCostBasis,
    ResolvedAgentPolicy,
};

const GATEWAY_MAX_TURNS: u32 = 32;
const MAX_EXACT_F64_INTEGER: u64 = 9_007_199_254_740_991;

/// One bounded request to the hosted TokenTrimmer agent loop.
#[derive(Debug, Clone)]
pub struct ApiRunRequest {
    /// Exact provider-qualified model from policy, for example `openai/gpt-5`.
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
    /// Requested server-side model-turn ceiling. Must fit both policy call/turn
    /// limits and the gateway's public maximum.
    pub max_turns: u32,
    /// Aggregate API cash ceiling in integer micro-USD. Must be nonzero and no
    /// greater than the effective policy budget.
    pub max_cash_micros: u64,
    /// Client-tool resume ceiling. This bounds broker round trips independently
    /// of the server-side turn limit.
    pub max_resume_rounds: usize,
    /// Optional request-ledger tag. The effective policy hash is used when absent.
    pub tag: Option<String>,
    pub interactive: bool,
}

/// Successful transport result plus the exact authority admitted for the run.
#[derive(Debug, Clone)]
pub struct ApiRunOutcome {
    pub agent: AgentOutcome,
    pub provider: String,
    pub model: String,
    pub policy_sha256: String,
    pub max_cash_micros: u64,
}

impl ApiRunOutcome {
    /// The gateway records a started turn that settles above the cap instead of
    /// hiding or clamping it. Callers must surface this state as a failure.
    #[must_use]
    pub fn budget_breached(&self) -> bool {
        self.agent.stop_reason() == Some("budget_breach")
    }
}

#[derive(Debug, Error)]
pub enum ApiRunnerError {
    #[error("agent policy is invalid: {0}")]
    InvalidPolicy(#[from] AgentPolicyError),
    #[error("policy denied {field}: {detail}")]
    PolicyDenied { field: &'static str, detail: String },
    #[error("hosted API runner requires a tt_live_ TokenTrimmer principal")]
    LivePrincipalRequired,
    #[error("invalid TokenTrimmer gateway URL: {0}")]
    InvalidGatewayUrl(String),
    #[error("failed to construct bounded gateway transport: {0}")]
    Transport(#[source] reqwest::Error),
    #[error("TokenTrimmer agent request failed: {0}")]
    Gateway(#[from] tt_client::Error),
    #[error("agent run exceeded its {seconds}s wall-time ceiling")]
    WallTimeExceeded { seconds: u64 },
    #[error("gateway returned invalid agent-run evidence: {0}")]
    InvalidRunEvidence(String),
}

/// Hosted API runner admitted by one already-resolved policy.
///
/// Construction validates the live principal and exact network destination.
/// [`run`](Self::run) validates the request-specific model and ceilings before
/// creating any network client or dispatching a request.
pub struct ApiRunner<'a> {
    gateway_base: String,
    api_key: String,
    policy: &'a ResolvedAgentPolicy,
}

impl<'a> ApiRunner<'a> {
    pub fn new(
        gateway_base: impl Into<String>,
        api_key: impl Into<String>,
        policy: &'a ResolvedAgentPolicy,
    ) -> Result<Self, ApiRunnerError> {
        policy.policy.validate()?;

        let api_key = api_key.into();
        if !api_key.starts_with("tt_live_") || api_key.len() == "tt_live_".len() {
            return Err(ApiRunnerError::LivePrincipalRequired);
        }

        let gateway_base = normalize_and_authorize_gateway(&gateway_base.into(), policy)?;
        Ok(Self {
            gateway_base,
            api_key,
            policy,
        })
    }

    /// Run the existing `/v1/agent/runs` contract. Local tool calls are handed
    /// only to `executor`; this layer has no filesystem/process/network powers.
    pub async fn run(
        &self,
        request: ApiRunRequest,
        executor: &(impl ToolExecutor + Sync),
    ) -> Result<ApiRunOutcome, ApiRunnerError> {
        let admitted = self.admit(&request)?;
        let timeout_seconds = self.policy.policy.limits.max_wall_time_seconds;

        let mut transport = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(timeout_seconds.min(10)))
            .timeout(Duration::from_secs(timeout_seconds))
            // A bearer-authenticated run never follows redirects. This is
            // intentionally stricter than a policy that permits other redirects.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!(
                "tt-price-first-api-runner/",
                env!("CARGO_PKG_VERSION")
            ));
        if !self.policy.policy.network.inherit_proxy_env {
            transport = transport.no_proxy();
        }
        let transport = transport.build().map_err(ApiRunnerError::Transport)?;
        let client = tt_client::Client::with_http_client(
            transport,
            self.gateway_base.clone(),
            self.api_key.clone(),
        );

        let mut builder =
            client
                .agent()
                .model(&admitted.gateway_model)
                .provider(&admitted.provider)
                .messages(request.messages)
                .tools(request.tools)
                .max_turns(request.max_turns)
                // The total run cap and each individual turn use the same hard cash
                // ceiling. The per-turn header also makes unknown pricing fail closed.
                .max_cost_usd(admitted.max_cost_usd)
                .cost_limit(admitted.max_cost_usd)
                .max_resume_rounds(request.max_resume_rounds)
                .tag(request.tag.unwrap_or_else(|| {
                    format!("price-first:{}", &self.policy.effective_sha256[..12])
                }));
        if request.interactive {
            builder = builder.interactive();
        }

        let result =
            tokio::time::timeout(Duration::from_secs(timeout_seconds), builder.run(executor))
                .await
                .map_err(|_| ApiRunnerError::WallTimeExceeded {
                    seconds: timeout_seconds,
                })??;
        validate_run_evidence(&result, request.max_turns, admitted.max_cost_usd)?;

        Ok(ApiRunOutcome {
            agent: result,
            provider: admitted.provider,
            model: request.model,
            policy_sha256: self.policy.effective_sha256.clone(),
            max_cash_micros: request.max_cash_micros,
        })
    }

    fn admit(&self, request: &ApiRunRequest) -> Result<AdmittedRequest, ApiRunnerError> {
        let policy = &self.policy.policy;
        require(
            policy
                .inference
                .allowed_runners
                .contains(&AgentRunner::TokenTrimmerApi),
            "inference.allowed_runners",
            "token_trimmer_api is not authorized",
        )?;
        require(
            policy
                .inference
                .allowed_cost_bases
                .contains(&PolicyCostBasis::ApiMetered),
            "inference.allowed_cost_bases",
            "api_metered is not authorized",
        )?;
        require(
            !request.messages.is_empty(),
            "messages",
            "at least one message is required",
        )?;

        let (provider, gateway_model) = request.model.split_once('/').ok_or_else(|| {
            denied(
                "inference.allowed_models",
                "model must be provider-qualified as provider/model",
            )
        })?;
        require(
            !provider.is_empty() && !gateway_model.is_empty(),
            "inference.allowed_models",
            "model must contain nonempty provider and model components",
        )?;
        require(
            policy
                .inference
                .allowed_providers
                .iter()
                .any(|allowed| allowed == provider),
            "inference.allowed_providers",
            format!("provider {provider:?} is not authorized"),
        )?;
        require(
            policy
                .inference
                .allowed_models
                .iter()
                .any(|allowed| allowed == &request.model),
            "inference.allowed_models",
            format!("model {:?} is not authorized", request.model),
        )?;

        let turn_ceiling = policy
            .limits
            .max_model_turns
            .min(policy.limits.max_api_calls)
            .min(GATEWAY_MAX_TURNS);
        require(
            request.max_turns > 0 && request.max_turns <= turn_ceiling,
            "limits.max_model_turns",
            format!(
                "requested {} turns; effective API/turn ceiling is {turn_ceiling}",
                request.max_turns
            ),
        )?;
        require(
            request.max_resume_rounds <= request.max_turns as usize,
            "max_resume_rounds",
            "resume ceiling cannot exceed the model-turn ceiling",
        )?;
        require(
            policy.limits.max_wall_time_seconds > 0,
            "limits.max_wall_time_seconds",
            "must be nonzero for an API run",
        )?;
        require(
            request.max_cash_micros > 0
                && request.max_cash_micros <= policy.budgets.max_api_cash_micros,
            "budgets.max_api_cash_micros",
            format!(
                "requested {} micro-USD; effective ceiling is {}",
                request.max_cash_micros, policy.budgets.max_api_cash_micros
            ),
        )?;
        require(
            request.max_cash_micros <= MAX_EXACT_F64_INTEGER,
            "max_cash_micros",
            "exceeds the exact integer range of the gateway's USD wire type",
        )?;

        Ok(AdmittedRequest {
            provider: provider.to_string(),
            gateway_model: gateway_model.to_string(),
            max_cost_usd: request.max_cash_micros as f64 / 1_000_000.0,
        })
    }
}

struct AdmittedRequest {
    provider: String,
    gateway_model: String,
    max_cost_usd: f64,
}

fn denied(field: &'static str, detail: impl Into<String>) -> ApiRunnerError {
    ApiRunnerError::PolicyDenied {
        field,
        detail: detail.into(),
    }
}

fn require(
    condition: bool,
    field: &'static str,
    detail: impl Into<String>,
) -> Result<(), ApiRunnerError> {
    if condition {
        Ok(())
    } else {
        Err(denied(field, detail))
    }
}

fn normalize_and_authorize_gateway(
    raw: &str,
    policy: &ResolvedAgentPolicy,
) -> Result<String, ApiRunnerError> {
    let mut url = reqwest::Url::parse(raw)
        .map_err(|error| ApiRunnerError::InvalidGatewayUrl(error.to_string()))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ApiRunnerError::InvalidGatewayUrl(
            "userinfo is prohibited".into(),
        ));
    }
    if url.query().is_some() || url.fragment().is_some() || url.path() != "/" {
        return Err(ApiRunnerError::InvalidGatewayUrl(
            "base URL must not contain a path, query, or fragment".into(),
        ));
    }

    let scheme = match url.scheme() {
        "http" => NetworkScheme::Http,
        "https" => NetworkScheme::Https,
        other => {
            return Err(ApiRunnerError::InvalidGatewayUrl(format!(
                "unsupported scheme {other:?}"
            )))
        }
    };
    let host = url
        .host_str()
        .ok_or_else(|| ApiRunnerError::InvalidGatewayUrl("host is required".into()))?
        .to_ascii_lowercase();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| ApiRunnerError::InvalidGatewayUrl("explicit port is required".into()))?;

    if scheme == NetworkScheme::Http && !is_loopback_host(&host) {
        return Err(ApiRunnerError::InvalidGatewayUrl(
            "cleartext HTTP is allowed only for a loopback gateway".into(),
        ));
    }
    let destination = NetworkDestination { scheme, host, port };
    require(
        policy
            .policy
            .network
            .allowed_destinations
            .contains(&destination),
        "network.allowed_destinations",
        format!("gateway destination {destination:?} is not authorized"),
    )?;

    url.set_path("");
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn is_loopback_host(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn validate_run_evidence(
    outcome: &AgentOutcome,
    max_turns: u32,
    max_cost_usd: f64,
) -> Result<(), ApiRunnerError> {
    let run = &outcome.run;
    if run.turns > max_turns {
        return Err(ApiRunnerError::InvalidRunEvidence(format!(
            "reported {} turns above requested ceiling {max_turns}",
            run.turns
        )));
    }

    let costs = &run.usage.per_turn_cost_usd;
    let expected_cost_count = if run.status == RunStatus::Failed {
        run.turns.saturating_sub(1) as usize
    } else {
        run.turns as usize
    };
    if costs.len() != expected_cost_count {
        return Err(ApiRunnerError::InvalidRunEvidence(format!(
            "reported {} per-turn costs for {} settled turns",
            costs.len(),
            expected_cost_count
        )));
    }
    if !valid_cost(run.usage.cost_usd)
        || !run.usage.baseline_cost_usd.is_some_and(valid_cost)
        || !costs.iter().copied().all(valid_cost)
        || !run.summarizer_tax_usd.is_none_or(valid_cost)
    {
        return Err(ApiRunnerError::InvalidRunEvidence(
            "cost fields must be present, finite, and nonnegative".into(),
        ));
    }
    let turn_sum: f64 = costs.iter().sum();
    if (turn_sum - run.usage.cost_usd).abs() > 0.000_000_5 {
        return Err(ApiRunnerError::InvalidRunEvidence(format!(
            "per-turn cost sum {turn_sum} does not match aggregate {}",
            run.usage.cost_usd
        )));
    }

    let over_budget = run.usage.cost_usd > max_cost_usd;
    if over_budget != (run.stop_reason.as_deref() == Some("budget_breach")) {
        return Err(ApiRunnerError::InvalidRunEvidence(
            "cost above the authorized cap must be reported exactly as budget_breach".into(),
        ));
    }
    Ok(())
}

fn valid_cost(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_policy::{
        parse_repository_policy, resolve_agent_policy, OrganizationPolicyMode,
    };
    use httpmock::prelude::*;
    use serde_json::json;
    use tt_client::{async_trait, user};

    struct NoTools;

    #[async_trait]
    impl ToolExecutor for NoTools {
        async fn call(
            &self,
            _name: &str,
            _arguments: &str,
        ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
            Err("no local tools were advertised".into())
        }
    }

    fn policy(port: u16) -> ResolvedAgentPolicy {
        let text = format!(
            r#"schema_version = 1
[filesystem]
readable_roots = ["."]
writable_roots = []
max_files = 0
max_file_bytes = 0
max_total_read_bytes = 0
max_total_write_bytes = 0
allow_symlinks = false
excluded_paths = [".env", ".git/**", ".tokentrimmer/**"]
[process]
allowed_commands = []
max_subprocesses = 0
max_duration_seconds = 0
max_output_bytes = 0
allow_shell = false
[network]
default = "deny"
allowed_destinations = [{{ scheme = "http", host = "127.0.0.1", port = {port} }}]
allow_redirects = false
inherit_proxy_env = false
[inference]
allowed_runners = ["token_trimmer_api"]
allowed_providers = ["openai"]
allowed_models = ["openai/gpt-5"]
allowed_cost_bases = ["api_metered"]
[limits]
max_api_calls = 4
max_model_turns = 4
max_retries = 0
max_wall_time_seconds = 5
max_diff_bytes = 0
max_changed_files = 0
[budgets]
max_api_cash_micros = 500000
max_subscription_marginal_cash_micros = 0
max_subscription_allocated_micros = 0
max_self_hosted_tco_micros = 0
subscription_quota_caps = []
allow_unmeasured = false
[approvals]
destructive_operations = "deny"
rollback = "deny"
[validation]
required_commands = []
stop_on_regression = true
"#
        );
        let repository = parse_repository_policy(&text).unwrap();
        resolve_agent_policy(
            OrganizationPolicyMode::NotConfigured,
            &repository,
            None,
            None,
        )
        .unwrap()
    }

    fn request() -> ApiRunRequest {
        ApiRunRequest {
            model: "openai/gpt-5".into(),
            messages: vec![user("fix it")],
            tools: vec![],
            max_turns: 2,
            max_cash_micros: 500_000,
            max_resume_rounds: 2,
            tag: Some("repo-a".into()),
            interactive: true,
        }
    }

    fn completed_run() -> serde_json::Value {
        json!({
            "id": "run-1",
            "status": "completed",
            "turns": 1,
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 3,
                "cost_usd": 0.0001,
                "baseline_cost_usd": 0.0002,
                "per_turn_cost_usd": [0.0001]
            },
            "messages": [{ "role": "assistant", "content": "done" }]
        })
    }

    #[tokio::test]
    async fn runs_existing_contract_with_exact_policy_bounds() {
        let server = MockServer::start_async().await;
        let post = server.mock(|when, then| {
            when.method(POST)
                .path("/v1/agent/runs")
                .header("authorization", "Bearer tt_live_fixture")
                .header("x-tokentrimmer-provider", "openai")
                .header("x-tokentrimmer-cost-limit-usd", "0.5")
                .header("x-tokentrimmer-interactive", "1")
                .header("x-tokentrimmer-tag", "repo-a")
                .body_includes("\"model\":\"gpt-5\"")
                .body_includes("\"max_turns\":2")
                .body_includes("\"max_cost_usd\":0.5");
            then.status(200)
                .header("content-type", "application/json")
                .json_body(completed_run());
        });
        let resolved = policy(server.port());
        let runner = ApiRunner::new(server.base_url(), "tt_live_fixture", &resolved).unwrap();

        let outcome = runner.run(request(), &NoTools).await.unwrap();

        post.assert();
        assert_eq!(outcome.agent.run.id, "run-1");
        assert_eq!(outcome.provider, "openai");
        assert_eq!(outcome.model, "openai/gpt-5");
        assert_eq!(outcome.max_cash_micros, 500_000);
        assert!(!outcome.budget_breached());
    }

    #[test]
    fn requires_live_principal_and_exact_destination() {
        let resolved = policy(4321);
        assert!(matches!(
            ApiRunner::new("http://127.0.0.1:4321", "tt_test_fixture", &resolved),
            Err(ApiRunnerError::LivePrincipalRequired)
        ));
        assert!(matches!(
            ApiRunner::new("http://127.0.0.1:4322", "tt_live_fixture", &resolved),
            Err(ApiRunnerError::PolicyDenied {
                field: "network.allowed_destinations",
                ..
            })
        ));
        assert!(matches!(
            ApiRunner::new("http://example.com:4321", "tt_live_fixture", &resolved),
            Err(ApiRunnerError::InvalidGatewayUrl(_))
        ));
    }

    #[tokio::test]
    async fn denies_model_and_budget_before_dispatch() {
        let server = MockServer::start_async().await;
        let any = server.mock(|when, then| {
            when.any_request();
            then.status(500);
        });
        let resolved = policy(server.port());
        let runner = ApiRunner::new(server.base_url(), "tt_live_fixture", &resolved).unwrap();

        let mut bad_model = request();
        bad_model.model = "anthropic/claude-sonnet-4".into();
        assert!(matches!(
            runner.run(bad_model, &NoTools).await,
            Err(ApiRunnerError::PolicyDenied {
                field: "inference.allowed_providers",
                ..
            })
        ));

        let mut bad_budget = request();
        bad_budget.max_cash_micros += 1;
        assert!(matches!(
            runner.run(bad_budget, &NoTools).await,
            Err(ApiRunnerError::PolicyDenied {
                field: "budgets.max_api_cash_micros",
                ..
            })
        ));
        any.assert_calls(0);
    }

    #[tokio::test]
    async fn authenticated_runner_never_follows_redirects() {
        let server = MockServer::start_async().await;
        let target = server.mock(|when, then| {
            when.method(POST).path("/redirect-target");
            then.status(200).json_body(completed_run());
        });
        let redirect = server.mock(|when, then| {
            when.method(POST).path("/v1/agent/runs");
            then.status(307).header("location", "/redirect-target");
        });
        let resolved = policy(server.port());
        let runner = ApiRunner::new(server.base_url(), "tt_live_fixture", &resolved).unwrap();

        assert!(matches!(
            runner.run(request(), &NoTools).await,
            Err(ApiRunnerError::Gateway(tt_client::Error::Status {
                status: 307,
                ..
            }))
        ));
        redirect.assert();
        target.assert_calls(0);
    }

    #[tokio::test]
    async fn rejects_incomplete_cost_evidence() {
        let server = MockServer::start_async().await;
        server.mock(|when, then| {
            when.method(POST).path("/v1/agent/runs");
            let mut response = completed_run();
            response["usage"]["per_turn_cost_usd"] = json!([]);
            then.status(200)
                .header("content-type", "application/json")
                .json_body(response);
        });
        let resolved = policy(server.port());
        let runner = ApiRunner::new(server.base_url(), "tt_live_fixture", &resolved).unwrap();

        assert!(matches!(
            runner.run(request(), &NoTools).await,
            Err(ApiRunnerError::InvalidRunEvidence(_))
        ));
    }
}
