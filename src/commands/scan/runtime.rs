//! Run a native reviewer with no project context and a bounded, private output channel.

use anyhow::{Result, anyhow, bail, ensure};
use serde::Deserialize;
use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

const MAX_PROMPT_BYTES: usize = 3 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_HELP_BYTES: usize = 128 * 1024;
const REVIEW_TIMEOUT: Duration = Duration::from_secs(120);
const HELP_TIMEOUT: Duration = Duration::from_secs(10);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(4);

pub(super) fn review(prompt: &str, schema: &Value) -> Result<Vec<u8>> {
    ensure!(
        prompt.len() <= MAX_PROMPT_BYTES,
        "sensitive review input exceeds its byte limit"
    );
    let configured = crate::infra::config::get_global("runtime.default")
        .map_err(|_| anyhow!("cannot read the configured review runtime"))?
        .unwrap_or_else(|| "claude-code".to_owned());
    let selected = crate::adapter::normalize(&configured)
        .map_err(|_| anyhow!("the configured review runtime is not supported"))?;
    ensure!(
        selected == "claude-code",
        "the configured runtime does not provide an isolated sensitive review"
    );
    let environment = review_environment(std::env::vars_os());
    let program = resolve_claude(&environment)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| anyhow!("cannot initialize the review runtime"))?
        .block_on(review_claude(&program, &environment, prompt, schema))
}

fn resolve_claude(environment: &[(OsString, OsString)]) -> Result<String> {
    let path = std::env::var_os("PATH")
        .ok_or_else(|| anyhow!("cannot locate Claude Code on an absolute executable path"))?;
    let lookup = [(OsString::from("PATH"), path)];
    let cwd = std::env::current_dir()
        .map_err(|_| anyhow!("cannot locate the review working directory"))?;
    let installation = home_installation(&cwd, environment);
    resolve_claude_outside(&lookup, workspace_root().as_deref(), installation.as_ref())
}

struct HomeInstallation {
    lexical: std::path::PathBuf,
    canonical: std::path::PathBuf,
}

fn home_installation(cwd: &Path, environment: &[(OsString, OsString)]) -> Option<HomeInstallation> {
    let cwd = cwd.canonicalize().ok()?;
    if cwd.ancestors().any(|path| path.join(".git").exists()) {
        return None;
    }
    let home_key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let lexical = std::path::PathBuf::from(&environment.iter().find(|(key, _)| key == home_key)?.1);
    if !lexical.is_absolute() {
        return None;
    }
    let canonical = lexical.canonicalize().ok()?;
    (cwd == canonical).then_some(HomeInstallation { lexical, canonical })
}

fn native_installation_entry(candidate: &Path, resolved: &Path, home: &HomeInstallation) -> bool {
    let name = if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    };
    let relative = Path::new(".local").join("bin").join(name);
    if candidate != home.lexical.join(&relative) && candidate != home.canonical.join(&relative) {
        return false;
    }
    if candidate
        .components()
        .any(|part| part == std::path::Component::ParentDir)
    {
        return false;
    }
    let regular_directory = |relative: &str| {
        let path = home.canonical.join(relative);
        std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir())
            && path.canonicalize().is_ok_and(|canonical| canonical == path)
    };
    if !regular_directory(".local") || !regular_directory(".local/bin") {
        return false;
    }
    let entry = home.canonical.join(relative);
    let Ok(metadata) = std::fs::symlink_metadata(&entry) else {
        return false;
    };
    if metadata.is_file() {
        return resolved == entry;
    }
    if !metadata.file_type().is_symlink()
        || ![
            ".local/share",
            ".local/share/claude",
            ".local/share/claude/versions",
        ]
        .iter()
        .all(|path| regular_directory(path))
    {
        return false;
    }
    let Ok(link) = std::fs::read_link(&entry) else {
        return false;
    };
    // The recognized installation link names a regular versioned binary directly; redirected
    // installation directories or extra link hops must not authorize a project executable.
    let version = if link.is_absolute() {
        link
    } else {
        entry.parent().unwrap().join(link)
    };
    let versions = home.canonical.join(".local/share/claude/versions");
    version.parent() == Some(versions.as_path())
        && std::fs::symlink_metadata(&version).is_ok_and(|metadata| metadata.is_file())
        && resolved == version
}

fn resolve_claude_outside(
    environment: &[(OsString, OsString)],
    workspace: Option<&Path>,
    installation: Option<&HomeInstallation>,
) -> Result<String> {
    let path = environment
        .iter()
        .find(|(key, _)| key == "PATH")
        .map(|(_, value)| value)
        .ok_or_else(|| anyhow!("cannot locate Claude Code on an absolute executable path"))?;
    let names = if cfg!(windows) {
        &["claude.exe", "claude.cmd"][..]
    } else {
        &["claude"][..]
    };
    for directory in std::env::split_paths(path).filter(|directory| directory.is_absolute()) {
        for name in names {
            let candidate = directory.join(name);
            let Ok(metadata) = candidate.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 == 0 {
                    continue;
                }
            }
            let resolved = candidate
                .canonicalize()
                .map_err(|_| anyhow!("cannot resolve the Claude Code executable"))?;
            let excluded = workspace.is_some_and(|workspace| {
                candidate.starts_with(workspace)
                    || directory
                        .canonicalize()
                        .is_ok_and(|path| path.starts_with(workspace))
                    || resolved.starts_with(workspace)
            });
            if excluded
                && !installation
                    .is_some_and(|home| native_installation_entry(&candidate, &resolved, home))
            {
                continue;
            }
            return resolved
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("cannot resolve the Claude Code executable"));
        }
    }
    bail!("cannot locate Claude Code on an absolute executable path")
}

