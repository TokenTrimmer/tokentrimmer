//! External authorization and isolation boundary for the local coding agent.
//!
//! Model tool calls are untrusted requests. This broker snapshots only approved
//! repository files into a run-scoped capability directory, stages every write
//! away from the source checkout, executes allowlisted argv without model-built
//! shell strings inside a probed OS sandbox, pins network requests to one
//! validated DNS result, and enforces wall/output/read/write/diff ceilings.

mod filesystem;
mod network;
mod process;

use std::{io, path::Path, time::Duration};

pub use filesystem::{FileChange, FileChangeKind, PatchSet};
pub use network::NetworkFetchEvidence;
pub use process::{ProcessExecutionEvidence, SandboxBackend};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use tokio::sync::Mutex;
use tt_client::{Tool, ToolExecutor};

use crate::agent_policy::{AgentPolicyError, ResolvedAgentPolicy};
use filesystem::Workspace;

pub const READ_FILE_TOOL: &str = "read_file";
pub const LIST_FILES_TOOL: &str = "list_files";
pub const WRITE_FILE_TOOL: &str = "write_file";
pub const RUN_COMMAND_TOOL: &str = "run_command";
pub const FETCH_URL_TOOL: &str = "fetch_url";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PolicyDecision {
    pub sequence: u32,
    pub tool: String,
    pub action_sha256: String,
    pub allowed: bool,
    pub reason_code: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BrokerEvidence {
    pub run_id: String,
    pub policy_sha256: String,
    pub repository_snapshot_sha256: String,
    pub sandbox_backend: SandboxBackend,
    pub source_checkout_modified: bool,
    pub read_bytes_returned: u64,
    pub write_bytes_staged: u64,
    pub output_bytes_returned: u64,
    pub policy_decisions: Vec<PolicyDecision>,
    pub process_executions: Vec<ProcessExecutionEvidence>,
    pub network_fetches: Vec<NetworkFetchEvidence>,
    pub patch: PatchSet,
}

#[derive(Debug, Error)]
pub enum BrokerError {
    #[error("agent policy is invalid: {0}")]
    InvalidPolicy(#[from] AgentPolicyError),
    #[error("policy denied {field}: {detail}")]
    PolicyDenied { field: &'static str, detail: String },
    #[error("unknown local tool {0:?}")]
    UnknownTool(String),
    #[error("invalid arguments for {tool}: {detail}")]
    InvalidArguments { tool: String, detail: String },
    #[error("{operation} failed for {path}")]
    Io {
        operation: &'static str,
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("local process sandbox unavailable: {0}")]
    SandboxUnavailable(String),
    #[error("local process sandbox acceptance probe failed: {0}")]
    SandboxProbeFailed(String),
    #[error("run wall-time ceiling reached")]
    WallTimeExceeded,
    #[error("bounded network request failed")]
    Network(#[source] reqwest::Error),
    #[error("bounded DNS lookup failed")]
    Dns(#[source] io::Error),
    #[error("failed to serialize tool evidence")]
    Serialize(#[source] serde_json::Error),
}

impl BrokerError {
    pub(super) fn policy(field: &'static str, detail: impl Into<String>) -> Self {
        Self::PolicyDenied {
            field,
            detail: detail.into(),
        }
    }

    pub(super) fn io(operation: &'static str, path: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }

    pub(super) fn network(source: reqwest::Error) -> Self {
        Self::Network(source)
    }

    pub(super) fn dns(source: io::Error) -> Self {
        Self::Dns(source)
    }

    pub(super) fn serialize(source: serde_json::Error) -> Self {
        Self::Serialize(source)
    }

    fn reason_code(&self) -> &'static str {
        match self {
            Self::InvalidPolicy(_) => "invalid_policy",
            Self::PolicyDenied { .. } => "policy_denied",
            Self::UnknownTool(_) => "unknown_tool",
            Self::InvalidArguments { .. } => "invalid_arguments",
            Self::Io { .. } => "io_error",
            Self::SandboxUnavailable(_) => "sandbox_unavailable",
            Self::SandboxProbeFailed(_) => "sandbox_probe_failed",
            Self::WallTimeExceeded => "wall_time_exceeded",
            Self::Network(_) => "network_error",
            Self::Dns(_) => "dns_error",
            Self::Serialize(_) => "serialization_error",
        }
    }
}

/// A run-scoped tool broker. The original repository is opened only while
/// constructing the bounded snapshot; all model-visible mutations stay staged.
pub struct LocalExecutionBroker {
    state: Mutex<BrokerState>,
}

pub(super) struct BrokerState {
    run_id: String,
    policy: ResolvedAgentPolicy,
    started: std::time::Instant,
    workspace: Workspace,
    backend: SandboxBackend,
    read_bytes: u64,
    write_bytes: u64,
    output_bytes: u64,
    processes_started: u32,
    decisions: Vec<PolicyDecision>,
    process_evidence: Vec<ProcessExecutionEvidence>,
    network_evidence: Vec<NetworkFetchEvidence>,
}

impl LocalExecutionBroker {
    pub fn new(
        repository: impl AsRef<Path>,
        run_id: impl Into<String>,
        policy: &ResolvedAgentPolicy,
    ) -> Result<Self, BrokerError> {
        let started = std::time::Instant::now();
        policy.policy.validate()?;
        let run_id = run_id.into();
        if run_id.is_empty() || run_id.chars().any(|ch| ch == '\0' || ch.is_control()) {
            return Err(BrokerError::policy(
                "run_id",
                "run identity must be nonempty and contain no control characters",
            ));
        }
        if policy.policy.limits.max_wall_time_seconds == 0 {
            return Err(BrokerError::policy(
                "limits.max_wall_time_seconds",
                "must be nonzero for a local run",
            ));
        }

        let repository = repository.as_ref();
        let backend = process::select_backend(repository, &policy.policy.process.allowed_commands)?;
        let workspace =
            Workspace::new(repository, &policy.policy.filesystem, &policy.policy.limits)?;
        if started.elapsed() >= Duration::from_secs(policy.policy.limits.max_wall_time_seconds) {
            return Err(BrokerError::WallTimeExceeded);
        }
        Ok(Self {
            state: Mutex::new(BrokerState {
                run_id,
                policy: policy.clone(),
                started,
                workspace,
                backend,
                read_bytes: 0,
                write_bytes: 0,
                output_bytes: 0,
                processes_started: 0,
                decisions: Vec::new(),
                process_evidence: Vec::new(),
                network_evidence: Vec::new(),
            }),
        })
    }

    /// Advertise only capabilities with nonempty authority and nonzero resource
    /// ceilings. The broker still rechecks every concrete call.
    pub fn tool_definitions(policy: &ResolvedAgentPolicy) -> Vec<Tool> {
        let mut tools = Vec::new();
        let filesystem = &policy.policy.filesystem;
        let process = &policy.policy.process;
        if !filesystem.readable_roots.is_empty()
            && filesystem.max_files > 0
            && filesystem.max_file_bytes > 0
            && filesystem.max_total_read_bytes > 0
            && process.max_output_bytes > 0
        {
            tools.push(tt_client::tool(
                READ_FILE_TOOL,
                "Read one authorized UTF-8 repository file from the isolated run snapshot.",
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["path"],
                    "properties": { "path": { "type": "string", "minLength": 1 } }
                }),
            ));
            tools.push(tt_client::tool(
                LIST_FILES_TOOL,
                "List authorized files beneath one repository-relative path in the isolated snapshot.",
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["path"],
                    "properties": { "path": { "type": "string", "minLength": 1 } }
                }),
            ));
        }
        if !filesystem.writable_roots.is_empty()
            && filesystem.max_file_bytes > 0
            && filesystem.max_total_write_bytes > 0
            && policy.policy.limits.max_changed_files > 0
            && policy.policy.limits.max_diff_bytes > 0
        {
            tools.push(tt_client::tool(
                WRITE_FILE_TOOL,
                "Stage one complete UTF-8 file in the isolated run workspace; never writes the source checkout.",
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["path", "content"],
                    "properties": {
                        "path": { "type": "string", "minLength": 1 },
                        "content": { "type": "string" }
                    }
                }),
            ));
        }
        if !process.allowed_commands.is_empty()
            && process.max_subprocesses > 0
            && process.max_duration_seconds > 0
            && process.max_output_bytes > 0
        {
            tools.push(tt_client::tool(
                RUN_COMMAND_TOOL,
                "Run one policy-allowlisted executable and argv in an isolated no-network workspace. No shell interpolation.",
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["executable", "args"],
                    "properties": {
                        "executable": { "type": "string", "minLength": 1 },
                        "args": { "type": "array", "items": { "type": "string" } }
                    }
                }),
            ));
        }
        if !policy.policy.network.allowed_destinations.is_empty() && process.max_output_bytes > 0 {
            tools.push(tt_client::tool(
                FETCH_URL_TOOL,
                "GET one exact policy-authorized HTTP(S) URL through pinned DNS and bounded output.",
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["url"],
                    "properties": { "url": { "type": "string", "minLength": 1 } }
                }),
            ));
        }
        tools
    }

    pub async fn workspace_path(&self) -> std::path::PathBuf {
        self.state.lock().await.workspace.workspace_path()
    }

    pub async fn evidence(&self) -> Result<BrokerEvidence, BrokerError> {
        let state = self.state.lock().await;
        Ok(BrokerEvidence {
            run_id: state.run_id.clone(),
            policy_sha256: state.policy.effective_sha256.clone(),
            repository_snapshot_sha256: state.workspace.baseline_sha256(),
            sandbox_backend: state.backend,
            source_checkout_modified: false,
            read_bytes_returned: state.read_bytes,
            write_bytes_staged: state.write_bytes,
            output_bytes_returned: state.output_bytes,
            policy_decisions: state.decisions.clone(),
            process_executions: state.process_evidence.clone(),
            network_fetches: state.network_evidence.clone(),
            patch: state.workspace.patch()?,
        })
    }
}

