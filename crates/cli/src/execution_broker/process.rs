use std::{
    ffi::OsString,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
use std::net::TcpListener;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::process::Command as StdCommand;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncReadExt as _;

use crate::agent_policy::CommandRule;

use super::{sanitize_terminal, BrokerError, BrokerState};

const MAX_EXECUTABLE_BYTES: u64 = 128 * 1024 * 1024;
const OUTPUT_CHUNK_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBackend {
    NotRequired,
    MacOsSeatbelt,
    LinuxBubblewrap,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcessExecutionEvidence {
    pub sequence: u32,
    pub requested_executable: String,
    pub resolved_executable: String,
    pub executable_sha256: String,
    pub argv_sha256: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub output_limit_exceeded: bool,
    pub duration_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RunCommandArgs {
    executable: String,
    #[serde(default)]
    args: Vec<String>,
}

#[derive(Debug, Serialize)]
struct RunCommandOutput {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
    output_limit_exceeded: bool,
    executable_sha256: String,
    patch_changed_files: usize,
    patch_diff_bytes: u64,
}

pub(super) fn select_backend(
    repository: &Path,
    command_rules: &[CommandRule],
) -> Result<SandboxBackend, BrokerError> {
    if command_rules.is_empty() {
        return Ok(SandboxBackend::NotRequired);
    }

    #[cfg(target_os = "macos")]
    {
        probe_seatbelt(repository)?;
        return Ok(SandboxBackend::MacOsSeatbelt);
    }
    #[cfg(target_os = "linux")]
    {
        probe_bubblewrap()?;
        return Ok(SandboxBackend::LinuxBubblewrap);
    }
    #[allow(unreachable_code)]
    Err(BrokerError::SandboxUnavailable(
        "no reviewed local process sandbox exists for this platform".into(),
    ))
}

impl BrokerState {
    pub(super) async fn run_command(
        &mut self,
        args: RunCommandArgs,
    ) -> Result<String, BrokerError> {
        self.require_live()?;
        if !self.policy.policy.process.allow_shell && is_shell_executable(&args.executable) {
            return Err(BrokerError::policy(
                "process.allow_shell",
                format!("shell executable {:?} is not authorized", args.executable),
            ));
        }
        validate_argv(&args.executable, &args.args)?;
        let rule = matching_rule(
            &self.policy.policy.process.allowed_commands,
            &args.executable,
            &args.args,
        )?;
        if self.processes_started >= self.policy.policy.process.max_subprocesses {
            return Err(BrokerError::policy(
                "process.max_subprocesses",
                format!(
                    "already started {} commands; ceiling is {}",
                    self.processes_started, self.policy.policy.process.max_subprocesses
                ),
            ));
        }
        if self.backend == SandboxBackend::NotRequired {
            return Err(BrokerError::SandboxUnavailable(
                "process tool was not authorized when the broker was created".into(),
            ));
        }

        let resolved = resolve_executable(rule, &args.executable)?;
        let (command_root, command_repo, command_runtime) = self.workspace.command_workspace()?;
        let bin_dir = command_runtime.join("bin");
        let home_dir = command_runtime.join("home");
        let tmp_dir = command_runtime.join("tmp");
        std::fs::create_dir(&bin_dir)
            .and_then(|()| std::fs::create_dir(&home_dir))
            .and_then(|()| std::fs::create_dir(&tmp_dir))
            .map_err(|error| BrokerError::io("create command runtime", "runtime", error))?;
        let staged_executable = bin_dir.join(safe_executable_name(&args.executable)?);
        let executable_sha256 = copy_and_hash_executable(&resolved, &staged_executable)?;

        let remaining_wall = self.remaining_wall_time()?;
        let process_duration = Duration::from_secs(self.policy.policy.process.max_duration_seconds)
            .min(remaining_wall);
        if process_duration.is_zero() {
            return Err(BrokerError::policy(
                "process.max_duration_seconds",
                "must be nonzero for a command",
            ));
        }

        let max_file_bytes = self.policy.policy.filesystem.max_file_bytes;
        let output_ceiling = self
            .policy
            .policy
            .process
            .max_output_bytes
            .saturating_sub(self.output_bytes);
        if output_ceiling == 0 {
            return Err(BrokerError::policy(
                "process.max_output_bytes",
                "no output budget remains for a command",
            ));
        }
        let mut command = sandbox_command(
            self.backend,
            self.workspace.source_path(),
            command_root.path(),
            &command_repo,
            &command_runtime,
            &resolved,
            &staged_executable,
            &args.args,
            max_file_bytes,
            process_duration,
        )?;
        self.processes_started += 1;
        let sequence = self.processes_started;
        let started = Instant::now();
        let raw = run_bounded_child(&mut command, process_duration, output_ceiling).await?;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.output_bytes = self
            .output_bytes
            .saturating_add(raw.stdout.len() as u64)
            .saturating_add(raw.stderr.len() as u64);

        let argv_sha256 = hash_argv(&args.executable, &args.args);
        self.process_evidence.push(ProcessExecutionEvidence {
            sequence,
            requested_executable: args.executable,
            resolved_executable: resolved.display().to_string(),
            executable_sha256: executable_sha256.clone(),
            argv_sha256,
            exit_code: raw.exit_code,
            timed_out: raw.timed_out,
            output_limit_exceeded: raw.output_limit_exceeded,
            duration_ms,
        });
        let remaining_write = self
            .policy
            .policy
            .filesystem
            .max_total_write_bytes
            .saturating_sub(self.write_bytes);
        let (patch, command_write_bytes) = self
            .workspace
            .accept_command_workspace(&command_repo, remaining_write)?;
        self.write_bytes = self.write_bytes.saturating_add(command_write_bytes);

        serde_json::to_string(&RunCommandOutput {
            exit_code: raw.exit_code,
            stdout: sanitize_terminal(&raw.stdout),
            stderr: sanitize_terminal(&raw.stderr),
            timed_out: raw.timed_out,
            output_limit_exceeded: raw.output_limit_exceeded,
            executable_sha256,
            patch_changed_files: patch.changes.len(),
            patch_diff_bytes: patch.diff_bytes,
        })
        .map_err(BrokerError::serialize)
    }
}

fn matching_rule<'a>(
    rules: &'a [CommandRule],
    executable: &str,
    args: &[String],
) -> Result<&'a CommandRule, BrokerError> {
    rules
        .iter()
        .find(|rule| {
            rule.executable == executable
                && rule
                    .argv_prefixes
                    .iter()
                    .any(|prefix| args.starts_with(prefix))
        })
        .ok_or_else(|| {
            BrokerError::policy(
                "process.allowed_commands",
                format!("command {executable:?} with this argv is not authorized"),
            )
        })
}

fn validate_argv(executable: &str, args: &[String]) -> Result<(), BrokerError> {
    if executable.is_empty()
        || executable
            .chars()
            .any(|ch| ch == '\0' || ch == '\n' || ch == '\r' || ch.is_control())
    {
        return Err(BrokerError::policy(
            "process.allowed_commands.executable",
            "executable contains an empty or control-bearing value",
        ));
    }
    if args.iter().any(|arg| {
        arg.chars()
            .any(|ch| ch == '\0' || ch == '\n' || ch == '\r' || ch.is_control())
    }) {
        return Err(BrokerError::policy(
            "process.argv",
            "arguments containing control characters are refused",
        ));
    }
    Ok(())
}

fn is_shell_executable(executable: &str) -> bool {
    Path::new(executable)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "sh" | "bash"
                    | "dash"
                    | "zsh"
                    | "fish"
                    | "ksh"
                    | "csh"
                    | "tcsh"
                    | "pwsh"
                    | "powershell"
                    | "powershell.exe"
                    | "cmd.exe"
            )
        })
}

