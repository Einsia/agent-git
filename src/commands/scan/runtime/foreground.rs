//! Foreground review uses the native terminal; a successful exit is not an accepted report.

use anyhow::{Result, anyhow, ensure};
use std::ffi::{OsStr, OsString};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

const MAX_OPENING_BYTES: usize = 8 * 1024;
const TOOLS: &str = "Bash,Read,Write,AskUserQuestion";
const EMPTY_GIT_CONFIG: &str = "empty-git-config";

/// The caller keeps the trusted workspace and frozen publication owner alive through review.
/// Its `agit-home` contains only the caller's prepared inspection store. No ambient Agit store
/// or merge authority is imported. Native permissions remain authoritative over tool access.
pub(in crate::commands) struct ForegroundReview {
    program: String,
    workspace: PathBuf,
    environment: Vec<(OsString, OsString)>,
    arguments: Vec<String>,
    report: PathBuf,
    session_id: String,
    git_config: PathBuf,
}

/// The child exited successfully. The caller must still validate the report and its complete
/// inventory before asking for publication consent; this value grants no publication authority.
pub(in crate::commands) struct ForegroundExit {
    report: PathBuf,
    session_id: String,
}

impl ForegroundExit {
    pub(in crate::commands) fn report_path(&self) -> &Path {
        &self.report
    }

    pub(in crate::commands) fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl ForegroundReview {
    pub(in crate::commands) fn prepare(workspace: &Path, opening_prompt: &str) -> Result<Self> {
        require_terminal(
            std::io::stdin().is_terminal(),
            std::io::stdout().is_terminal(),
        )?;
        validate_opening(opening_prompt)?;
        super::require_configured_claude()?;
        let workspace = workspace
            .canonicalize()
            .map_err(|_| anyhow!("cannot resolve the prepared audit workspace"))?;
        ensure!(
            workspace.is_dir(),
            "the prepared audit workspace is not a directory"
        );
        let agent_home = prepared_agent_home(&workspace)?;
        let git_config = workspace.join(EMPTY_GIT_CONFIG);
        let environment = foreground_environment(std::env::vars_os(), &agent_home, &git_config);
        let program = super::resolve_claude(&environment)?;
        // Windows batch launchers can invoke a shell despite separate Command arguments.
        #[cfg(windows)]
        ensure!(
            Path::new(&program)
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|extension| extension.eq_ignore_ascii_case("exe")),
            "foreground audit requires the native Claude executable on Windows"
        );
        let report = workspace.join("audit-report.json");
        require_absent_report(&report)?;
        let session_id = uuid::Uuid::new_v4().to_string();
        let arguments = foreground_arguments(opening_prompt, &session_id, &report)?;
        create_empty_git_config(&workspace)?;
        Ok(Self {
            program,
            workspace,
            environment,
            arguments,
            report,
            session_id,
            git_config,
        })
    }

    pub(in crate::commands) fn run(self) -> Result<ForegroundExit> {
        require_terminal(
            std::io::stdin().is_terminal(),
            std::io::stdout().is_terminal(),
        )?;
        require_absent_report(&self.report)?;
        verify_empty_git_config(&self.workspace, &self.git_config)?;
        let status = self.command().status().map_err(super::spawn_failure)?;
        require_success(status)?;
        verify_empty_git_config(&self.workspace, &self.git_config)?;
        Ok(ForegroundExit {
            report: self.report,
            session_id: self.session_id,
        })
    }

    pub(in crate::commands) fn session_id(&self) -> &str {
        &self.session_id
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.arguments)
            .current_dir(&self.workspace)
            .env_clear()
            .envs(self.environment.iter().cloned())
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        command
    }
}

fn require_terminal(input: bool, output: bool) -> Result<()> {
    ensure!(
        input && output,
        "interactive audit requires terminal input and output"
    );
    Ok(())
}

fn validate_opening(prompt: &str) -> Result<()> {
    ensure!(
        !prompt.trim().is_empty() && prompt.len() <= MAX_OPENING_BYTES && !prompt.contains('\0'),
        "audit opening directions are empty, invalid, or exceed their byte limit"
    );
    Ok(())
}

fn prepared_agent_home(workspace: &Path) -> Result<PathBuf> {
    let path = workspace.join("agit-home");
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|_| anyhow!("the prepared audit store is unavailable"))?;
    ensure!(
        metadata.is_dir(),
        "the prepared audit store is not an owned directory"
    );
    let path = path
        .canonicalize()
        .map_err(|_| anyhow!("cannot resolve the prepared audit store"))?;
    ensure!(
        path.parent() == Some(workspace),
        "the prepared audit store leaves its workspace"
    );
    Ok(path)
}

