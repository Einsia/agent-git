//! The environment every agit a test starts begins from.
//!
//! A test's AGIT_HOME is new, so the first command the binary runs reports a new installation.
//! With no Hub set the binary takes the production Hub, and that report and the command's usage
//! statistics go to production. A command cleared here keeps nothing from the parent but a Hub on
//! a local port nothing listens on; a test that serves a Hub sets its own after this, which wins.
//!
//! A test whose subject is the production Hub itself removes that Hub again, and then must send
//! its statistics to a local sink. The sink's address and key are spelled only here: the binary
//! ignores a sink whose address or key fails its check and sends to production instead.

// Each test that includes this module uses only part of it.
#![allow(dead_code)]

use std::process::Command;

/// A Hub address that refuses connections.
pub const LOCAL_HUB: &str = "http://127.0.0.1:9";

/// A port on the local machine nothing listens on.
pub const REFUSING_PORT: u16 = 9;

/// The key a local sink is configured with.
pub const SINK_KEY: &str = "test_key";

pub trait LabEnv {
    /// Clear the environment and point the Hub at [`LOCAL_HUB`].
    fn env_clear_local(&mut self) -> &mut Self;

    /// Send usage statistics to `port` on the local machine, whatever Hub the command uses.
    fn telemetry_sink(&mut self, port: u16) -> &mut Self;
}

impl LabEnv for Command {
    fn env_clear_local(&mut self) -> &mut Self {
        self.env_clear().env("AGIT_HUB_URL", LOCAL_HUB)
    }

    fn telemetry_sink(&mut self, port: u16) -> &mut Self {
        self.env("AGIT_TELEMETRY_HOST", format!("http://127.0.0.1:{port}"))
            .env("AGIT_TELEMETRY_KEY", SINK_KEY)
    }
}

/// The agent runtimes a command can find on PATH and ask about themselves, as `agit doctor` does.
const RUNTIMES: [&str; 3] = ["claude", "codex", "opencode"];

/// The parent's PATH without the agent runtimes installed on the computer running the tests.
/// `agit doctor` asks each runtime it finds on PATH about itself, so a test that passed PATH on
/// reached whichever runtimes that computer had, and waited as long as they took to answer
/// (#417). Each folder holding a runtime is left out.
pub fn path_without_runtimes() -> std::ffi::OsString {
    without_runtimes(&std::env::var_os("PATH").unwrap_or_default())
}

/// `path` without the folders that hold an agent runtime. When git is in one of them, a scratch
/// folder with a `git` that runs it takes that folder's place, so git stays and the runtime goes.
/// Git for Windows finds its installation from where its launcher sits, so there a git that shares
/// a folder with a runtime stops the test instead.
pub fn without_runtimes(path: &std::ffi::OsStr) -> std::ffi::OsString {
    let holds = |folder: &std::path::Path, program: &str| {
        ["", ".exe", ".cmd", ".bat", ".com", ".ps1"]
            .iter()
            .any(|suffix| folder.join(format!("{program}{suffix}")).is_file())
    };
    let mut kept = Vec::new();
    for folder in std::env::split_paths(path) {
        if !RUNTIMES.iter().any(|runtime| holds(&folder, runtime)) {
            kept.push(folder);
        } else if holds(&folder, "git") {
            kept.push(git_alone(&folder));
        }
    }
    std::env::join_paths(kept).expect("a PATH made of PATH's own folders joins")
}

#[cfg(unix)]
fn git_alone(folder: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    // One scratch folder per git for the whole test binary, however many commands it starts.
    static ALONE: std::sync::Mutex<
        std::collections::BTreeMap<std::path::PathBuf, std::path::PathBuf>,
    > = std::sync::Mutex::new(std::collections::BTreeMap::new());
    let mut alone = ALONE.lock().unwrap();
    if let Some(scratch) = alone.get(folder) {
        return scratch.clone();
    }
    let scratch = tempfile::Builder::new()
        .prefix("agit-test-git-")
        .tempdir()
        .unwrap()
        .keep();
    let git = scratch.join("git");
    let target = folder.join("git").to_string_lossy().replace('\'', "'\\''");
    std::fs::write(&git, format!("#!/bin/sh\nexec '{target}' \"$@\"\n")).unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
    alone.insert(folder.to_path_buf(), scratch.clone());
    scratch
}

#[cfg(windows)]
fn git_alone(folder: &std::path::Path) -> std::path::PathBuf {
    panic!(
        "test setup, not agit: git and an agent runtime share {} on this computer's PATH, so the \
         tests can't leave the runtime out without losing git; put git on PATH from a folder that \
         holds no codex, claude or opencode",
        folder.display()
    )
}

/// The same environment for a command run on a pseudo-terminal.
#[cfg(feature = "rc")]
impl LabEnv for portable_pty::CommandBuilder {
    fn env_clear_local(&mut self) -> &mut Self {
        self.env_clear();
        self.env("AGIT_HUB_URL", LOCAL_HUB);
        self
    }

    fn telemetry_sink(&mut self, port: u16) -> &mut Self {
        self.env("AGIT_TELEMETRY_HOST", format!("http://127.0.0.1:{port}"));
        self.env("AGIT_TELEMETRY_KEY", SINK_KEY);
        self
    }
}
