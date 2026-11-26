//! Private Git executables use child-local search paths and retain the caller's Git configuration.

use std::path::PathBuf;
use std::process::Command;

#[cfg(feature = "cli")]
pub(crate) fn config_count(value: &std::ffi::OsStr) -> Option<u32> {
    let text = value.to_str()?;
    text.parse::<u32>()
        .ok()
        .filter(|count| count.to_string() == text)
}

#[cfg(feature = "cli")]
pub(crate) fn supported_runtime_config(key: &std::ffi::OsStr, value: &std::ffi::OsStr) -> bool {
    if key.eq_ignore_ascii_case("color.ui") {
        return value.eq_ignore_ascii_case("false");
    }
    let Some((section, rest)) = key.to_str().and_then(|key| key.split_once('.')) else {
        return false;
    };
    let Some((address, field)) = rest.rsplit_once('.') else {
        return false;
    };
    section.eq_ignore_ascii_case("credential")
        && field.eq_ignore_ascii_case("helper")
        && address.split_once("://").is_some_and(|(scheme, rest)| {
            (scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http"))
                && !rest.is_empty()
                && !rest.starts_with('/')
        })
        && !address.contains(['\\', '*', '?', '#'])
        && !address
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
        && url::Url::parse(address).is_ok_and(|url| {
            url.host_str().is_some_and(|host| !host.contains('*'))
                && url.username().is_empty()
                && url.password().is_none()
        })
}

#[cfg(feature = "bundled-git")]
mod bundled;

/// Prepare the embedded toolchain before any CLI Git subprocess can start.
pub fn initialize() -> anyhow::Result<()> {
    #[cfg(feature = "bundled-git")]
    bundled::initialize()?;
    Ok(())
}

pub fn is_bundled() -> bool {
    #[cfg(feature = "bundled-git")]
    return bundled::runtime().is_some();
    #[cfg(not(feature = "bundled-git"))]
    false
}

pub fn command() -> Command {
    #[cfg(feature = "bundled-git")]
    if let Some(runtime) = bundled::runtime() {
        let mut command = super::background::command(runtime.git());
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        if let Some(config) = std::env::var_os("GIT_CONFIG_SYSTEM") {
            command.env("GIT_CONFIG_SYSTEM", config);
        }
        runtime.configure(&mut command);
        return command;
    }
    super::background::command("git")
}

/// Repository writes use a child-local umask without forking the daemon's address space.
pub(crate) fn private_command() -> Command {
    let git = command();
    #[cfg(unix)]
    {
        let mut command = super::background::command("/bin/sh");
        // Arguments remain data, including the bundled executable path and repository names.
        command
            .args(["-c", "umask 077; exec \"$@\"", "agit-git"])
            .arg(git.get_program());
        for (key, value) in git.get_envs() {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
        command
    }
    #[cfg(not(unix))]
    git
}

/// Git for Windows maps `/dev/null` to its null device; an empty filename is not portable.
/// Apply before the subcommand so the empty graft file's advice override is a global option.
pub(crate) fn disable_grafts(command: &mut Command) {
    command
        .env("GIT_GRAFT_FILE", "/dev/null")
        .args(["-c", "advice.graftFileDeprecated=false"]);
}

/// Reapply private executable lookup after a caller clears the subprocess environment.
pub fn configure(command: &mut Command) {
    #[cfg(feature = "bundled-git")]
    if let Some(runtime) = bundled::runtime() {
        runtime.configure(command);
    }
    #[cfg(not(feature = "bundled-git"))]
    let _ = command;
}

#[cfg(feature = "rc")]
pub fn async_command() -> tokio::process::Command {
    command().into()
}

/// Git for Windows requires ordinary drive or UNC paths when opening configuration and helpers.
pub fn path_for_git(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        use std::ffi::OsString;
        use std::path::{Component, Prefix};

        let mut components = path.components();
        let prefix = match components.next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                Prefix::VerbatimDisk(drive) => {
                    Some(OsString::from(format!("{}:", char::from(drive))))
                }
                Prefix::VerbatimUNC(server, share) => {
                    let mut prefix = OsString::from(r"\\");
                    prefix.push(server);
                    prefix.push(r"\");
                    prefix.push(share);
                    Some(prefix)
                }
                _ => None,
            },
            _ => None,
        };
        if let Some(prefix) = prefix {
            return PathBuf::from(prefix).join(components.as_path());
        }
    }
    path
}
