//! Native Claude control binds to a process and session generation, never a transcript guess.

mod approval;
pub(crate) mod completion;
pub(crate) mod event_queue;
mod install;
pub(crate) mod permission;
pub(crate) mod status;
mod transport;
pub(crate) use approval::wait_approval;
pub use install::install_plugin;
pub(crate) use transport::TerminalActivity;
#[cfg(test)]
pub(crate) use transport::fixture;
pub use transport::{Client, Endpoint, Snapshot, Update};

use anyhow::{Context, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const RECORD_LIMIT: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Process {
    pub pid: u32,
    birth: Birth,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct Birth {
    boot: String,
    seconds: u64,
    fraction: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    pub version: u32,
    pub session: String,
    pub cwd: PathBuf,
    pub generation: String,
    pub process: Process,
    #[serde(default)]
    pub descriptor: PathBuf,
    revision: Birth,
    registry: PathBuf,
}

fn directory() -> crate::Result<PathBuf> {
    let directory = crate::infra::config::agit_home()?.join("native-claude");
    crate::infra::config::create_state_dir(&directory)?;
    let metadata = std::fs::symlink_metadata(&directory)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "native control directory must be private to its owner"
    );
    Ok(directory)
}

fn read_registration(path: &Path) -> crate::Result<Registration> {
    let file = super::native_inbox::open_regular(path)?;
    ensure!(
        file.metadata()?.uid() == unsafe { libc::geteuid() },
        "native control record has another owner"
    );
    let mut bytes = Vec::new();
    file.take(RECORD_LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= RECORD_LIMIT,
        "native control record exceeds its size limit"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

impl Registration {
    pub fn transcript_path(&self) -> crate::Result<PathBuf> {
        let path = self.transcript_target()?;
        ensure!(path.try_exists()?, "native transcript has not been written");
        Ok(path)
    }

    pub(crate) fn transcript_target(&self) -> crate::Result<PathBuf> {
        ensure!(
            self.is_registered(),
            "native registration is no longer current"
        );
        let live = self
            .live()
            .ok()
            .context("native writer is no longer available")?;
        super::claude_inbox::live_transcript_target_in(
            &live,
            &self.cwd,
            &self
                .registry
                .parent()
                .context("native registry has no runtime home")?
                .join("projects"),
        )
    }
    /// Both the OS lifetime and the runtime's current session must still agree.
    pub fn is_current(&self) -> bool {
        self.refusal().is_ok()
    }

    /// Why this registration does not name the conversation's current writer. Only another live
    /// process naming the conversation, one not admitted beside it, is the typed
    /// [`NotCurrentWriter`]; a process that no longer matches, is not listed yet, or whose
    /// directory no longer matches is the plain error it always was.
    fn refusal(&self) -> crate::Result<()> {
        let mismatch = || anyhow::anyhow!("the caller is not the current native session writer");
        if self.version != 1
            || !process(self.process.pid).is_some_and(|(process, _)| process == self.process)
        {
            return Err(mismatch());
        }
        let live = match self.live() {
            Ok(live) => live,
            Err(super::claude_inbox::Missing::Shared) => return Err(NotCurrentWriter.into()),
            Err(super::claude_inbox::Missing::NotListed) => return Err(mismatch()),
        };
        if live.pid() == self.process.pid
            && live.cwd().and_then(|cwd| cwd.canonicalize().ok()).as_ref() == Some(&self.cwd)
        {
            Ok(())
        } else {
            Err(mismatch())
        }
    }

    /// This registered process as Claude Code's session registry names it. The only other process
    /// that may name the same conversation meanwhile is the daemon's own headless writer handing
    /// it over (or whose release failed), as the lease file records it by pid and start time:
    /// otherwise a terminal opened during a web turn could never register, and its hold would
    /// end. Of two terminals on one conversation the first stays its writer, so one of them can
    /// write: a terminal opened later never takes this one's place, and is held until this one
    /// closes. Only a terminal counts: a headless writer started later is running a turn, and
    /// this process must not write beside it. Any other second process still makes it no writer
    /// at all, a start time that can't be read included.
    fn live(&self) -> Result<super::claude_inbox::Live, super::claude_inbox::Missing> {
        let writer = super::lease::path()
            .and_then(|path| super::lease::Leases::handover_writer_in(&path, &self.session));
        let own = started_ms(self.process.pid).map(|started| (started, self.process.pid));
        super::claude_inbox::discover_pid_in(
            &self.registry,
            &self.session,
            self.process.pid,
            |other, terminal| {
                let later = own.is_some_and(|own| {
                    started_ms(other).is_some_and(|started| (started, other) > own)
                });
                (terminal && later)
                    || writer.is_some_and(|writer| {
                        writer.pid == other && started_ms(other) == Some(writer.started_ms)
                    })
            },
        )
    }

    /// A delayed poll cannot re-enroll a generation superseded by a native transition.
    pub fn is_registered(&self) -> bool {
        directory()
            .ok()
            .and_then(|directory| {
                read_registration(&directory.join(format!("{}.json", self.process.pid))).ok()
            })
            .is_some_and(|current| current == *self && self.is_current())
    }
}

/// The registration of the native writer that runs `session` and is an ancestor of this process.
/// A settings hook is the writer's descendant through the shell, and any launcher, that runs it;
/// a process outside that tree cannot speak for the session.
pub(crate) fn ancestor_registration(session: &str) -> Option<Registration> {
    const ANCESTRY: usize = 6;
    let directory = crate::infra::config::agit_home()
        .ok()?
        .join("native-claude");
    let mut pid = std::process::id();
    for _ in 0..ANCESTRY {
        let (_, parent) = process(pid)?;
        if let Ok(registration) = read_registration(&directory.join(format!("{parent}.json")))
            && registration.process.pid == parent
            && registration.session == session
            && registration.is_registered()
        {
            return Some(registration);
        }
        pid = parent;
    }
    None
}

/// The registering process's parent is not the one process that writes the session now: another
/// process also names it, and is not the daemon's own writer handing it over. A definite refusal,
/// unlike a failure to reach agit.
#[derive(Debug)]
pub struct NotCurrentWriter;

impl std::fmt::Display for NotCurrentWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the caller is not the current native session writer")
    }
}

impl std::error::Error for NotCurrentWriter {}

/// Only a child of the registered native writer can announce its current generation.
pub fn register(session: &str, generation: &str) -> crate::Result<Registration> {
    ensure!(
        super::native_inbox::valid_id(session) && super::native_inbox::valid_id(generation),
        "native control requires exact session and generation UUIDs"
    );
    let (child, parent_pid) =
        process(std::process::id()).context("cannot verify the registration process")?;
    let (parent, _) = process(parent_pid).context("cannot verify the native parent process")?;
    let registry = crate::adapter::claude_code::sessions_dir()?.canonicalize()?;
    let cwd = std::env::current_dir()?.canonicalize()?;
    let preferred = std::env::var_os("AGIT_NATIVE_CONTROL_DESCRIPTOR");
    let registration = Registration {
        version: 1,
        session: session.to_owned(),
        cwd,
        generation: generation.to_owned(),
        process: parent,
        revision: child.birth,
        descriptor: discover_descriptor(preferred.as_deref().map(Path::new))?,
        registry,
    };
    registration.refusal()?;
    publish(&directory()?, &registration)
}

pub fn descriptor_path() -> crate::Result<PathBuf> {
    Ok(super::rc_dir()?.join("claude-native.json"))
}

pub fn discover_descriptor(preferred: Option<&Path>) -> crate::Result<PathBuf> {
    transport::discover_descriptor(preferred)
}

fn publish(directory: &Path, registration: &Registration) -> crate::Result<Registration> {
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join(format!("{}.lock", registration.process.pid)))?;
    lock.lock_exclusive()?;
    let path = directory.join(format!("{}.json", registration.process.pid));
    match read_registration(&path) {
        Ok(current) => {
            if current.process == registration.process
                && current.session == registration.session
                && current.generation == registration.generation
                && current.cwd == registration.cwd
                && current.registry == registration.registry
                && current.descriptor == registration.descriptor
            {
                // Rediscovery preserves the generation's original lifetime fence.
                return Ok(current);
            }
            ensure!(
                current.process != registration.process || registration.revision > current.revision,
                "a newer native session registration is already present"
            );
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
        Err(error) => return Err(error),
    }
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut temporary, registration)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(&path)?;
    super::native_inbox::sync_directory(directory)?;
    Ok(registration.clone())
}