fn resolve_executable(rule: &CommandRule, requested: &str) -> Result<PathBuf, BrokerError> {
    let requested_path = Path::new(requested);
    let candidate = if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        let path = std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"));
        std::env::split_paths(&path)
            .filter(|directory| directory.is_absolute())
            .map(|directory| directory.join(requested))
            .find(|candidate| candidate.is_file())
            .ok_or_else(|| {
                BrokerError::policy(
                    "process.allowed_commands.executable",
                    format!("authorized executable {requested:?} was not found on PATH"),
                )
            })?
    };
    let resolved = std::fs::canonicalize(&candidate).map_err(|error| {
        BrokerError::io(
            "resolve executable",
            format!("command {:?}", rule.executable),
            error,
        )
    })?;
    if !resolved.is_absolute() || !resolved.is_file() {
        return Err(BrokerError::policy(
            "process.allowed_commands.executable",
            "resolved executable is not an absolute regular file",
        ));
    }
    Ok(resolved)
}

fn safe_executable_name(requested: &str) -> Result<String, BrokerError> {
    Path::new(requested)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            BrokerError::policy(
                "process.allowed_commands.executable",
                "executable has no safe file name",
            )
        })
}

fn copy_and_hash_executable(source: &Path, destination: &Path) -> Result<String, BrokerError> {
    let mut input = std::fs::File::open(source)
        .map_err(|error| BrokerError::io("open executable", "authorized command", error))?;
    let metadata = input
        .metadata()
        .map_err(|error| BrokerError::io("inspect executable", "authorized command", error))?;
    if !metadata.is_file() || metadata.len() > MAX_EXECUTABLE_BYTES {
        return Err(BrokerError::policy(
            "process.allowed_commands.executable",
            format!(
                "resolved executable must be a regular file no larger than {MAX_EXECUTABLE_BYTES} bytes"
            ),
        ));
    }
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| BrokerError::io("stage executable", "runtime", error))?;
    let mut digest = Sha256::new();
    let mut copied = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|error| BrokerError::io("read executable", "authorized command", error))?;
        if read == 0 {
            break;
        }
        copied = copied.saturating_add(read as u64);
        if copied > MAX_EXECUTABLE_BYTES {
            return Err(BrokerError::policy(
                "process.allowed_commands.executable",
                "executable grew beyond the copy ceiling",
            ));
        }
        digest.update(&buffer[..read]);
        output
            .write_all(&buffer[..read])
            .map_err(|error| BrokerError::io("stage executable", "runtime", error))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        output
            .set_permissions(std::fs::Permissions::from_mode(0o500))
            .map_err(|error| BrokerError::io("set executable mode", "runtime", error))?;
    }
    Ok(hex::encode(digest.finalize()))
}