async fn review_claude(
    program: &str,
    environment: &[(OsString, OsString)],
    prompt: &str,
    schema: &Value,
) -> Result<Vec<u8>> {
    let mut cancellation = Cancellation::new()?;
    let directory = tempfile::Builder::new()
        .prefix("agit-sensitive-review-")
        .tempdir()
        .map_err(|_| anyhow!("cannot create an isolated review directory"))?;
    let agent_home = directory.path().join("agent-state");
    std::fs::create_dir(&agent_home)
        .map_err(|_| anyhow!("cannot create temporary review state"))?;
    let mut environment = environment.to_vec();
    environment.retain(|(key, _)| canonical_environment_key(key.clone()) != "AGIT_HOME");
    environment.push(("AGIT_HOME".into(), agent_home.into_os_string()));
    let help = execute_cancellable(
        program,
        &["--safe-mode".into(), "--help".into()],
        directory.path(),
        &environment,
        b"",
        HELP_TIMEOUT,
        MAX_HELP_BYTES,
        cancellation.wait(),
    )
    .await?;
    let help = std::str::from_utf8(&help)
        .map_err(|_| anyhow!("the installed review runtime has invalid capability output"))?;
    let args = claude_arguments(schema)?;
    for option in args.iter().filter(|arg| arg.starts_with("--")) {
        ensure!(
            help.split_whitespace().any(|word| word == option),
            "the installed Claude runtime lacks a required isolation option; update Claude Code"
        );
    }
    let output = execute_cancellable(
        program,
        &args,
        directory.path(),
        &environment,
        prompt.as_bytes(),
        REVIEW_TIMEOUT,
        MAX_OUTPUT_BYTES,
        cancellation.wait(),
    )
    .await?;
    parse_claude_result(&output)
}

fn claude_arguments(schema: &Value) -> Result<Vec<String>> {
    Ok(vec![
        "--print".into(),
        "--safe-mode".into(),
        // Native managed policy remains authoritative over the ordinary hook setting.
        "--settings".into(),
        r#"{"disableAllHooks":true}"#.into(),
        "--tools".into(),
        String::new(),
        "--disable-slash-commands".into(),
        "--strict-mcp-config".into(),
        "--mcp-config".into(),
        r#"{"mcpServers":{}}"#.into(),
        "--no-chrome".into(),
        "--no-session-persistence".into(),
        "--permission-mode".into(),
        "dontAsk".into(),
        "--permission-prompts".into(),
        "none".into(),
        "--output-format".into(),
        "json".into(),
        "--json-schema".into(),
        serde_json::to_string(schema)
            .map_err(|_| anyhow!("cannot encode the review output schema"))?,
    ])
}

fn review_environment(
    source: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    let workspace = workspace_root();
    review_environment_outside(source, workspace.as_deref())
}

fn review_environment_outside(
    source: impl IntoIterator<Item = (OsString, OsString)>,
    workspace: Option<&Path>,
) -> Vec<(OsString, OsString)> {
    source
        .into_iter()
        .map(|(key, value)| (canonical_environment_key(key), value))
        .filter(|(key, _)| environment_key_allowed(key))
        .filter_map(|(key, value)| {
            if key != "PATH" {
                return Some((key, value));
            }
            let directories = std::env::split_paths(&value).filter(|path| {
                path.is_absolute()
                    && workspace.is_none_or(|workspace| {
                        path.canonicalize()
                            .is_ok_and(|path| !path.starts_with(workspace))
                    })
            });
            std::env::join_paths(directories)
                .ok()
                .map(|value| (key, value))
        })
        .collect()
}

fn workspace_root() -> Option<std::path::PathBuf> {
    let cwd = std::env::current_dir().ok()?.canonicalize().ok()?;
    Some(
        cwd.ancestors()
            .find(|path| path.join(".git").exists())
            .unwrap_or(&cwd)
            .to_owned(),
    )
}

fn canonical_environment_key(key: OsString) -> OsString {
    #[cfg(windows)]
    if let Some(key) = key.to_str() {
        return key.to_ascii_uppercase().into();
    }
    key
}