#[cfg(target_os = "macos")]
fn process(pid: u32) -> Option<(Process, u32)> {
    let pid = i32::try_from(pid).ok().filter(|pid| *pid > 0)?;
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as i32;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (read == size && info.pbi_uid == unsafe { libc::geteuid() }).then_some((
        Process {
            pid: pid as u32,
            birth: Birth {
                boot: String::new(),
                seconds: info.pbi_start_tvsec,
                fraction: info.pbi_start_tvusec,
            },
        },
        info.pbi_ppid,
    ))
}

#[cfg(target_os = "linux")]
fn process(pid: u32) -> Option<(Process, u32)> {
    if pid == 0 {
        return None;
    }
    let path = PathBuf::from(format!("/proc/{pid}"));
    if std::fs::metadata(&path).ok()?.uid() != unsafe { libc::geteuid() } {
        return None;
    }
    let stat = std::fs::read_to_string(path.join("stat")).ok()?;
    let fields: Vec<_> = stat.rsplit_once(") ")?.1.split_whitespace().collect();
    Some((
        Process {
            pid,
            birth: Birth {
                boot: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                    .ok()?
                    .trim()
                    .to_owned(),
                seconds: fields.get(19)?.parse().ok()?,
                fraction: 0,
            },
        },
        fields.get(1)?.parse().ok()?,
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process(_: u32) -> Option<(Process, u32)> {
    None
}

/// The Claude Code process that runs `session` and is an ancestor of this process: the terminal
/// whose settings hook is running. Another process naming the same session (a headless writer)
/// is not in this hook's ancestry.
pub(crate) fn terminal_of_hook(session: &str) -> Option<u32> {
    const ANCESTRY: usize = 6;
    let candidates = super::claude_inbox::session_pids(session);
    let mut pid = std::process::id();
    for _ in 0..ANCESTRY {
        let (_, parent) = process(pid)?;
        if candidates.contains(&parent) {
            return Some(parent);
        }
        pid = parent;
    }
    None
}

/// What the daemons on this machine said about a terminal that loaded a conversation.
pub(crate) enum Announced {
    /// A daemon runs the conversation: the terminal's lease state, and the line it shows while
    /// its own input must wait.
    Lease(super::lease::State, Option<String>),
    /// No daemon runs it, or none is running at all: nothing holds the terminal back.
    Free,
    /// A daemon is there but did not answer; the terminal's state is unknown.
    Unknown,
}

impl Announced {
    pub(crate) fn json(&self) -> serde_json::Value {
        match self {
            Self::Lease(state, notice) => {
                serde_json::json!({"known": true, "state": state, "notice": notice})
            }
            Self::Free => serde_json::json!({"known": true, "state": null, "notice": null}),
            Self::Unknown => serde_json::json!({"known": false}),
        }
    }
}

/// Tell the daemons on this machine, in either namespace, that the terminal `pid` loaded
/// `session`, continuing the branch `loaded` once its plugin has named one.
pub(crate) fn announce_terminal(
    session: &str,
    pid: u32,
    plugin: bool,
    loaded: Option<super::lease::Loaded>,
) -> Announced {
    use super::control::{Reply, Request};
    const BOUND: std::time::Duration = std::time::Duration::from_secs(1);
    let request = Request::TerminalOpened {
        thread_id: session.to_owned(),
        pid,
        plugin,
        loaded,
    };
    let Ok(home) = crate::infra::config::agit_home() else {
        return Announced::Unknown;
    };
    let mut unanswered = false;
    for namespace in ["rc", "desktop-rc"] {
        let directory = home.join(namespace);
        let socket = super::control::socket_path_for(&directory);
        if !directory.is_dir() || !socket.exists() {
            continue;
        }
        match super::control::ask_at(&socket, &request, BOUND) {
            Ok(Reply::TerminalOpened {
                state: Some(state),
                notice,
            }) => return Announced::Lease(state, notice),
            Ok(Reply::TerminalOpened { state: None, .. }) => {}
            // A socket nothing listens on is left by a daemon that is gone, which holds no
            // conversation. One connect decides it within the terminal's hook budget; a daemon
            // that accepts and does not answer is unknown.
            Err(_)
                if super::control::connect_within(&socket, BOUND).is_err_and(|error| {
                    matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    )
                }) => {}
            _ => unanswered = true,
        }
    }
    if unanswered {
        Announced::Unknown
    } else {
        Announced::Free
    }
}

/// Tell the daemons on this machine that the headless writer of `session` finished a turn on
/// record `leaf`. Only the daemon running that writer keeps it.
pub(crate) fn report_writer_leaf(session: &str, leaf: &str) {
    use super::control::Request;
    const BOUND: std::time::Duration = std::time::Duration::from_secs(1);
    let request = Request::WriterLeaf {
        thread_id: session.to_owned(),
        leaf: leaf.to_owned(),
    };
    let Ok(home) = crate::infra::config::agit_home() else {
        return;
    };
    for namespace in ["rc", "desktop-rc"] {
        let socket = super::control::socket_path_for(&home.join(namespace));
        if socket.exists() {
            let _ = super::control::ask_at(&socket, &request, BOUND);
        }
    }
}

/// When `pid` started, in wall-clock milliseconds, rounded down to the clock's resolution.
pub(crate) fn started_ms(pid: u32) -> Option<i64> {
    let (process, _) = process(pid)?;
    #[cfg(target_os = "linux")]
    {
        // Linux counts the start in clock ticks after boot; `btime` is the boot in epoch seconds.
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let boot: i64 = stat
            .lines()
            .find_map(|line| line.strip_prefix("btime "))?
            .trim()
            .parse()
            .ok()?;
        // `c_long` is narrower than `i64` on 32-bit targets.
        #[allow(clippy::useless_conversion)]
        let ticks = i64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) })
            .ok()
            .filter(|ticks| *ticks > 0)?;
        let after_boot = i64::try_from(process.birth.seconds).ok()?;
        Some(boot * 1_000 + after_boot * 1_000 / ticks)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Some(
            i64::try_from(process.birth.seconds).ok()? * 1_000
                + i64::try_from(process.birth.fraction).ok()? / 1_000,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One registered Claude Code process: a running process and its listening socket.
    #[cfg(unix)]
    fn registered(
        root: &Path,
        registry: &Path,
        session: &str,
        name: &str,
    ) -> (std::process::Child, std::os::unix::net::UnixListener) {
        let child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let socket = root.join(format!("{name}.sock"));
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        crate::rc::claude_inbox::fixture::register(registry, child.id(), session, &socket, None);
        (child, listener)
    }

    /// Only another live process naming the conversation is the typed refusal the plugin holds on
    /// for good. A lone registered process whose directory no longer matches, or one not listed
    /// yet, gets the plain error, which the plugin bounds like an outage: the typed refusal there
    /// would hold a terminal for good under a line blaming a process that does not exist.
    #[cfg(unix)]
    #[test]
    fn only_another_live_process_is_the_typed_refusal() {
        let root = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(root.path(), || {
            let registry = root.path().join("sessions");
            let session = uuid::Uuid::new_v4().to_string();
            let (mut terminal, _t) = registered(root.path(), &registry, &session, "terminal");
            let (process, _) = process(terminal.id()).unwrap();
            let mut registration = Registration {
                version: 1,
                session: session.clone(),
                // Not where the registry fixture says the process runs (`/`).
                cwd: root.path().canonicalize().unwrap(),
                generation: uuid::Uuid::new_v4().to_string(),
                revision: process.birth.clone(),
                process,
                registry: registry.clone(),
                descriptor: root.path().join("local.json"),
            };
            let error = registration.refusal().unwrap_err();
            assert!(
                !error.is::<NotCurrentWriter>(),
                "a directory mismatch is not typed"
            );
            registration.cwd = PathBuf::from("/");
            assert!(registration.refusal().is_ok());
            registration.session = uuid::Uuid::new_v4().to_string();
            let error = registration.refusal().unwrap_err();
            assert!(
                !error.is::<NotCurrentWriter>(),
                "a process not listed yet is not typed"
            );
            terminal.kill().unwrap();
            terminal.wait().unwrap();
        });
    }

    /// Two terminals on one conversation leave one able to write: the first. The later one is
    /// held, and takes over once the first closes. A later process that isn't a terminal changes
    /// nothing: it may be a headless writer running a turn.
    #[cfg(unix)]
    #[test]
    fn of_two_terminals_on_a_conversation_the_first_stays_its_writer() {
        let root = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(root.path(), || {
            let registry = root.path().join("sessions");
            let session = uuid::Uuid::new_v4().to_string();
            let (mut first, _a) = registered(root.path(), &registry, &session, "first");
            // Start times are read in clock ticks: the second one starts visibly later.
            std::thread::sleep(std::time::Duration::from_millis(50));
            let (mut second, _b) = registered(root.path(), &registry, &session, "second");
            let as_terminal = |pid: u32, entrypoint: &str| {
                let path = registry.join(format!("{pid}.json"));
                let mut record: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                record["entrypoint"] = serde_json::json!(entrypoint);
                std::fs::write(&path, record.to_string()).unwrap();
            };
            let registration = |pid: u32| {
                let (process, _) = process(pid).unwrap();
                Registration {
                    version: 1,
                    session: session.clone(),
                    cwd: PathBuf::from("/"),
                    generation: uuid::Uuid::new_v4().to_string(),
                    revision: process.birth.clone(),
                    process,
                    registry: registry.clone(),
                    descriptor: root.path().join("local.json"),
                }
            };
            as_terminal(first.id(), "cli");
            as_terminal(second.id(), "sdk-cli");
            let error = registration(first.id()).refusal().unwrap_err();
            assert!(
                error.is::<NotCurrentWriter>(),
                "a headless process started later still holds this terminal"
            );

            as_terminal(second.id(), "cli");
            assert!(registration(first.id()).refusal().is_ok());
            let error = registration(second.id()).refusal().unwrap_err();
            assert!(error.is::<NotCurrentWriter>(), "the later terminal is held");

            first.kill().unwrap();
            first.wait().unwrap();
            assert!(registration(second.id()).refusal().is_ok());
            second.kill().unwrap();
            second.wait().unwrap();
        });
    }

    /// A terminal opened while the daemon's own headless writer still runs the conversation is
    /// its writer for registration only when the other process naming it is the writer the lease
    /// file records a handover waiting on (or a failed release of), by pid and start time. With no
    /// such lease, a writer recorded with another start time, a lease that no longer waits on it,
    /// or a third process, the terminal is no writer. If any second process made it no writer,
    /// its hold would end after 30 s with input going out beside the headless turn. The lease is
    /// read where the daemon writes it, although the hook asking runs as a plain command: read
    /// from the hook's own namespace, the terminal could never register.
    #[cfg(unix)]
    #[test]
    fn a_terminal_registers_beside_only_the_daemons_handing_over_writer() {
        let root = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(root.path(), || {
            let registry = root.path().join("sessions");
            let session = uuid::Uuid::new_v4().to_string();
            let (mut terminal, _t) = registered(root.path(), &registry, &session, "terminal");
            let (mut writer, _w) = registered(root.path(), &registry, &session, "writer");
            let (process, _) = process(terminal.id()).unwrap();
            let registration = Registration {
                version: 1,
                session: session.clone(),
                // Where the registry fixture says the process runs.
                cwd: PathBuf::from("/"),
                generation: uuid::Uuid::new_v4().to_string(),
                revision: process.birth.clone(),
                process,
                registry: registry.clone(),
                descriptor: root.path().join("local.json"),
            };
            // The lease file sits where the daemon keeps its state; the hook that registers runs
            // as a plain command, in the other namespace.
            crate::rc::select_local_authority();
            let path = crate::rc::rc_dir().unwrap().join("leases.json");
            crate::rc::select_plain_authority();
            let plain = crate::rc::rc_dir().unwrap().join("leases.json");
            assert_ne!(path, plain);
            let write = |path: &Path, kind: &str, state: &str, started_ms: i64| {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(
                    path,
                    serde_json::json!({"threads": {session.clone(): {
                        "epoch": 1, "holder": {"kind": kind, "session_id": "agit-h"},
                        "state": state, "reason": "terminal_opened", "since_ms": 0,
                        "writer": {"pid": writer.id(), "started_ms": started_ms, "plugin": false}}}})
                    .to_string(),
                )
                .unwrap();
            };
            let lease =
                |kind: &str, state: &str, started_ms: i64| write(&path, kind, state, started_ms);
            let found = || registration.live().ok().map(|live| live.pid());
            let typed = || {
                registration
                    .refusal()
                    .err()
                    .is_some_and(|error| error.is::<NotCurrentWriter>())
            };
            let started = started_ms(writer.id()).unwrap();
            assert_eq!(
                found(),
                None,
                "no lease: the second process is not handing over"
            );
            assert!(typed(), "another live process names the conversation");
            write(&plain, "headless", "releasing", started);
            assert_eq!(found(), None, "a lease outside the daemon's namespace");
            lease("headless", "releasing", started);
            assert_eq!(found(), Some(terminal.id()));
            assert!(registration.is_current());
            lease("headless", "release_failed", started);
            assert_eq!(found(), Some(terminal.id()));
            lease("headless", "releasing", started + 1);
            assert_eq!(
                found(),
                None,
                "another process with the recorded writer's pid"
            );
            lease("headless", "held", started);
            assert_eq!(found(), None, "a lease that no longer waits on the writer");
            lease("headless", "releasing", started);
            let (mut third, _o) = registered(root.path(), &registry, &session, "other");
            assert_eq!(found(), None, "a third process naming the conversation");
            assert!(!registration.is_current());
            assert!(typed());
            for child in [&mut terminal, &mut writer, &mut third] {
                child.kill().unwrap();
                child.wait().unwrap();
            }
        });
    }

    #[test]
    fn delayed_registration_cannot_restore_a_prior_session_generation() {
        let directory = tempfile::tempdir().unwrap();
        let (process, _) = process(std::process::id()).unwrap();
        let mut current = Registration {
            version: 1,
            session: uuid::Uuid::new_v4().to_string(),
            cwd: directory.path().to_owned(),
            generation: uuid::Uuid::new_v4().to_string(),
            revision: process.birth.clone(),
            process,
            registry: directory.path().to_owned(),
            descriptor: directory.path().join("local.json"),
        };
        let first = current.clone();
        publish(directory.path(), &first).unwrap();
        let mut rediscovered = first.clone();
        rediscovered.revision.seconds += 1;
        assert_eq!(publish(directory.path(), &rediscovered).unwrap(), first);
        rediscovered.descriptor = directory.path().join("cloud.json");
        assert_eq!(
            publish(directory.path(), &rediscovered).unwrap(),
            rediscovered
        );
        assert!(publish(directory.path(), &first).is_err());
        current.revision.seconds += 2;
        current.generation = uuid::Uuid::new_v4().to_string();
        publish(directory.path(), &current).unwrap();
        assert!(
            publish(directory.path(), &first).is_err(),
            "late registration must not restore an old generation even when the session ID returns"
        );
        let path = directory
            .path()
            .join(format!("{}.json", current.process.pid));
        assert_eq!(read_registration(&path).unwrap(), current);
        assert!(
            !current.is_registered(),
            "a process without a native writer registration cannot become controllable"
        );
    }
}