#[allow(clippy::too_many_arguments)]
fn sandbox_command(
    backend: SandboxBackend,
    source_repo: &Path,
    command_root: &Path,
    command_repo: &Path,
    command_runtime: &Path,
    resolved_executable: &Path,
    staged_executable: &Path,
    args: &[String],
    max_file_bytes: u64,
    duration: Duration,
) -> Result<tokio::process::Command, BrokerError> {
    let file_blocks = max_file_bytes.saturating_add(511) / 512;
    let cpu_seconds = duration.as_secs().max(1);
    let limit_script =
        "ulimit -f \"$1\" || exit 125; ulimit -t \"$2\" || exit 125; shift 2; exec \"$@\"";

    let mut command = match backend {
        SandboxBackend::MacOsSeatbelt => {
            let profile = seatbelt_profile(source_repo, command_root)?;
            let mut command = tokio::process::Command::new("/usr/bin/sandbox-exec");
            command
                .arg("-p")
                .arg(profile)
                .arg("/bin/sh")
                .arg("-c")
                .arg(limit_script)
                .arg("tt-limit")
                .arg(file_blocks.to_string())
                .arg(cpu_seconds.to_string())
                .arg(resolved_executable)
                .args(args)
                .current_dir(command_repo);
            command
        }
        SandboxBackend::LinuxBubblewrap => {
            let mut command = tokio::process::Command::new("/usr/bin/bwrap");
            command.args([
                "--die-with-parent",
                "--new-session",
                "--unshare-pid",
                "--unshare-net",
                "--unshare-ipc",
                "--unshare-uts",
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--dir",
                "/etc",
            ]);
            for root in ["/usr", "/bin", "/lib", "/lib64"] {
                if Path::new(root).exists() {
                    command.args(["--ro-bind", root, root]);
                }
            }
            for root in ["/etc/ld.so.cache", "/etc/ssl", "/etc/ca-certificates"] {
                if Path::new(root).exists() {
                    command.args(["--ro-bind", root, root]);
                }
            }
            command
                .arg("--bind")
                .arg(command_repo)
                .arg("/workspace")
                .arg("--bind")
                .arg(command_runtime)
                .arg("/runtime")
                .args(["--chdir", "/workspace", "--"])
                .arg("/bin/sh")
                .arg("-c")
                .arg(limit_script)
                .arg("tt-limit")
                .arg(file_blocks.to_string())
                .arg(cpu_seconds.to_string())
                .arg(
                    Path::new("/runtime/bin").join(staged_executable.file_name().ok_or_else(
                        || BrokerError::policy("process", "missing executable name"),
                    )?),
                )
                .args(args);
            command
        }
        SandboxBackend::NotRequired => {
            return Err(BrokerError::SandboxUnavailable(
                "process sandbox was not initialized".into(),
            ))
        }
    };
    let (sandbox_home, sandbox_tmp) = match backend {
        SandboxBackend::LinuxBubblewrap => (
            PathBuf::from("/runtime/home"),
            PathBuf::from("/runtime/tmp"),
        ),
        SandboxBackend::MacOsSeatbelt | SandboxBackend::NotRequired => {
            (command_runtime.join("home"), command_runtime.join("tmp"))
        }
    };

    command
        .env_clear()
        .env("HOME", sandbox_home)
        .env("TMPDIR", sandbox_tmp)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .env("NO_PROXY", "*")
        .env("no_proxy", "*")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.as_std_mut().process_group(0);
    }
    Ok(command)
}

