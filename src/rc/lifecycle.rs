//! Namespace-scoped daemon reconciliation shared by upgrades and bridge attachment.

use super::{
    build_identity::{BUILD_ID, RPC_FEATURES},
    control::{self, Presence, Reply, Request, Status},
};
use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(50);
const REQUIRED: &[&str] = &["peer-control-v1", "history-v2"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeTarget {
    pid: u32,
    instance_id: Option<String>,
    executable: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Absent,
    Unchanged,
    Restarted,
    Deferred {
        message: String,
        recovery_command: String,
    },
}

struct Requirements<'a> {
    extra: &'a [String],
    current_build: bool,
}

impl Requirements<'_> {
    fn validate(&self) -> crate::Result<()> {
        for feature in self.extra {
            ensure!(
                RPC_FEATURES.contains(&feature.as_str()),
                "this bridge does not provide required RPC feature {feature}"
            );
        }
        Ok(())
    }

    fn accepts(&self, status: &Status) -> bool {
        status.identity.as_ref().is_some_and(|identity| {
            (!self.current_build || identity.build_id == BUILD_ID)
                && REQUIRED
                    .iter()
                    .copied()
                    .chain(self.extra.iter().map(String::as_str))
                    .all(|feature| {
                        identity
                            .rpc_features
                            .iter()
                            .any(|present| present == feature)
                    })
        })
    }
}

fn probe() -> crate::Result<Option<Status>> {
    match control::presence() {
        Presence::Absent => Ok(None),
        Presence::Unclear(reason) => bail!("cannot determine local daemon state: {reason}"),
        Presence::Running(_) => match control::ask(&Request::Status)? {
            Reply::Status(status) => Ok(Some(status)),
            other => bail!("local daemon status is unavailable: {other:?}"),
        },
    }
}

fn lock() -> crate::Result<File> {
    let path = super::rc_dir()?.join("lifecycle.lock");
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    let deadline = Instant::now() + WAIT;
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(file),
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                ensure!(
                    Instant::now() < deadline,
                    "another local daemon reconciliation is still running in {}",
                    path.display()
                );
                std::thread::sleep(POLL);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

pub fn recovery_command() -> crate::Result<String> {
    recovery_command_for(&std::env::current_exe()?)
}

pub fn recovery_command_for(exe: &Path) -> crate::Result<String> {
    let home = crate::infra::config::agit_home()?;
    local_command(&home, exe, "restart --if-idle")
}

fn local_command(home: &Path, exe: &Path, action: &str) -> crate::Result<String> {
    #[cfg(unix)]
    {
        Ok(format!(
            "AGIT_HOME={} {} rc local {action}",
            shlex::try_quote(&home.to_string_lossy())?,
            shlex::try_quote(&exe.to_string_lossy())?
        ))
    }
    #[cfg(windows)]
    {
        Ok(format!(
            "$env:AGIT_HOME = '{}'; & '{}' rc local {action}",
            home.display().to_string().replace('\'', "''"),
            exe.display().to_string().replace('\'', "''")
        ))
    }
}

#[cfg(unix)]
pub(super) fn stopped_recovery_command(home: &Path) -> crate::Result<String> {
    local_command(home, &std::env::current_exe()?, "recover --confirm-stopped")
}

pub(super) fn recover_stopped() -> crate::Result<bool> {
    #[cfg(unix)]
    {
        let _lock = lock()?;
        control::recover_stopped()
    }
    #[cfg(windows)]
    bail!("legacy socket recovery is only needed on Unix; Windows uses named-pipe ownership")
}

fn deferred(message: impl Into<String>) -> crate::Result<Outcome> {
    Ok(Outcome::Deferred {
        message: message.into(),
        recovery_command: recovery_command()?,
    })
}

fn legacy_deferred(reason: &str) -> crate::Result<Outcome> {
    let home = crate::infra::config::agit_home()?;
    #[cfg(unix)]
    let message = format!(
        "{reason}; finish user work, explicitly stop this local daemon, then independently confirm \
         all daemons and starters using this AGIT_HOME's local namespace have exited before running {} and retrying startup",
        stopped_recovery_command(&home)?
    );
    #[cfg(windows)]
    let message =
        format!("{reason}; finish user work, then explicitly stop and start this local daemon");
    Ok(Outcome::Deferred {
        message,
        recovery_command: local_command(&home, &std::env::current_exe()?, "stop")?,
    })
}

fn require_usable(outcome: Outcome) -> crate::Result<()> {
    if let Outcome::Deferred {
        message,
        recovery_command,
    } = outcome
    {
        bail!(
            "local daemon is incompatible or still using an old build: {message}. Recovery: {recovery_command}"
        );
    }
    Ok(())
}

fn ready(requirements: &Requirements<'_>) -> crate::Result<()> {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Ok(Some(status)) = probe() {
            ensure!(
                requirements.accepts(&status),
                "replacement local daemon does not provide the required build or RPC features"
            );
            if status.online {
                super::local::wait_ready()?;
                return Ok(());
            }
        }
        ensure!(
            Instant::now() < deadline,
            "local daemon did not become ready; inspect agitd-*.log in {}",
            super::rc_dir()?.display()
        );
        std::thread::sleep(POLL);
    }
}