fn require_absent_report(report: &Path) -> Result<()> {
    match std::fs::symlink_metadata(report) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err(anyhow!("the audit report destination is not fresh")),
    }
}

/// Config isolation must not truncate or follow a file supplied before this preparation.
/// The caller owns the workspace lifetime; native reads retain the fresh empty file until exit.
fn create_empty_git_config(workspace: &Path) -> Result<()> {
    let path = workspace.join(EMPTY_GIT_CONFIG);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .map_err(|_| anyhow!("cannot create a fresh audit Git configuration"))?;
    drop(file);
    verify_empty_git_config(workspace, &path)
}

fn verify_empty_git_config(workspace: &Path, path: &Path) -> Result<()> {
    ensure!(
        path == workspace.join(EMPTY_GIT_CONFIG),
        "the audit Git configuration leaves its prepared workspace"
    );
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| anyhow!("the audit Git configuration is unavailable"))?;
    ensure!(
        metadata.is_file() && metadata.len() == 0,
        "the audit Git configuration is not an empty regular file"
    );
    ensure!(
        path.canonicalize().ok().as_deref() == Some(path),
        "the audit Git configuration leaves its prepared workspace"
    );
    Ok(())
}

fn foreground_arguments(prompt: &str, session: &str, report: &Path) -> Result<Vec<String>> {
    validate_opening(prompt)?;
    let report = report
        .to_str()
        .ok_or_else(|| anyhow!("the audit report path is not Unicode"))?;
    let mut arguments = vec![
        "--safe-mode".into(),
        "--setting-sources".into(),
        "user".into(),
        "--settings".into(),
        r#"{"disableAllHooks":true}"#.into(),
        "--disable-slash-commands".into(),
        "--strict-mcp-config".into(),
        "--mcp-config".into(),
        r#"{"mcpServers":{}}"#.into(),
        "--no-chrome".into(),
        "--tools".into(),
        TOOLS.into(),
        "--permission-mode".into(),
        "default".into(),
        "--session-id".into(),
        session.into(),
    ];
    // A fixed leading sentence keeps caller text in the positional prompt, even if it starts
    // with an option. Separate argv entries preserve quotes, newlines and shell metacharacters.
    arguments.push(format!(
        "Review the prepared publication described below. Read only the supplied inspection inputs, \
         use agit show as directed, and ask the person at the terminal when clarification is needed. \
         Write only the requested audit report to this JSON-quoted path: {}. \
         Do not publish, upload, alter source data, or treat answers as publication consent. \
         Return to the terminal after the report is ready.\n\n{prompt}",
        serde_json::to_string(report).map_err(|_| anyhow!("cannot encode the audit report path"))?,
    ));
    Ok(arguments)
}

fn foreground_environment(
    source: impl IntoIterator<Item = (OsString, OsString)>,
    agent_home: &Path,
    git_config: &Path,
) -> Vec<(OsString, OsString)> {
    let source = source.into_iter().collect::<Vec<_>>();
    let mut environment = super::review_environment(source.iter().cloned());
    environment.extend(source.into_iter().filter_map(|(key, value)| {
        let key = super::canonical_environment_key(key);
        terminal_key(&key).then_some((key, value))
    }));
    environment.push(("AGIT_HOME".into(), agent_home.as_os_str().to_owned()));
    let git_config = crate::domain::repo::inspection_git_path_spelling(git_config.to_owned());
    environment.extend([
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        (
            "GIT_CONFIG_SYSTEM".into(),
            git_config.as_os_str().to_owned(),
        ),
        ("GIT_CONFIG_GLOBAL".into(), git_config.into_os_string()),
        ("GIT_CONFIG_COUNT".into(), "0".into()),
        ("GIT_NO_REPLACE_OBJECTS".into(), "1".into()),
        ("GIT_NO_LAZY_FETCH".into(), "1".into()),
        ("GIT_ATTR_NOSYSTEM".into(), "1".into()),
    ]);
    environment
}

fn terminal_key(key: &OsStr) -> bool {
    matches!(
        key.to_str(),
        Some("TERM" | "COLORTERM" | "LANG" | "LC_ALL" | "LC_CTYPE" | "NO_COLOR")
    )
}

fn require_success(status: ExitStatus) -> Result<()> {
    ensure!(
        status.success(),
        "interactive audit did not exit successfully"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