struct RawProcessResult {
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
    output_limit_exceeded: bool,
}

#[derive(Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

enum StreamMessage {
    Data(StreamKind, Vec<u8>),
    Closed,
}

async fn run_bounded_child(
    command: &mut tokio::process::Command,
    duration: Duration,
    output_ceiling: u64,
) -> Result<RawProcessResult, BrokerError> {
    let mut child = command
        .spawn()
        .map_err(|error| BrokerError::io("start sandboxed command", "authorized command", error))?;
    let pid = child.id();
    let stdout = child.stdout.take().ok_or_else(|| {
        BrokerError::SandboxProbeFailed("sandboxed command stdout was not piped".into())
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        BrokerError::SandboxProbeFailed("sandboxed command stderr was not piped".into())
    })?;
    let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
    let stdout_reader = tokio::spawn(read_stream(stdout, StreamKind::Stdout, sender.clone()));
    let stderr_reader = tokio::spawn(read_stream(stderr, StreamKind::Stderr, sender));

    let deadline = tokio::time::Instant::now() + duration;
    let hard_deadline = deadline + Duration::from_secs(2);
    let mut interval = tokio::time::interval(Duration::from_millis(10));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut raw_bytes = 0u64;
    let mut streams_open = 2u8;
    let mut status = None;
    let mut timed_out = false;
    let mut output_limit_exceeded = false;
    let mut terminated = false;

    while status.is_none() || streams_open > 0 {
        tokio::select! {
            () = tokio::time::sleep_until(hard_deadline) => {
                if !terminated {
                    timed_out = true;
                }
                terminate_process_group(&mut child, pid).await;
                break;
            }
            () = tokio::time::sleep_until(deadline), if !terminated => {
                timed_out = true;
                terminated = true;
                terminate_process_group(&mut child, pid).await;
            }
            _ = interval.tick() => {
                if status.is_none() {
                    status = child.try_wait().map_err(|error| {
                        BrokerError::io("wait for sandboxed command", "authorized command", error)
                    })?;
                }
            }
            message = receiver.recv(), if streams_open > 0 => {
                match message {
                    Some(StreamMessage::Closed) => streams_open = streams_open.saturating_sub(1),
                    Some(StreamMessage::Data(kind, chunk)) => {
                        let chunk_len = chunk.len() as u64;
                        let remaining = output_ceiling.saturating_sub(raw_bytes);
                        let accepted = usize::try_from(remaining.min(chunk_len)).unwrap_or(0);
                        match kind {
                            StreamKind::Stdout => stdout_bytes.extend_from_slice(&chunk[..accepted]),
                            StreamKind::Stderr => stderr_bytes.extend_from_slice(&chunk[..accepted]),
                        }
                        raw_bytes = raw_bytes.saturating_add(chunk_len);
                        if raw_bytes > output_ceiling && !terminated {
                            output_limit_exceeded = true;
                            terminated = true;
                            terminate_process_group(&mut child, pid).await;
                        }
                    }
                    None => streams_open = 0,
                }
            }
        }
    }
    if status.is_none() {
        terminate_process_group(&mut child, pid).await;
        status = match tokio::time::timeout(Duration::from_secs(1), child.wait()).await {
            Ok(Ok(status)) => Some(status),
            Ok(Err(error)) => {
                return Err(BrokerError::io(
                    "reap sandboxed command",
                    "authorized command",
                    error,
                ))
            }
            Err(_) => None,
        };
    }
    stdout_reader.abort();
    stderr_reader.abort();

    Ok(RawProcessResult {
        exit_code: status.and_then(|status| status.code()),
        stdout: stdout_bytes,
        stderr: stderr_bytes,
        timed_out,
        output_limit_exceeded,
    })
}