fn environment_key_allowed(key: &OsStr) -> bool {
    matches!(
        key.to_str(),
        Some(
            "HOME"
                | "USERPROFILE"
                | "SYSTEMROOT"
                | "SystemRoot"
                | "PATH"
                | "PATHEXT"
                | "CLAUDE_CONFIG_DIR"
                | "XDG_CONFIG_HOME"
                | "ANTHROPIC_API_KEY"
                | "ANTHROPIC_AUTH_TOKEN"
                | "ANTHROPIC_BASE_URL"
                | "ANTHROPIC_CUSTOM_HEADERS"
                | "ANTHROPIC_MODEL"
                | "ANTHROPIC_DEFAULT_MODEL"
                | "ANTHROPIC_DEFAULT_OPUS_MODEL"
                | "ANTHROPIC_DEFAULT_SONNET_MODEL"
                | "ANTHROPIC_DEFAULT_HAIKU_MODEL"
                | "ANTHROPIC_DEFAULT_FABLE_MODEL"
                | "ANTHROPIC_SMALL_FAST_MODEL"
                | "CLAUDE_CODE_CLIENT_CERT"
                | "CLAUDE_CODE_CLIENT_KEY"
                | "CLAUDE_CODE_CLIENT_KEY_PASSPHRASE"
                | "CLAUDE_CODE_OAUTH_TOKEN"
                | "CLAUDE_CODE_USE_BEDROCK"
                | "CLAUDE_CODE_SKIP_BEDROCK_AUTH"
                | "ANTHROPIC_BEDROCK_BASE_URL"
                | "ANTHROPIC_BEDROCK_REGION_PREFIX"
                | "ANTHROPIC_BEDROCK_SERVICE_TIER"
                | "ANTHROPIC_SMALL_FAST_MODEL_AWS_REGION"
                | "CLAUDE_CODE_USE_VERTEX"
                | "CLAUDE_CODE_SKIP_VERTEX_AUTH"
                | "ANTHROPIC_VERTEX_BASE_URL"
                | "CLAUDE_CODE_USE_FOUNDRY"
                | "CLAUDE_CODE_SKIP_FOUNDRY_AUTH"
                | "AWS_ACCESS_KEY_ID"
                | "AWS_SECRET_ACCESS_KEY"
                | "AWS_SESSION_TOKEN"
                | "AWS_REGION"
                | "AWS_DEFAULT_REGION"
                | "AWS_PROFILE"
                | "AWS_CONFIG_FILE"
                | "AWS_SHARED_CREDENTIALS_FILE"
                | "AWS_BEARER_TOKEN_BEDROCK"
                | "AWS_CA_BUNDLE"
                | "AWS_ROLE_ARN"
                | "AWS_ROLE_SESSION_NAME"
                | "AWS_WEB_IDENTITY_TOKEN_FILE"
                | "AWS_CONTAINER_CREDENTIALS_FULL_URI"
                | "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"
                | "AWS_CONTAINER_AUTHORIZATION_TOKEN"
                | "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE"
                | "AWS_EC2_METADATA_DISABLED"
                | "AWS_ENDPOINT_URL"
                | "AWS_ENDPOINT_URL_BEDROCK"
                | "AWS_ENDPOINT_URL_BEDROCK_RUNTIME"
                | "AWS_ENDPOINT_URL_STS"
                | "GOOGLE_APPLICATION_CREDENTIALS"
                | "ANTHROPIC_VERTEX_PROJECT_ID"
                | "CLOUD_ML_REGION"
                | "GOOGLE_CLOUD_PROJECT"
                | "GOOGLE_CLOUD_QUOTA_PROJECT"
                | "GCLOUD_PROJECT"
                | "CLOUDSDK_CONFIG"
                | "ANTHROPIC_FOUNDRY_API_KEY"
                | "ANTHROPIC_FOUNDRY_AUTH_TOKEN"
                | "ANTHROPIC_FOUNDRY_RESOURCE"
                | "ANTHROPIC_FOUNDRY_BASE_URL"
                | "AZURE_CLIENT_ID"
                | "AZURE_TENANT_ID"
                | "AZURE_CLIENT_SECRET"
                | "AZURE_CLIENT_CERTIFICATE_PATH"
                | "AZURE_CLIENT_CERTIFICATE_PASSWORD"
                | "AZURE_CLIENT_SEND_CERTIFICATE_CHAIN"
                | "AZURE_FEDERATED_TOKEN_FILE"
                | "AZURE_AUTHORITY_HOST"
                | "AZURE_CONFIG_DIR"
                | "AZURE_TOKEN_CREDENTIALS"
                | "HTTP_PROXY"
                | "HTTPS_PROXY"
                | "ALL_PROXY"
                | "NO_PROXY"
                | "http_proxy"
                | "https_proxy"
                | "all_proxy"
                | "no_proxy"
                | "NODE_EXTRA_CA_CERTS"
                | "SSL_CERT_FILE"
                | "SSL_CERT_DIR"
        )
    )
}

#[derive(Deserialize)]
struct ClaudeResult {
    #[serde(rename = "type")]
    kind: String,
    subtype: String,
    is_error: bool,
    structured_output: Value,
    #[serde(default)]
    permission_denials: Vec<Value>,
    #[serde(default)]
    errors: Vec<Value>,
    #[serde(default)]
    stop_reason: Option<String>,
}

fn parse_claude_result(output: &[u8]) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(output)
        .map_err(|_| anyhow!("review runtime returned invalid UTF-8"))?;
    let value = super::json::parse(text)
        .map_err(|_| anyhow!("review runtime returned an invalid result envelope"))?;
    ensure!(
        !contains_tool_activity(&value),
        "review runtime attempted a tool action"
    );
    let result: ClaudeResult = serde_json::from_value(value)
        .map_err(|_| anyhow!("review runtime returned an invalid result envelope"))?;
    ensure!(
        result.kind == "result"
            && result.subtype == "success"
            && !result.is_error
            && result.permission_denials.is_empty()
            && result.errors.is_empty()
            && result
                .stop_reason
                .as_deref()
                .is_none_or(|reason| reason == "end_turn"),
        "review runtime did not finish a successful review"
    );
    ensure!(
        result.structured_output.is_object(),
        "review runtime did not return a structured classification report"
    );
    serde_json::to_vec(&result.structured_output)
        .map_err(|_| anyhow!("cannot decode the review classification report"))
}

fn contains_tool_activity(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(contains_tool_activity),
        Value::Object(fields) => {
            fields
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| matches!(kind, "tool_use" | "tool_result" | "control_request"))
                || fields.iter().any(|(key, value)| {
                    (matches!(
                        key.as_str(),
                        "tool_calls" | "tool_use" | "tool_uses" | "tool_results"
                    ) && value != &Value::Null
                        && value.as_array().is_none_or(|items| !items.is_empty()))
                        || contains_tool_activity(value)
                })
        }
        _ => false,
    }
}

struct ReviewProcess {
    child: Child,
    #[cfg(unix)]
    pgid: Option<i32>,
    #[cfg(windows)]
    job: Option<crate::rc::windows_job::Job>,
}