fn replace(status: &Status, requirements: &Requirements<'_>) -> crate::Result<Outcome> {
    let Some(identity) = &status.identity else {
        return legacy_deferred("the running daemon predates safe restart negotiation");
    };
    if !identity
        .rpc_features
        .iter()
        .any(|feature| feature == "safe-restart-v1")
    {
        return legacy_deferred("the running daemon does not support safe restart");
    }
    match control::ask(&Request::StopIfIdle {
        instance_id: identity.instance_id.clone(),
        build_id: identity.build_id.clone(),
    })? {
        Reply::Stopping => {}
        Reply::Busy { blockers } => return deferred(blockers.join("; ")),
        Reply::InstanceChanged => {
            return deferred("the daemon instance changed during reconciliation; retry attachment");
        }
        other => return deferred(format!("safe restart was not accepted: {other:?}")),
    }
    let deadline = Instant::now() + WAIT;
    loop {
        match probe() {
            Ok(None) => break,
            Ok(Some(current))
                if current
                    .identity
                    .as_ref()
                    .is_some_and(|current| current.instance_id != identity.instance_id) =>
            {
                if requirements.accepts(&current) {
                    ready(requirements)?;
                    return Ok(Outcome::Unchanged);
                }
                return deferred(
                    "another daemon took ownership while waiting for the old instance to exit",
                );
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return deferred("the old daemon has not exited; no replacement was started");
        }
        std::thread::sleep(POLL);
    }
    super::local::spawn_daemon()?;
    ready(requirements)?;
    Ok(Outcome::Restarted)
}

pub fn attach(ensure_daemon: bool, extra: &[String], current_build: bool) -> crate::Result<()> {
    attach_inner(ensure_daemon, extra, current_build).with_context(|| {
        format!(
            "local daemon attachment failed in {}",
            super::rc_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_default()
        )
    })
}

pub fn stop_and_wait() -> crate::Result<Reply> {
    let _lock = lock()?;
    let reply = control::ask(&Request::Stop)?;
    ensure!(
        matches!(reply, Reply::Stopping),
        "daemon refused stop: {reply:?}"
    );
    // Stop acknowledges admission; attachment must wait for lifetime ownership to be released.
    let deadline = Instant::now() + WAIT;
    while control::presence() != Presence::Absent {
        ensure!(
            Instant::now() < deadline,
            "daemon accepted stop but has not exited; no replacement was started"
        );
        std::thread::sleep(POLL);
    }
    Ok(reply)
}

fn attach_inner(ensure_daemon: bool, extra: &[String], current_build: bool) -> crate::Result<()> {
    let requirements = Requirements {
        extra,
        current_build,
    };
    requirements.validate()?;
    let _lock = lock()?;
    let result = match probe()? {
        None if ensure_daemon => {
            super::local::spawn_daemon()?;
            ready(&requirements)?;
            Outcome::Restarted
        }
        None => bail!("no local daemon is running; start it or pass --ensure"),
        Some(status) if requirements.accepts(&status) => {
            ready(&requirements)?;
            Outcome::Unchanged
        }
        Some(status) if ensure_daemon => replace(&status, &requirements)?,
        Some(_) => deferred(
            "the running daemon lacks the required build or RPC features; attach with --ensure to attempt a safe replacement",
        )?,
    };
    require_usable(result)
}

pub fn restart_if_idle() -> crate::Result<Outcome> {
    let _lock = lock()?;
    match probe()? {
        None => Ok(Outcome::Absent),
        Some(status) => replace(
            &status,
            &Requirements {
                extra: &[],
                current_build: true,
            },
        ),
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    std::fs::canonicalize(left).unwrap_or_else(|_| left.into())
        == std::fs::canonicalize(right).unwrap_or_else(|_| right.into())
}

pub fn upgrade_target(executable: &Path) -> crate::Result<Option<UpgradeTarget>> {
    super::select_local_authority();
    let Some(status) = probe()? else {
        return Ok(None);
    };
    if status
        .identity
        .as_ref()
        .is_some_and(|identity| !same_path(&identity.executable, executable))
    {
        return Ok(None);
    }
    Ok(Some(UpgradeTarget {
        pid: status.pid,
        instance_id: status.identity.map(|identity| identity.instance_id),
        executable: executable.into(),
    }))
}

pub fn after_upgrade(target: UpgradeTarget) -> crate::Result<Outcome> {
    let _lock = lock()?;
    let Some(status) = probe()? else {
        return Ok(Outcome::Absent);
    };
    if status.pid != target.pid
        || status
            .identity
            .as_ref()
            .map(|identity| &identity.instance_id)
            != target.instance_id.as_ref()
    {
        return if status
            .identity
            .as_ref()
            .is_some_and(|identity| identity.build_id == BUILD_ID)
        {
            Ok(Outcome::Unchanged)
        } else {
            deferred(
                "the daemon changed during installation and has not confirmed the installed build; retry a safe restart",
            )
        };
    }
    if let Some(identity) = &status.identity {
        ensure!(
            same_path(&target.executable, &std::env::current_exe()?),
            "upgrade reconciliation must run from the installed executable"
        );
        ensure!(
            same_path(&identity.executable, &target.executable),
            "the running daemon belongs to another installation"
        );
        if identity.build_id == BUILD_ID {
            return Ok(Outcome::Unchanged);
        }
    }
    replace(
        &status,
        &Requirements {
            extra: &[],
            current_build: true,
        },
    )
}

pub fn report(outcome: &Outcome) -> crate::Result<()> {
    match outcome {
        Outcome::Deferred {
            message,
            recovery_command,
        } => {
            eprintln!("Local daemon restart deferred: {message}.\nRecovery: {recovery_command}");
        }
        Outcome::Restarted => eprintln!("Local daemon restarted with the installed build."),
        Outcome::Absent | Outcome::Unchanged => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn recovery_uses_the_saved_installation_path_after_replacement() {
        let executable = Path::new("/tmp/installed cli/agit");
        let command = recovery_command_for(executable).unwrap();
        let words = shlex::split(&command).unwrap();
        assert!(words[0].starts_with("AGIT_HOME="));
        assert_eq!(
            &words[1..],
            &[
                "/tmp/installed cli/agit",
                "rc",
                "local",
                "restart",
                "--if-idle"
            ]
        );
    }

    #[test]
    fn attachment_distinguishes_capability_compatibility_from_build_replacement() {
        let legacy: Reply = serde_json::from_value(serde_json::json!({
            "reply":"status", "pid":1, "hub":"local-owner", "online":true,
            "uptime_secs":1, "agit_version":env!("CARGO_PKG_VERSION"), "sessions":[]
        }))
        .unwrap();
        let Reply::Status(mut status) = legacy else {
            panic!("expected legacy status")
        };
        let compatible = Requirements {
            extra: &[],
            current_build: false,
        };
        assert!(!compatible.accepts(&status));
        let mut identity = super::super::build_identity::DaemonIdentity::current().unwrap();
        identity.build_id = "another-build-with-the-same-version".into();
        status.identity = Some(identity);
        assert!(
            compatible.accepts(&status),
            "compatible clients must not replace each other's builds"
        );
        assert!(
            !Requirements {
                extra: &[],
                current_build: true
            }
            .accepts(&status)
        );
        status
            .identity
            .as_mut()
            .unwrap()
            .rpc_features
            .retain(|feature| feature != "peer-control-v1");
        assert!(!compatible.accepts(&status));
        assert!(
            Requirements {
                extra: &["unknown-feature".into()],
                current_build: false
            }
            .validate()
            .is_err()
        );
    }
}
