//! Private Git executables use child-local search paths and retain the caller's Git configuration.

use std::path::PathBuf;
use std::process::Command;

#[cfg(feature = "cli")]
fn config_count(value: &std::ffi::OsStr) -> Option<u32> {
    let text = value.to_str()?;
    text.parse::<u32>()
        .ok()
        .filter(|count| count.to_string() == text)
}

#[cfg(feature = "cli")]
fn concrete_http_scope(address: &str) -> bool {
    address.split_once("://").is_some_and(|(scheme, rest)| {
        (scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http"))
            && !rest.is_empty()
            && !rest.starts_with('/')
            && !rest.split('/').next().unwrap_or_default().contains('@')
    }) && !address.contains(['\\', '*', '?', '#'])
        && !address
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
        && url::Url::parse(address).is_ok_and(|url| {
            url.host_str().is_some_and(|host| !host.contains('*'))
                && url.username().is_empty()
                && url.password().is_none()
        })
}

#[cfg(feature = "cli")]
fn check_setting(key: &std::ffi::OsStr, value: &std::ffi::OsStr) -> Result<(), &'static str> {
    if key.eq_ignore_ascii_case("color.ui") {
        return value
            .eq_ignore_ascii_case("false")
            .then_some(())
            .ok_or("color.ui must disable color");
    }
    let (section, rest) = key
        .to_str()
        .and_then(|key| key.split_once('.'))
        .ok_or("invalid Git setting name")?;
    if !section.eq_ignore_ascii_case("credential") && !section.eq_ignore_ascii_case("http") {
        return Err("setting is outside the credential, transport and display policy");
    }
    let (_, field) = rest.rsplit_once('.').filter(|(scope, _)| concrete_http_scope(scope))
        .ok_or("authentication and HTTP settings require a concrete HTTP(S) URL without user information")?;
    if section.eq_ignore_ascii_case("credential") {
        return (field.starts_with(|ch: char| ch.is_ascii_alphabetic())
            && field
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-'))
        .then_some(())
        .ok_or("invalid credential field name");
    }
    if field.eq_ignore_ascii_case("followRedirects") {
        return value
            .eq_ignore_ascii_case("false")
            .then_some(())
            .ok_or("HTTP redirects must stay disabled");
    }
    if field.eq_ignore_ascii_case("proactiveAuth") {
        return value
            .eq_ignore_ascii_case("auto")
            .then_some(())
            .ok_or("HTTP proactive authentication must use auto");
    }
    Err("HTTP setting may override transport routing or security")
}

#[cfg(feature = "cli")]
pub(crate) fn check_runtime_environment(
    environment: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    routing: &[&str],
    action: &str,
) -> anyhow::Result<()> {
    let mut count = Some(0);
    let mut entries = std::collections::BTreeMap::new();
    let mut problems = Vec::new();
    for (name, value) in environment {
        let name = name.to_string_lossy();
        #[cfg(windows)]
        let name = name.to_ascii_uppercase();
        let name: &str = name.as_ref();
        if name == "GIT_CONFIG_COUNT" {
            count = config_count(&value);
            if count.is_none() {
                problems.push("invalid GIT_CONFIG_COUNT".into());
            }
        } else if let Some(suffix) = name
            .strip_prefix("GIT_CONFIG_KEY_")
            .or_else(|| name.strip_prefix("GIT_CONFIG_VALUE_"))
        {
            if let Some(index) = config_count(suffix.as_ref()) {
                let (key, setting) = entries.entry(index).or_insert((None, None));
                if name.starts_with("GIT_CONFIG_KEY_") {
                    *key = Some(value);
                } else {
                    *setting = Some(value);
                }
            } else {
                problems.push("noncanonical numbered Git configuration index".into());
            }
        } else if name == "GIT_CONFIG_PARAMETERS" {
            problems.push("opaque GIT_CONFIG_PARAMETERS cannot be validated".into());
        } else if routing.contains(&name) {
            problems.push(format!("unset {name}: it can override Git routing"));
        }
    }
    let mut next = 0;
    let missing = |first, end| {
        format!(
            "missing numbered Git configuration at indices {first}..={}",
            end - 1
        )
    };
    for (index, (key, value)) in entries {
        if let Some(count) = count {
            let end = index.min(count);
            if next < end {
                problems.push(missing(next, end));
            }
            next = end.saturating_add(1);
            if index >= count {
                problems.push(format!("index {index}: outside GIT_CONFIG_COUNT"));
                continue;
            }
        }
        let result = match (key, value) {
            (Some(key), Some(value)) => check_setting(&key, &value),
            (None, _) => Err("missing key"),
            (_, None) => Err("missing value"),
        };
        if let Err(reason) = result {
            problems.push(format!("index {index}: {reason}"));
        }
    }
    if let Some(count) = count
        && next < count
    {
        problems.push(missing(next, count));
    }
    anyhow::ensure!(
        problems.is_empty(),
        "{action}: unsupported runtime Git configuration: {}",
        problems.join("; ")
    );
    Ok(())
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