impl ReviewProcess {
    fn gone(&mut self) -> bool {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            return false;
        }
        #[cfg(unix)]
        {
            self.pgid.is_none_or(|pgid| {
                let result = unsafe { libc::killpg(pgid, 0) };
                result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            })
        }
        #[cfg(windows)]
        {
            self.job
                .as_ref()
                .is_some_and(|job| job.active_processes().is_ok_and(|count| count == 0))
        }
        #[cfg(not(any(unix, windows)))]
        {
            false
        }
    }

    fn terminate(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
        #[cfg(windows)]
        if let Some(job) = &self.job {
            let _ = job.terminate();
        }
        let _ = self.child.start_kill();
    }

    async fn cleanup(&mut self) -> Result<()> {
        self.cleanup_within(CLEANUP_TIMEOUT).await
    }

    async fn cleanup_within(&mut self, within: Duration) -> Result<()> {
        if !self.gone() {
            self.terminate();
        }
        let deadline = std::time::Instant::now() + within;
        loop {
            if self.gone() {
                #[cfg(unix)]
                {
                    self.pgid = None;
                }
                return Ok(());
            }
            ensure!(
                std::time::Instant::now() < deadline,
                "review runtime process termination could not be verified"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

impl Drop for ReviewProcess {
    fn drop(&mut self) {
        // Cancellation terminates the owned processes even after the direct child exits.
        self.terminate();
        #[cfg(windows)]
        if let Some(job) = self.job.take() {
            let _ = job.terminate_and_close();
        }
    }
}

struct Cancellation {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(windows)]
    interrupt: tokio::signal::windows::CtrlC,
    #[cfg(windows)]
    terminate: tokio::signal::windows::CtrlBreak,
}

impl Cancellation {
    fn new() -> Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                interrupt: signal(SignalKind::interrupt())
                    .map_err(|_| anyhow!("cannot supervise review cancellation"))?,
                terminate: signal(SignalKind::terminate())
                    .map_err(|_| anyhow!("cannot supervise review cancellation"))?,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                interrupt: tokio::signal::windows::ctrl_c()
                    .map_err(|_| anyhow!("cannot supervise review cancellation"))?,
                terminate: tokio::signal::windows::ctrl_break()
                    .map_err(|_| anyhow!("cannot supervise review cancellation"))?,
            })
        }
        #[cfg(not(any(unix, windows)))]
        bail!("this platform cannot supervise review cancellation")
    }

    async fn wait(&mut self) {
        #[cfg(any(unix, windows))]
        tokio::select! {
            _ = self.interrupt.recv() => (),
            _ = self.terminate.recv() => (),
        }
        #[cfg(not(any(unix, windows)))]
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
async fn execute(
    program: &str,
    args: &[String],
    cwd: &Path,
    environment: &[(OsString, OsString)],
    input: &[u8],
    timeout: Duration,
    max_output: usize,
) -> Result<Vec<u8>> {
    let mut cancellation = Cancellation::new()?;
    execute_cancellable(
        program,
        args,
        cwd,
        environment,
        input,
        timeout,
        max_output,
        cancellation.wait(),
    )
    .await
}

fn spawn_failure(error: std::io::Error) -> anyhow::Error {
    anyhow!(
        "cannot start the configured review runtime; verify its installation (kind: {:?}; OS code: {:?})",
        error.kind(),
        error.raw_os_error()
    )
}

#[allow(clippy::too_many_arguments)]
async fn execute_cancellable(
    program: &str,
    args: &[String],
    cwd: &Path,
    environment: &[(OsString, OsString)],
    input: &[u8],
    timeout: Duration,
    max_output: usize,
    cancellation: impl std::future::Future<Output = ()>,
) -> Result<Vec<u8>> {
    ensure!(
        cfg!(any(unix, windows)),
        "this platform cannot supervise the review runtime"
    );
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(environment.iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    let job = crate::rc::windows_job::Job::new()
        .map_err(|_| anyhow!("cannot supervise the review runtime"))?;
    #[cfg(windows)]
    crate::rc::windows_job::Job::configure(&mut command);
    let child = command.spawn().map_err(spawn_failure)?;
    #[cfg(unix)]
    let pgid = child.id().map(|id| id as i32);
    let mut tree = ReviewProcess {
        child,
        #[cfg(unix)]
        pgid,
        #[cfg(windows)]
        job: Some(job),
    };
    #[cfg(windows)]
    if tree
        .job
        .as_ref()
        .expect("a Windows reviewer owns its Job")
        .attach_and_resume(&tree.child)
        .is_err()
    {
        tree.cleanup().await?;
        bail!("cannot supervise the review runtime");
    }
    let result = tokio::select! {
        biased;
        () = cancellation => Err(anyhow!("review was cancelled")),
        result = tokio::time::timeout(timeout, async {
        let mut stdin = tree.child.stdin.take().expect("stdin is piped");
        let stdout = tree.child.stdout.take().expect("stdout is piped");
        let stderr = tree.child.stderr.take().expect("stderr is piped");
        let write = async {
            stdin
                .write_all(input)
                .await
                .map_err(|_| anyhow!("review runtime did not accept the complete input"))?;
            stdin
                .shutdown()
                .await
                .map_err(|_| anyhow!("cannot close review runtime input"))?;
            drop(stdin);
            Ok::<(), anyhow::Error>(())
        };
        let (_, output) = tokio::try_join!(write, collect_output(stdout, stderr, max_output))?;
        let status = tree
            .child
            .wait()
            .await
            .map_err(|_| anyhow!("cannot verify review runtime exit status"))?;
        verify_exit(status)?;
        ensure!(tree.gone(), "review runtime left subprocesses running");
        Ok(output)
        }) => result
            .map_err(|_| anyhow!("review runtime exceeded its time limit"))
            .and_then(|result| result),
    };
    tree.cleanup().await?;
    result
}

fn verify_exit(status: ExitStatus) -> Result<()> {
    ensure!(status.success(), "review runtime did not exit successfully");
    Ok(())
}

async fn collect_output(
    mut stdout: impl AsyncRead + Unpin,
    mut stderr: impl AsyncRead + Unpin,
    maximum: usize,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let mut out_buffer = [0; 8192];
    let mut err_buffer = [0; 8192];
    let mut out_open = true;
    let mut err_open = true;
    while out_open || err_open {
        let (is_stdout, read) = tokio::select! {
            read = stdout.read(&mut out_buffer), if out_open => (true, read),
            read = stderr.read(&mut err_buffer), if err_open => (false, read),
        };
        let count = read.map_err(|_| anyhow!("cannot read the complete review runtime output"))?;
        if count == 0 {
            if is_stdout {
                out_open = false;
            } else {
                err_open = false;
            }
            continue;
        }
        ensure!(
            output.len() + errors.len() + count <= maximum,
            "review runtime output exceeds its byte limit"
        );
        if is_stdout {
            output.extend_from_slice(&out_buffer[..count]);
        } else {
            errors.extend_from_slice(&err_buffer[..count]);
        }
    }
    let stderr = std::str::from_utf8(&errors)
        .map_err(|_| anyhow!("review runtime returned invalid UTF-8"))?;
    ensure!(
        stderr.trim().is_empty(),
        "review runtime emitted diagnostics; its review is unavailable"
    );
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spawn_diagnostics_preserve_error_categories_without_private_error_payloads() {
        let error = std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "private-executable-path private-argument private-transcript",
        );
        let diagnostic = format!("{:#}", spawn_failure(error));
        assert!(diagnostic.contains("PermissionDenied"));
        assert!(!diagnostic.contains("private-"));
        let error = std::io::Error::from_raw_os_error(13);
        let diagnostic = format!("{:#}", spawn_failure(error));
        assert!(diagnostic.contains("OS code: Some(13)"));
        assert!(!diagnostic.contains("private-"));
    }

    #[cfg(windows)]
    const WINDOWS_JOB_PROBE: &str =
        "commands::scan::runtime::tests::windows_job_cleanup_deadline_probe";
    #[cfg(windows)]
    const WINDOWS_JOB_CHILD: &str =
        "commands::scan::runtime::tests::windows_review_job_waiting_child";

    #[cfg(windows)]
    #[test]
    fn windows_job_cleanup_deadline_disposes_without_blocking() {
        let fixture = tempfile::tempdir().unwrap();
        let mut probe = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", WINDOWS_JOB_PROBE, "--ignored", "--nocapture"])
            .env("AGIT_TEST_WINDOWS_REVIEW_JOB_PROBE", fixture.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = probe.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = probe.kill();
                let reap_deadline = std::time::Instant::now() + Duration::from_secs(2);
                while probe.try_wait().unwrap().is_none()
                    && std::time::Instant::now() < reap_deadline
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                panic!("Windows review cleanup probe exceeded its watchdog deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success(), "Windows review cleanup probe failed");
        assert_eq!(
            std::fs::read(fixture.path().join("completed")).unwrap(),
            b"cleanup-unverified; disposal-returned; direct-child-terminal",
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "native subprocess for the bounded Windows Job cleanup test"]
    fn windows_job_cleanup_deadline_probe() {
        use std::os::windows::io::{AsRawHandle, BorrowedHandle};
        use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::WaitForSingleObject;

        let Some(fixture) = std::env::var_os("AGIT_TEST_WINDOWS_REVIEW_JOB_PROBE") else {
            return;
        };
        let fixture = std::path::PathBuf::from(fixture);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let ready = fixture.join("ready");
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--exact", WINDOWS_JOB_CHILD, "--ignored", "--nocapture"])
                    .env("AGIT_TEST_WINDOWS_REVIEW_JOB_READY", &ready)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true);
                let job = crate::rc::windows_job::Job::new().unwrap();
                crate::rc::windows_job::Job::configure(&mut command);
                let child = command.spawn().unwrap();
                let mut process = ReviewProcess {
                    child,
                    job: Some(job),
                };
                let borrowed =
                    unsafe { BorrowedHandle::borrow_raw(process.child.raw_handle().unwrap()) };
                let retained = borrowed.try_clone_to_owned().unwrap();
                assert_eq!(
                    unsafe { WaitForSingleObject(retained.as_raw_handle().cast(), 0) },
                    WAIT_TIMEOUT,
                );
                assert!(
                    !ready.exists(),
                    "a suspended reviewer cannot execute before Job assignment"
                );
                process
                    .job
                    .as_ref()
                    .unwrap()
                    .attach_and_resume(&process.child)
                    .unwrap();
                let startup_deadline = std::time::Instant::now() + Duration::from_secs(5);
                while !std::fs::read(&ready).is_ok_and(|contents| contents == b"running") {
                    assert!(
                        std::time::Instant::now() < startup_deadline,
                        "native Job child did not start"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                process.job.as_ref().unwrap().force_nonempty_accounting();
                let allowance = Duration::from_millis(125);
                let started = std::time::Instant::now();
                let error = process.cleanup_within(allowance).await.unwrap_err();
                assert_eq!(
                    error.to_string(),
                    "review runtime process termination could not be verified"
                );
                assert!(started.elapsed() >= allowance);
                let job = process.job.take().unwrap();
                assert!(
                    !process.gone(),
                    "disposing a Job is not proof of process exit"
                );
                process.job = Some(job);
                drop(process);
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "review Job disposal must remain bounded"
                );
                assert_eq!(
                    unsafe { WaitForSingleObject(retained.as_raw_handle().cast(), 3000) },
                    WAIT_OBJECT_0,
                    "the retained direct child must be terminal",
                );
            });
        std::fs::write(
            fixture.join("completed"),
            b"cleanup-unverified; disposal-returned; direct-child-terminal",
        )
        .unwrap();
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "native child owned by the Windows review cleanup probe"]
    fn windows_review_job_waiting_child() {
        let Some(ready) = std::env::var_os("AGIT_TEST_WINDOWS_REVIEW_JOB_READY") else {
            return;
        };
        std::fs::write(ready, b"running").unwrap();
        std::thread::sleep(Duration::from_secs(30));
    }

    fn success() -> Value {
        json!({
            "type": "result", "subtype": "success", "is_error": false,
            "structured_output": {"assessments": []}, "permission_denials": [],
            "stop_reason": "end_turn"
        })
    }

    #[test]
    fn native_result_requires_success_and_structured_output_without_tool_activity() {
        assert_eq!(
            parse_claude_result(&serde_json::to_vec(&success()).unwrap()).unwrap(),
            br#"{"assessments":[]}"#
        );
        for (field, replacement) in [
            ("type", json!("assistant")),
            ("subtype", json!("error_max_turns")),
            ("is_error", json!(true)),
            ("is_error", json!("false")),
            ("structured_output", Value::Null),
            ("structured_output", json!("{\"assessments\":[]}")),
            ("permission_denials", json!([{"tool_name": "Write"}])),
            ("errors", json!(["private-native-error"])),
            ("stop_reason", json!("max_tokens")),
            ("tool_calls", json!([{"name": "Read"}])),
            ("message", json!({"content": [{"type": "tool_use"}]})),
        ] {
            let mut value = success();
            value[field] = replacement;
            let error = parse_claude_result(&serde_json::to_vec(&value).unwrap()).unwrap_err();
            assert!(!format!("{error:#}").contains("private-native-error"));
        }
        for field in ["type", "subtype", "is_error", "structured_output"] {
            let mut value = success();
            value.as_object_mut().unwrap().remove(field);
            assert!(parse_claude_result(&serde_json::to_vec(&value).unwrap()).is_err());
        }
    }

    #[test]
    fn result_rejects_duplicate_fields_invalid_utf8_and_multiple_envelopes() {
        for output in [
            br#"{"type":"result","subtype":"success","is_error":true,"is_error":false,"structured_output":{"assessments":[]}}"#.as_slice(),
            br#"{"type":"result","subtype":"success","is_error":false,"structured_output":{"assessments":[{"category":"absolute-path","category":"none"}]}}"#.as_slice(),
            b"{}\n{}", b"[{}]", b"\xff", b"{\"private\":\"\xff\"}", b"private-native-error",
        ] {
            let error = parse_claude_result(output).unwrap_err();
            assert!(!format!("{error:#}").contains("private-native-error"));
        }
    }

    #[test]
    fn arguments_disable_model_tools_session_storage_and_repository_customizations() {
        let schema = json!({"type": "object"});
        let args = claude_arguments(&schema).unwrap();
        let after = |flag: &str| {
            let at = args.iter().position(|argument| argument == flag).unwrap();
            args[at + 1].clone()
        };
        assert_eq!(after("--tools"), "");
        assert_eq!(after("--settings"), r#"{"disableAllHooks":true}"#);
        assert_eq!(after("--mcp-config"), r#"{"mcpServers":{}}"#);
        assert_eq!(after("--permission-mode"), "dontAsk");
        assert_eq!(after("--permission-prompts"), "none");
        assert_eq!(after("--output-format"), "json");
        assert_eq!(after("--json-schema"), schema.to_string());
        for flag in [
            "--safe-mode",
            "--disable-slash-commands",
            "--strict-mcp-config",
            "--no-chrome",
            "--no-session-persistence",
        ] {
            assert!(args.iter().any(|argument| argument == flag));
        }
        for flag in [
            "--bare",
            "--restricted",
            "--resume",
            "--continue",
            "--dangerously-skip-permissions",
        ] {
            assert!(!args.iter().any(|argument| argument == flag));
        }
    }

    #[test]
    fn native_credentials_are_allowlisted_without_session_or_executable_injection() {
        let source = [
            ("HOME", "/native-home"),
            ("ANTHROPIC_API_KEY", "private-key"),
            ("ANTHROPIC_BASE_URL", "https://native-provider.invalid"),
            ("CLAUDE_CONFIG_DIR", "/native-config"),
            ("AGIT_SESSION", "wrong-session"),
            ("AGIT_RC", "1"),
            ("AGIT_RC_SUPERVISED_HOOK", "1"),
            ("CODEX_HOME", "/other-runtime"),
            ("NODE_OPTIONS", "--require=untrusted"),
            ("BASH_ENV", "/untrusted"),
            ("LD_PRELOAD", "/untrusted"),
            ("CLAUDE_CODE_MANAGED_SETTINGS_PATH", "/untrusted"),
            ("CLAUDE_CODE_ENABLE_TELEMETRY", "1"),
        ];
        let filtered = review_environment(
            source
                .into_iter()
                .map(|(key, value)| (key.into(), value.into())),
        );
        assert_eq!(filtered.len(), 4);
        assert!(
            filtered
                .iter()
                .any(|(key, value)| key == "ANTHROPIC_API_KEY" && value == "private-key")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_environment_keys_keep_native_case_insensitive_meaning() {
        let filtered = review_environment([
            ("Path".into(), std::env::temp_dir().into_os_string()),
            ("SystemRoot".into(), r"C:\Windows".into()),
        ]);
        assert!(filtered.iter().any(|(key, _)| key == "PATH"));
        assert!(filtered.iter().any(|(key, _)| key == "SYSTEMROOT"));
    }

    #[cfg(unix)]
    #[test]
    fn executable_symlinks_cannot_route_review_input_into_the_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let untrusted = script(&workspace, "exit 0");
        let executable = bin.path().join("claude");
        std::os::unix::fs::symlink(&untrusted, &executable).unwrap();
        let environment = vec![("PATH".into(), bin.path().as_os_str().to_owned())];
        let excluded = workspace.path().canonicalize().unwrap();
        assert!(resolve_claude_outside(&environment, Some(&excluded), None).is_err());
        std::fs::remove_file(&executable).unwrap();
        let native = script(&bin, "exit 0");
        std::os::unix::fs::symlink(&native, &executable).unwrap();
        assert_eq!(
            Path::new(&resolve_claude_outside(&environment, Some(&excluded), None).unwrap()),
            Path::new(&native).canonicalize().unwrap(),
        );
    }

    struct InstalledFixture {
        _directory: tempfile::TempDir,
        home: std::path::PathBuf,
        entry: std::path::PathBuf,
        #[cfg(unix)]
        version: std::path::PathBuf,
    }

    impl InstalledFixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let home = directory.path().canonicalize().unwrap().join("profile");
            let name = if cfg!(windows) {
                "claude.exe"
            } else {
                "claude"
            };
            let entry = home.join(".local/bin").join(name);
            let version = home.join(".local/share/claude/versions/2.1.263");
            std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
            std::fs::create_dir_all(version.parent().unwrap()).unwrap();
            Self::executable(&version);
            #[cfg(unix)]
            std::os::unix::fs::symlink(&version, &entry).unwrap();
            #[cfg(not(unix))]
            std::fs::copy(&version, &entry).unwrap();
            Self {
                _directory: directory,
                home,
                entry,
                #[cfg(unix)]
                version,
            }
        }

        fn executable(path: &Path) {
            std::fs::write(path, b"synthetic native installation").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
        }

        fn environment(&self, directories: &[std::path::PathBuf]) -> Vec<(OsString, OsString)> {
            vec![
                (
                    (if cfg!(windows) { "USERPROFILE" } else { "HOME" }).into(),
                    self.home.as_os_str().to_owned(),
                ),
                ("PATH".into(), std::env::join_paths(directories).unwrap()),
            ]
        }
    }

    #[test]
    fn a_native_home_install_is_selected_without_restoring_home_bins_to_child_path() {
        let fixture = InstalledFixture::new();
        let shadow = fixture.home.join("project/bin");
        std::fs::create_dir_all(&shadow).unwrap();
        InstalledFixture::executable(&shadow.join(fixture.entry.file_name().unwrap()));
        let environment =
            fixture.environment(&[shadow, fixture.entry.parent().unwrap().to_owned()]);
        let installation = home_installation(&fixture.home, &environment).unwrap();
        let selected =
            resolve_claude_outside(&environment, Some(&fixture.home), Some(&installation)).unwrap();
        assert_eq!(Path::new(&selected), fixture.entry.canonicalize().unwrap());
        let child = review_environment_outside(environment, Some(&fixture.home));
        let path = &child.iter().find(|(key, _)| key == "PATH").unwrap().1;
        assert!(
            path.is_empty(),
            "native selection must not restore sibling helper shadows"
        );
    }

    #[test]
    fn a_git_home_does_not_receive_the_native_installation_exception() {
        let fixture = InstalledFixture::new();
        let environment = fixture.environment(&[fixture.entry.parent().unwrap().to_owned()]);
        std::fs::create_dir(fixture.home.join(".git")).unwrap();
        assert!(home_installation(&fixture.home, &environment).is_none());
        assert!(resolve_claude_outside(&environment, Some(&fixture.home), None).is_err());
        let outside_home = fixture.home.parent().unwrap();
        assert!(home_installation(outside_home, &environment).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn redirected_native_installation_directories_and_extra_link_hops_are_rejected() {
        for relative in [".local/bin", ".local/share/claude/versions"] {
            let fixture = InstalledFixture::new();
            let redirected = fixture.home.join(relative);
            let moved = fixture.home.join("project-owned");
            std::fs::rename(&redirected, &moved).unwrap();
            std::os::unix::fs::symlink(&moved, &redirected).unwrap();
            let environment = fixture.environment(&[fixture.entry.parent().unwrap().to_owned()]);
            let installation = home_installation(&fixture.home, &environment).unwrap();
            assert!(
                resolve_claude_outside(&environment, Some(&fixture.home), Some(&installation))
                    .is_err()
            );
        }
        let fixture = InstalledFixture::new();
        let bridge = fixture.home.parent().unwrap().join("outside-bridge");
        let alias = fixture.version.parent().unwrap().join("alias-version");
        std::os::unix::fs::symlink(&fixture.version, &bridge).unwrap();
        std::os::unix::fs::symlink(&bridge, &alias).unwrap();
        std::fs::remove_file(&fixture.entry).unwrap();
        std::os::unix::fs::symlink(&alias, &fixture.entry).unwrap();
        let environment = fixture.environment(&[fixture.entry.parent().unwrap().to_owned()]);
        let installation = home_installation(&fixture.home, &environment).unwrap();
        assert!(
            resolve_claude_outside(&environment, Some(&fixture.home), Some(&installation)).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_home_lookup_rejects_parent_paths_and_neighboring_version_directories() {
        let fixture = InstalledFixture::new();
        let environment = fixture.environment(&[fixture.home.join(".local/bin/../bin")]);
        let installation = home_installation(&fixture.home, &environment).unwrap();
        assert!(
            resolve_claude_outside(&environment, Some(&fixture.home), Some(&installation)).is_err()
        );
        let other = fixture
            .home
            .join(".local/share/claude/versions-evil/2.1.263");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        InstalledFixture::executable(&other);
        std::fs::remove_file(&fixture.entry).unwrap();
        std::os::unix::fs::symlink(other, &fixture.entry).unwrap();
        let environment = fixture.environment(&[fixture.entry.parent().unwrap().to_owned()]);
        assert!(
            resolve_claude_outside(&environment, Some(&fixture.home), Some(&installation)).is_err()
        );
    }

    #[cfg(unix)]
    fn script(directory: &tempfile::TempDir, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = directory.path().join("reviewer");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path.to_str().unwrap().to_owned()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_native_runtime_receives_private_stdin_and_isolated_launch_contract() {
        let fixtures = tempfile::tempdir().unwrap();
        let program = script(
            &fixtures,
            r#"
[ -d "$AGIT_HOME" ] || exit 44
printf '%s\n' "$AGIT_HOME" >> "$CLAUDE_CONFIG_DIR/agent-homes"
if [ "$2" = "--help" ]; then
  printf '%s\n' '--print --safe-mode --settings --tools --disable-slash-commands --strict-mcp-config --mcp-config --no-chrome --no-session-persistence --permission-mode --permission-prompts --output-format --json-schema'
  exit 0
fi
[ -z "${HOME+x}" ] || exit 41
[ -z "${AGIT_SESSION+x}" ] || exit 42
[ -z "${NODE_OPTIONS+x}" ] || exit 43
printf '%s\n' "$PWD" > "$CLAUDE_CONFIG_DIR/cwd"
printf '%s\n' "$@" > "$CLAUDE_CONFIG_DIR/args"
/bin/cat > "$CLAUDE_CONFIG_DIR/input"
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"structured_output":{"assessments":[]}}'
"#,
        );
        let original_home = fixtures.path().join("original-home");
        let environment = vec![
            (
                "CLAUDE_CONFIG_DIR".into(),
                fixtures.path().as_os_str().to_owned(),
            ),
            ("AGIT_HOME".into(), original_home.as_os_str().to_owned()),
        ];
        let result = review_claude(
            &program,
            &environment,
            "private-transcript",
            &json!({"type":"object"}),
        )
        .await
        .unwrap();
        assert_eq!(result, br#"{"assessments":[]}"#);
        assert_eq!(
            std::fs::read(fixtures.path().join("input")).unwrap(),
            b"private-transcript"
        );
        let args = std::fs::read_to_string(fixtures.path().join("args")).unwrap();
        assert!(!args.contains("private-transcript"));
        let cwd = std::fs::read_to_string(fixtures.path().join("cwd")).unwrap();
        assert!(cwd.contains("agit-sensitive-review-"));
        assert!(!Path::new(cwd.trim()).exists());
        let homes = std::fs::read_to_string(fixtures.path().join("agent-homes")).unwrap();
        let homes: Vec<_> = homes.lines().collect();
        assert_eq!(homes.len(), 2);
        assert_eq!(homes[0], homes[1]);
        assert_ne!(Path::new(homes[0]), original_home);
        assert!(Path::new(homes[0]).ends_with("agent-state"));
        assert!(!Path::new(homes[0]).exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unsupported_flags_stop_before_transcript_delivery() {
        let fixtures = tempfile::tempdir().unwrap();
        let program = script(
            &fixtures,
            r#"
if [ "$2" = "--help" ]; then
  printf '%s\n' '--print --tools'
  exit 0
fi
touch "$CLAUDE_CONFIG_DIR/model-called"
"#,
        );
        let environment = vec![(
            "CLAUDE_CONFIG_DIR".into(),
            fixtures.path().as_os_str().to_owned(),
        )];
        assert!(
            review_claude(&program, &environment, "private-transcript", &json!({}))
                .await
                .is_err()
        );
        assert!(!fixtures.path().join("model-called").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn raw_output_bound_covers_whitespace_stderr_and_diagnostics_after_stdout_eof() {
        let fixtures = tempfile::tempdir().unwrap();
        for body in [
            "while :; do printf '                                        '; done",
            "while :; do printf '                                        ' >&2; done",
            "printf '{}'; exec 1>&-; printf 'private-native-error' >&2",
            "printf '{}'; exit 9",
            "printf '{}'; printf '\\377' >&2",
        ] {
            let program = script(&fixtures, body);
            let error = execute(
                &program,
                &[],
                fixtures.path(),
                &[],
                b"",
                Duration::from_secs(2),
                1024,
            )
            .await
            .unwrap_err();
            assert!(!format!("{error:#}").contains("private-native-error"));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deadline_covers_blocked_input_and_kills_descendants_before_they_write() {
        let fixtures = tempfile::tempdir().unwrap();
        let program = script(
            &fixtures,
            r#"
( /bin/sleep 1; printf 'escaped' > "$CLAUDE_CONFIG_DIR/escaped" ) &
/bin/sleep 20
"#,
        );
        let environment = vec![(
            "CLAUDE_CONFIG_DIR".into(),
            fixtures.path().as_os_str().to_owned(),
        )];
        let input = vec![b'x'; MAX_PROMPT_BYTES];
        let error = execute(
            &program,
            &[],
            fixtures.path(),
            &environment,
            &input,
            Duration::from_millis(100),
            1024,
        )
        .await
        .unwrap_err();
        assert!(!error.to_string().is_empty());
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(!fixtures.path().join("escaped").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_stops_review_and_denies_late_child_write() {
        let fixtures = tempfile::tempdir().unwrap();
        let program = script(
            &fixtures,
            r#"
( /bin/sleep 1; printf 'escaped' > "$CLAUDE_CONFIG_DIR/escaped" ) &
printf '%s\n' "$!" > "$CLAUDE_CONFIG_DIR/ready"
/bin/sleep 20
"#,
        );
        let environment = vec![(
            "CLAUDE_CONFIG_DIR".into(),
            fixtures.path().as_os_str().to_owned(),
        )];
        let cancelled = std::cell::Cell::new(false);
        let cancellation = async {
            while !fixtures.path().join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            cancelled.set(true);
        };
        let error = execute_cancellable(
            &program,
            &[],
            fixtures.path(),
            &environment,
            b"",
            Duration::from_secs(2),
            1024,
            cancellation,
        )
        .await
        .unwrap_err();
        assert!(
            cancelled.get(),
            "the cancellation future must become ready (execution error: {error:#}; ready marker exists: {})",
            fixtures.path().join("ready").exists()
        );
        assert!(matches!(
            error.to_string().as_str(),
            "review was cancelled" | "review runtime process termination could not be verified"
        ));
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(!fixtures.path().join("escaped").exists());
    }
}
