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
const UPGRADE_RETRY: Duration = Duration::from_secs(5);
const REQUIRED: &[&str] = &["peer-control-v1", "history-v2"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeTarget {
    pid: u32,
    instance_id: Option<String>,
    executable: PathBuf,
    #[serde(default)]
    allow_other_installation: bool,
    #[serde(default)]
    watch_id: Option<String>,
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
            "local daemon did not become ready; inspect agitd-*.log (startup output) and diagnostics-*.jsonl (what it did) in {}",
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
    upgrade_target_for(executable, true)
}

fn upgrade_target_for(
    executable: &Path,
    allow_other_installation: bool,
) -> crate::Result<Option<UpgradeTarget>> {
    super::select_local_authority();
    let Some(status) = probe()? else {
        return Ok(None);
    };
    if status.identity.as_ref().is_some_and(|identity| {
        !allow_other_installation && !same_path(&identity.executable, executable)
    }) {
        return Ok(None);
    }
    Ok(Some(UpgradeTarget {
        pid: status.pid,
        instance_id: status.identity.map(|identity| identity.instance_id),
        executable: executable.into(),
        allow_other_installation,
        watch_id: None,
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
    if target.allow_other_installation
        && crate::commands::upgrade::is_older_than(env!("CARGO_PKG_VERSION"), &status.agit_version)
            != Some(false)
    {
        return Ok(Outcome::Unchanged);
    }
    if let Some(identity) = &status.identity {
        ensure!(
            same_path(&target.executable, &std::env::current_exe()?),
            "upgrade reconciliation must run from the installed executable"
        );
        if !target.allow_other_installation {
            ensure!(
                same_path(&identity.executable, &target.executable),
                "the running daemon belongs to another installation"
            );
        }
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

/// Reconcile an out-of-band installer against the daemon that currently owns this executable.
pub fn reconcile_installed(allow_other_installation: bool) -> crate::Result<Outcome> {
    let exe = std::env::current_exe()?;
    let Some(target) = upgrade_target_for(&exe, allow_other_installation)? else {
        return Ok(Outcome::Absent);
    };
    let outcome = after_upgrade(target.clone())?;
    if matches!(outcome, Outcome::Deferred { .. }) {
        schedule_upgrade_watch(&exe, &target)?;
    }
    Ok(outcome)
}

/// Keep trying the guarded handoff while this installation remains the latest requested build.
/// A later installer supersedes this watcher, and a competing safe restart can change the daemon.
pub fn watch_upgrade(mut target: UpgradeTarget) -> crate::Result<()> {
    use sha2::Digest as _;
    let dir = super::rc_dir()?;
    let path = dir.join("upgrade-watch.lock");
    let latest = dir.join("upgrade-watch.latest");
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    fs2::FileExt::lock_exclusive(&file)?;
    let digest = sha2::Sha256::digest(std::fs::read(&target.executable)?);
    let mut last_error = None;
    loop {
        std::thread::sleep(UPGRADE_RETRY);
        if target
            .watch_id
            .as_ref()
            .is_some_and(|id| std::fs::read_to_string(&latest).ok().as_deref() != Some(id.as_str()))
        {
            return Ok(());
        }
        let Ok(installed) = std::fs::read(&target.executable) else {
            continue;
        };
        if sha2::Sha256::digest(installed) != digest {
            return Ok(());
        }
        let current = match probe() {
            Ok(Some(status)) => status,
            Ok(None) => return Ok(()),
            Err(_) => continue,
        };
        if current.pid != target.pid
            || current
                .identity
                .as_ref()
                .map(|identity| &identity.instance_id)
                != target.instance_id.as_ref()
        {
            if current
                .identity
                .as_ref()
                .is_some_and(|identity| identity.build_id == BUILD_ID)
                || crate::commands::upgrade::is_older_than(
                    env!("CARGO_PKG_VERSION"),
                    &current.agit_version,
                ) != Some(false)
            {
                return Ok(());
            }
            target.pid = current.pid;
            target.instance_id = current.identity.map(|identity| identity.instance_id);
        }
        match after_upgrade(target.clone()) {
            Ok(Outcome::Restarted) => {
                eprintln!("Local daemon restarted with the installed build.");
                return Ok(());
            }
            Ok(Outcome::Unchanged | Outcome::Absent) => return Ok(()),
            Ok(Outcome::Deferred { .. }) => {}
            Err(error) => {
                let message = format!("{error:#}");
                if last_error.as_ref() != Some(&message) {
                    eprintln!("Local daemon upgrade retry failed: {message}");
                    last_error = Some(message);
                }
            }
        }
    }
}

/// A reconciliation helper outlives the daemon it replaces, so it leads its own session.
fn detached_reconciler(exe: &Path) -> crate::Result<std::process::Command> {
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;
    let mut log_options = std::fs::OpenOptions::new();
    log_options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        log_options.mode(0o600);
    }
    let log = log_options.open(super::rc_dir()?.join("upgrade-watch.log"))?;
    let mut command = crate::infra::background::command(exe);
    command
        .env("AGIT_TELEMETRY_DEFER", "1")
        .env(crate::commands::upgrade::RESTART_ENV, "1")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command)
}

pub fn schedule_upgrade_watch(exe: &Path, target: &UpgradeTarget) -> crate::Result<()> {
    let mut watched = target.clone();
    watched.watch_id = Some(uuid::Uuid::new_v4().to_string());
    let mut command = detached_reconciler(exe)?;
    command
        .args(["rc", "local", "watch-upgrade", "--target"])
        .arg(serde_json::to_string(&watched)?);
    let mut child = command
        .spawn()
        .context("cannot start background daemon upgrade watcher")?;
    if let Err(error) = std::fs::write(
        super::rc_dir()?.join("upgrade-watch.latest"),
        watched.watch_id.as_deref().unwrap_or_default(),
    ) {
        let _ = child.kill();
        return Err(error.into());
    }
    Ok(())
}

#[cfg(unix)]
const INSTALLATION_POLL: Duration = Duration::from_secs(10);

/// The file at a daemon's launch path, as far as a replacement can tell it apart.
#[cfg(unix)]
type InstalledFile = Option<(u64, u64, u64, i64, i64)>;

#[cfg(unix)]
fn installed_file(path: &Path) -> InstalledFile {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|metadata| {
        (
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
        )
    })
}

/// Decides when a replaced installation is ready to take over the running daemon.
#[cfg(unix)]
struct InstallationWatch {
    handled: InstalledFile,
    seen: InstalledFile,
    failures: u32,
    wait: u32,
}

#[cfg(unix)]
impl InstallationWatch {
    fn new(running: InstalledFile) -> Self {
        Self {
            handled: running,
            seen: running,
            failures: 0,
            wait: 0,
        }
    }

    /// Whether `current` is a settled replacement that has not yet been handed off. An
    /// installer still writing the file shows a different file at every poll, and starting its
    /// build before the write finishes would run a truncated executable. An `urgent` look, made
    /// because a helper could not start from the running image, does not wait out a backoff.
    fn replaced(&mut self, current: InstalledFile, urgent: bool) -> bool {
        let settled = current.is_some() && current == self.seen;
        self.seen = current;
        if !settled {
            self.failures = 0;
            self.wait = 0;
            return false;
        }
        if current == self.handled {
            return false;
        }
        if self.wait > 0 && !urgent {
            self.wait -= 1;
            return false;
        }
        true
    }

    /// Records the outcome of handing off the replacement `replaced` last reported. Only a
    /// reconciliation that completed (restarted, deferred to its own watcher, or found nothing
    /// to do) settles it. A failed one is retried after 1, 2, 4, … up to 32 polls, since its
    /// cause (a missing execute bit, a busy lock) can clear without the file changing again.
    fn finished(&mut self, succeeded: bool) {
        if succeeded {
            self.handled = self.seen;
            self.failures = 0;
            self.wait = 0;
        } else {
            self.wait = 1 << self.failures.min(5);
            self.failures += 1;
        }
    }
}

/// Why the installation watch looks at the launch path now.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Look {
    /// The regular poll.
    Poll,
    /// A helper could not be started from the running image.
    Suspect,
}

#[cfg(unix)]
static INSTALLATION_LOOKS: std::sync::OnceLock<std::sync::mpsc::Sender<Look>> =
    std::sync::OnceLock::new();

/// A helper the daemon starts from its own image failed to start in a way a replaced
/// installation explains (the file gone, stale, unreadable or no longer an executable): the
/// installation watch looks at once instead of at its next poll.
pub(crate) fn helper_failed_to_start(error: &anyhow::Error) {
    #[cfg(unix)]
    if let Some(looks) = INSTALLATION_LOOKS.get() {
        suspect_installation(error, looks);
    }
    #[cfg(not(unix))]
    let _ = error;
}

#[cfg(unix)]
fn suspect_installation(error: &anyhow::Error, looks: &std::sync::mpsc::Sender<Look>) -> bool {
    let replaced = error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .filter_map(std::io::Error::raw_os_error)
        .any(|code| [libc::ESTALE, libc::ENOENT, libc::EACCES, libc::ENOEXEC].contains(&code));
    replaced && looks.send(Look::Suspect).is_ok()
}

/// A daemon whose executable is replaced keeps running only the image it loaded: every helper
/// it starts by path then fails or runs another build, so new controller links, settlement and
/// privacy checks stop while the conversations it holds still look attached. The installed
/// build takes over through the same guarded reconciliation an installer runs; a deferred
/// handoff keeps retrying in that reconciliation's own watcher.
#[cfg(unix)]
pub(crate) fn watch_installation(executable: PathBuf) -> std::io::Result<()> {
    let (looks, looked) = std::sync::mpsc::channel();
    if INSTALLATION_LOOKS.set(looks).is_err() {
        return Ok(());
    }
    let watch = InstallationWatch::new(installed_file(&executable));
    std::thread::Builder::new()
        .name("installation-watch".into())
        .spawn(move || {
            let reconcile = || {
                let reconciled = detached_reconciler(&executable).and_then(|mut command| {
                    command.args(["rc", "local", "reconcile-installed"]);
                    Ok(command.spawn()?.wait()?)
                });
                let failure = match reconciled {
                    Ok(status) if status.success() => return Ok(()),
                    Ok(status) => format!("its reconciliation exited with {status}"),
                    Err(error) => format!("{error:#}"),
                };
                eprintln!(
                    "agitd: {} was replaced, but its build could not take over yet: {failure}",
                    executable.display()
                );
                Err(())
            };
            let next = || match looked.recv_timeout(INSTALLATION_POLL) {
                Ok(look) => Some(look),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Some(Look::Poll),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
            };
            run_installation_watch(watch, next, || installed_file(&executable), reconcile);
        })?;
    Ok(())
}

/// Hands the installation over each time a look finds a settled replacement, until `next`
/// ends.
#[cfg(unix)]
fn run_installation_watch(
    mut watch: InstallationWatch,
    mut next: impl FnMut() -> Option<Look>,
    probe: impl Fn() -> InstalledFile,
    mut reconcile: impl FnMut() -> Result<(), ()>,
) {
    while let Some(look) = next() {
        if watch.replaced(probe(), look == Look::Suspect) {
            watch.finished(reconcile().is_ok());
        }
    }
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

    /// A replaced installation hands off once it settles and until a handoff completes. Firing
    /// on the first changed poll would start a build an installer is still writing; firing on
    /// every poll after a completed handoff would stack reconciliations while a busy daemon
    /// defers its restart; never retrying a failed one would leave the daemon on the old build.
    #[cfg(unix)]
    #[test]
    fn installation_watch_retries_a_settled_replacement_until_its_handoff_completes() {
        let file = |ino, len| Some((1, ino, len, 0, 0));
        let mut watch = InstallationWatch::new(file(1, 10));
        assert!(
            !watch.replaced(file(1, 10), false),
            "the running build is not a replacement"
        );
        assert!(
            !watch.replaced(file(2, 4), false),
            "a file still being written must settle first"
        );
        assert!(!watch.replaced(file(2, 8), false));
        assert!(watch.replaced(file(2, 8), false));
        watch.finished(false);
        assert!(
            !watch.replaced(file(2, 8), false),
            "a failed handoff backs off"
        );
        assert!(
            watch.replaced(file(2, 8), false),
            "a failed handoff is retried without a new installation"
        );
        watch.finished(false);
        assert!(!watch.replaced(file(2, 8), false));
        assert!(!watch.replaced(file(2, 8), false));
        assert!(
            watch.replaced(file(2, 8), false),
            "each failure doubles the wait"
        );
        watch.finished(true);
        assert!(
            !watch.replaced(file(2, 8), false),
            "a completed handoff is not repeated"
        );
        assert!(
            !watch.replaced(None, false),
            "a missing installation has nothing to run"
        );
        assert!(!watch.replaced(None, false));
        assert!(!watch.replaced(file(3, 8), false));
        assert!(
            watch.replaced(file(3, 8), false),
            "a later installation hands off again"
        );
    }

    /// Runs the installation watch over a script of looks, each made while the launch path
    /// holds the given file, with reconciliations answering in turn; returns how many ran.
    #[cfg(unix)]
    fn watched(
        running: InstalledFile,
        looks: &[(InstalledFile, Look)],
        answers: &[Result<(), ()>],
    ) -> usize {
        let current = std::cell::Cell::new(running);
        let mut looks = looks.iter();
        let mut answers = answers.iter();
        let mut ran = 0;
        run_installation_watch(
            InstallationWatch::new(running),
            || {
                let (file, look) = looks.next()?;
                current.set(*file);
                Some(*look)
            },
            || current.get(),
            || {
                ran += 1;
                *answers.next().unwrap_or(&Ok(()))
            },
        );
        ran
    }

    /// The regular poll alone hands a replaced installation over once it settles, and once only.
    /// Without it a daemon whose executable was renamed over keeps every helper it starts
    /// failing until someone restarts it (#298).
    #[cfg(unix)]
    #[test]
    fn the_poll_hands_a_replaced_installation_over() {
        let file = |ino, len| Some((1, ino, len, 0, 0));
        let poll = |file| (file, Look::Poll);
        assert_eq!(
            watched(
                file(1, 10),
                &[
                    poll(file(1, 10)),
                    poll(file(2, 8)),
                    poll(file(2, 8)),
                    poll(file(2, 8))
                ],
                &[]
            ),
            1
        );
        assert_eq!(watched(file(1, 10), &[poll(file(1, 10)); 4], &[]), 0);
    }

    /// A helper that cannot start from the running image because its file is gone, stale,
    /// unreadable or no longer an executable makes the watch look at once, past any backoff:
    /// on the Sim's read-only share every Cloud offer failed until a restart (#298). Any other
    /// failure to start is no sign of a replacement.
    #[cfg(unix)]
    #[test]
    fn a_helper_that_cannot_start_hands_the_installation_over_at_once() {
        let (looks, looked) = std::sync::mpsc::channel();
        for code in [libc::ESTALE, libc::ENOENT, libc::EACCES, libc::ENOEXEC] {
            let error = anyhow::Error::from(std::io::Error::from_raw_os_error(code))
                .context("cannot start the tunnel worker");
            assert!(suspect_installation(&error, &looks), "errno {code}");
            assert_eq!(looked.try_recv(), Ok(Look::Suspect));
        }
        let refused = anyhow::Error::from(std::io::Error::from_raw_os_error(libc::ECONNREFUSED));
        assert!(!suspect_installation(&refused, &looks));
        assert!(!suspect_installation(
            &anyhow::anyhow!("tunnel worker closed"),
            &looks
        ));
        assert!(looked.try_recv().is_err());

        let file = |ino, len| Some((1, ino, len, 0, 0));
        let suspect = |file| (file, Look::Suspect);
        assert_eq!(
            watched(
                file(1, 10),
                &[
                    suspect(file(2, 8)),
                    suspect(file(2, 8)),
                    suspect(file(2, 8))
                ],
                &[Err(())]
            ),
            2,
            "a failed handoff is retried at the next failed start, not after its backoff"
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
