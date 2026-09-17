//! Private Git executables use child-local search paths and retain the caller's Git configuration.

use std::path::PathBuf;
use std::process::Command;

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