impl BrokerState {
    pub(super) fn remaining_wall_time(&self) -> Result<Duration, BrokerError> {
        let ceiling = Duration::from_secs(self.policy.policy.limits.max_wall_time_seconds);
        ceiling
            .checked_sub(self.started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(BrokerError::WallTimeExceeded)
    }

    pub(super) fn require_live(&self) -> Result<(), BrokerError> {
        self.remaining_wall_time().map(|_| ())
    }

    fn read_file(&mut self, args: PathArgs) -> Result<String, BrokerError> {
        self.require_live()?;
        let content = self.workspace.read_text(&args.path)?;
        let bytes = content.len() as u64;
        let read_ceiling = self.policy.policy.filesystem.max_total_read_bytes;
        if self.read_bytes.saturating_add(bytes) > read_ceiling {
            return Err(BrokerError::policy(
                "filesystem.max_total_read_bytes",
                format!(
                    "read would raise returned bytes to {}; ceiling is {read_ceiling}",
                    self.read_bytes.saturating_add(bytes)
                ),
            ));
        }
        self.reserve_output(bytes)?;
        self.read_bytes = self.read_bytes.saturating_add(bytes);
        serde_json::to_string(&ReadFileOutput {
            path: args.path,
            content: sanitize_terminal(content.as_bytes()),
        })
        .map_err(BrokerError::serialize)
    }

    fn list_files(&mut self, args: PathArgs) -> Result<String, BrokerError> {
        self.require_live()?;
        let files = self.workspace.list_files(&args.path)?;
        let encoded =
            serde_json::to_string(&ListFilesOutput { files }).map_err(BrokerError::serialize)?;
        self.reserve_output(encoded.len() as u64)?;
        Ok(encoded)
    }

    fn write_file(&mut self, args: WriteFileArgs) -> Result<String, BrokerError> {
        self.require_live()?;
        let bytes = args.content.len() as u64;
        let write_ceiling = self.policy.policy.filesystem.max_total_write_bytes;
        if self.write_bytes.saturating_add(bytes) > write_ceiling {
            return Err(BrokerError::policy(
                "filesystem.max_total_write_bytes",
                format!(
                    "write would raise staged bytes to {}; ceiling is {write_ceiling}",
                    self.write_bytes.saturating_add(bytes)
                ),
            ));
        }
        let path = args.path.clone();
        let patch = self.workspace.write_text(&args.path, args.content)?;
        self.write_bytes = self.write_bytes.saturating_add(bytes);
        serde_json::to_string(&WriteFileOutput {
            path,
            staged_bytes: bytes,
            changed_files: patch.changes.len(),
            diff_bytes: patch.diff_bytes,
        })
        .map_err(BrokerError::serialize)
    }

    fn reserve_output(&mut self, bytes: u64) -> Result<(), BrokerError> {
        let ceiling = self.policy.policy.process.max_output_bytes;
        if self.output_bytes.saturating_add(bytes) > ceiling {
            return Err(BrokerError::policy(
                "process.max_output_bytes",
                format!(
                    "tool output would raise returned bytes to {}; ceiling is {ceiling}",
                    self.output_bytes.saturating_add(bytes)
                ),
            ));
        }
        self.output_bytes = self.output_bytes.saturating_add(bytes);
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PathArgs {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteFileArgs {
    path: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct ReadFileOutput {
    path: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct ListFilesOutput {
    files: Vec<String>,
}

#[derive(Debug, Serialize)]
struct WriteFileOutput {
    path: String,
    staged_bytes: u64,
    changed_files: usize,
    diff_bytes: u64,
}

#[tt_client::async_trait]
impl ToolExecutor for LocalExecutionBroker {
    async fn call(
        &self,
        name: &str,
        arguments: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let action_sha256 = action_hash(name, arguments);
        let mut state = self.state.lock().await;
        let result = match name {
            READ_FILE_TOOL => parse_args(name, arguments).and_then(|args| state.read_file(args)),
            LIST_FILES_TOOL => parse_args(name, arguments).and_then(|args| state.list_files(args)),
            WRITE_FILE_TOOL => parse_args(name, arguments).and_then(|args| state.write_file(args)),
            RUN_COMMAND_TOOL => match parse_args(name, arguments) {
                Ok(args) => state.run_command(args).await,
                Err(error) => Err(error),
            },
            FETCH_URL_TOOL => match parse_args(name, arguments) {
                Ok(args) => state.fetch_url(args).await,
                Err(error) => Err(error),
            },
            _ => Err(BrokerError::UnknownTool(name.to_string())),
        };
        let (allowed, reason_code) = match &result {
            Ok(_) => (true, "authorized"),
            Err(error) => (false, error.reason_code()),
        };
        let sequence = state.decisions.len() as u32 + 1;
        state.decisions.push(PolicyDecision {
            sequence,
            tool: name.to_string(),
            action_sha256,
            allowed,
            reason_code: reason_code.to_string(),
        });
        result.map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)
    }
}

fn parse_args<T: for<'de> Deserialize<'de>>(tool: &str, arguments: &str) -> Result<T, BrokerError> {
    serde_json::from_str(arguments).map_err(|error| BrokerError::InvalidArguments {
        tool: tool.to_string(),
        detail: error.to_string(),
    })
}

fn action_hash(name: &str, arguments: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"tokentrimmer-agent-tool:v1\0");
    digest.update(name.as_bytes());
    digest.update([0]);
    digest.update(arguments.as_bytes());
    hex::encode(digest.finalize())
}

pub(super) fn sanitize_terminal(bytes: &[u8]) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Normal,
        Escape,
        Csi,
        Osc,
        OscEscape,
    }

    let text = String::from_utf8_lossy(bytes);
    let mut output = String::with_capacity(text.len());
    let mut state = State::Normal;
    for character in text.chars() {
        state = match state {
            State::Normal if character == '\u{1b}' => State::Escape,
            State::Normal => {
                if !character.is_control() || matches!(character, '\n' | '\r' | '\t') {
                    output.push(character);
                }
                State::Normal
            }
            State::Escape if character == '[' => State::Csi,
            State::Escape if character == ']' => State::Osc,
            State::Escape => State::Normal,
            State::Csi if ('@'..='~').contains(&character) => State::Normal,
            State::Csi => State::Csi,
            State::Osc if character == '\u{7}' => State::Normal,
            State::Osc if character == '\u{1b}' => State::OscEscape,
            State::Osc => State::Osc,
            State::OscEscape if character == '\\' => State::Normal,
            State::OscEscape if character == '\u{1b}' => State::OscEscape,
            State::OscEscape => State::Osc,
        };
    }
    output
}

#[cfg(test)]
mod tests;