async fn read_stream<R>(
    mut stream: R,
    kind: StreamKind,
    sender: tokio::sync::mpsc::Sender<StreamMessage>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buffer = [0u8; OUTPUT_CHUNK_BYTES];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                if sender
                    .send(StreamMessage::Data(kind, buffer[..read].to_vec()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    }
    let _ = sender.send(StreamMessage::Closed).await;
}

async fn terminate_process_group(child: &mut tokio::process::Child, pid: Option<u32>) {
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
    let _ = child.start_kill();
}

fn hash_argv(executable: &str, args: &[String]) -> String {
    let mut digest = Sha256::new();
    digest.update(executable.as_bytes());
    for arg in args {
        digest.update([0]);
        digest.update(arg.as_bytes());
    }
    hex::encode(digest.finalize())
}

#[cfg(target_os = "macos")]
fn seatbelt_profile(source_repo: &Path, allowed_root: &Path) -> Result<String, BrokerError> {
    let source_repo = std::fs::canonicalize(source_repo)
        .map_err(|error| BrokerError::io("resolve repository", "repository", error))?;
    let allowed_root = std::fs::canonicalize(allowed_root)
        .map_err(|error| BrokerError::io("resolve command workspace", "runtime", error))?;
    Ok(format!(
        "(version 1)\n(deny default)\n(import \"system.sb\")\n(allow process*)\n(deny network*)\n(allow file-read* (subpath \"/bin\"))\n(allow file-read* (subpath \"/usr/bin\"))\n(allow file-read* (subpath \"/private/var/select\"))\n(deny file-read* file-write* (subpath {}))\n(allow file-read* file-write* (subpath {}))\n",
        seatbelt_string(&source_repo)?,
        seatbelt_string(&allowed_root)?,
    ))
}

#[cfg(not(target_os = "macos"))]
fn seatbelt_profile(_source_repo: &Path, _allowed_root: &Path) -> Result<String, BrokerError> {
    Err(BrokerError::SandboxUnavailable(
        "Seatbelt is available only on macOS".into(),
    ))
}

#[cfg(target_os = "macos")]
fn seatbelt_string(path: &Path) -> Result<String, BrokerError> {
    let path = path
        .to_str()
        .ok_or_else(|| BrokerError::policy("sandbox", "sandbox paths must be UTF-8"))?;
    if path
        .chars()
        .any(|ch| ch == '\0' || ch == '\n' || ch == '\r')
    {
        return Err(BrokerError::policy(
            "sandbox",
            "sandbox path contains a control character",
        ));
    }
    Ok(format!(
        "\"{}\"",
        path.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

#[cfg(target_os = "macos")]
fn probe_seatbelt(repository: &Path) -> Result<(), BrokerError> {
    if !Path::new("/usr/bin/sandbox-exec").is_file() {
        return Err(BrokerError::SandboxUnavailable(
            "/usr/bin/sandbox-exec is unavailable".into(),
        ));
    }
    let probe = tempfile::Builder::new()
        .prefix("tt-seatbelt-probe-")
        .tempdir()
        .map_err(|error| BrokerError::io("create sandbox probe", "runtime", error))?;
    let allowed = probe.path().join("allowed.txt");
    std::fs::write(&allowed, b"allowed")
        .map_err(|error| BrokerError::io("write sandbox probe", "runtime", error))?;
    let profile = seatbelt_profile(repository, probe.path())?;

    let allowed_output = StdCommand::new("/usr/bin/sandbox-exec")
        .arg("-p")
        .arg(&profile)
        .arg("/bin/cat")
        .arg(&allowed)
        .env_clear()
        .output()
        .map_err(|error| BrokerError::io("run sandbox probe", "runtime", error))?;
    if !allowed_output.status.success() || allowed_output.stdout != b"allowed" {
        return Err(BrokerError::SandboxProbeFailed(
            "Seatbelt did not permit the isolated workspace canary".into(),
        ));
    }

    let denied_output = StdCommand::new("/usr/bin/sandbox-exec")
        .arg("-p")
        .arg(&profile)
        .arg("/bin/ls")
        .arg(repository)
        .env_clear()
        .output()
        .map_err(|error| BrokerError::io("run sandbox probe", "runtime", error))?;
    if denied_output.status.success() {
        return Err(BrokerError::SandboxProbeFailed(
            "Seatbelt exposed the original repository".into(),
        ));
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| BrokerError::io("bind sandbox network probe", "runtime", error))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| BrokerError::io("configure sandbox network probe", "runtime", error))?;
    let port = listener
        .local_addr()
        .map_err(|error| BrokerError::io("inspect sandbox network probe", "runtime", error))?
        .port();
    let network_output = StdCommand::new("/usr/bin/sandbox-exec")
        .arg("-p")
        .arg(&profile)
        .arg("/usr/bin/nc")
        .args(["-w", "1", "127.0.0.1", &port.to_string()])
        .env_clear()
        .output()
        .map_err(|error| BrokerError::io("run sandbox network probe", "runtime", error))?;
    if network_output.status.success() || listener.accept().is_ok() {
        return Err(BrokerError::SandboxProbeFailed(
            "Seatbelt permitted a network connection".into(),
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn probe_bubblewrap() -> Result<(), BrokerError> {
    if !Path::new("/usr/bin/bwrap").is_file() {
        return Err(BrokerError::SandboxUnavailable(
            "/usr/bin/bwrap is required for process tools on Linux".into(),
        ));
    }
    let output = StdCommand::new("/usr/bin/bwrap")
        .args([
            "--die-with-parent",
            "--new-session",
            "--unshare-net",
            "--ro-bind",
            "/usr",
            "/usr",
            "--ro-bind",
            "/bin",
            "/bin",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--",
            "/bin/true",
        ])
        .env_clear()
        .output()
        .map_err(|error| BrokerError::io("run bubblewrap probe", "runtime", error))?;
    if !output.status.success() {
        return Err(BrokerError::SandboxProbeFailed(
            "bubblewrap namespace probe failed".into(),
        ));
    }
    Ok(())
}
