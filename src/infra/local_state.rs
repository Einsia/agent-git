//! Failures of agit's own state under `AGIT_HOME`, kept apart from Hub and network failures.
//!
//! Every lock file agit keeps is an advisory OS lock: the kernel releases it when the holding
//! process exits, so a lock file left on disk never blocks anything by itself. Deleting one while
//! its holder still runs lets the next process lock a fresh file of the same name, and two
//! writers then change the same state at once.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a command waits for another agit process to release one of its state locks. Holders
/// keep these locks only for local reads and writes, so a lock that stays held this long belongs
/// to a process that is stuck or still waiting for input.
pub const LOCK_WAIT: Duration = Duration::from_secs(30);

const LOCK_POLL: Duration = Duration::from_millis(50);

/// Set, a wait for a state lock that lasts longer than [`DIAGNOSED_WAIT`] writes one line to
/// stderr when it ends: which lock, its path, this process's id and how long it waited. No state
/// content. It is for finding a hidden holder in a test run, and off otherwise.
pub const LOCK_DIAGNOSTICS_ENV: &str = "AGIT_DIAGNOSE_LOCK_WAITS";
const DIAGNOSED_WAIT: Duration = Duration::from_secs(1);

/// Says a long wait for `what` at `path` ended, when [`LOCK_DIAGNOSTICS_ENV`] is set.
fn diagnose_wait(what: &str, path: &Path, started: Instant, outcome: &str) {
    let waited = started.elapsed();
    if waited >= DIAGNOSED_WAIT && std::env::var_os(LOCK_DIAGNOSTICS_ENV).is_some() {
        eprintln!(
            "agit lock wait: {what} ({}) process {} waited {:.1}s, {outcome}",
            path.display(),
            std::process::id(),
            waited.as_secs_f64()
        );
    }
}

/// How long a command waits without a word for a lock whose holder may be waiting for input
/// before it says what it is waiting for (see [`lock_patiently`]).
pub const LOCK_NOTICE: Duration = Duration::from_secs(2);

/// Local state that agit cannot use, as opposed to a Hub that cannot be reached.
#[derive(Debug)]
pub enum LocalStateError {
    /// The operating system refused a write under `AGIT_HOME`.
    NotWritable {
        home: Option<PathBuf>,
        path: PathBuf,
        source: std::io::Error,
    },
    /// Another process kept an agit lock for the whole wait.
    Contended { what: &'static str, path: PathBuf },
}

/// How [`LocalStateError::Contended`] begins, for a parent that reads a child's error text.
pub const CONTENDED: &str = "another agit process is holding";
/// What [`LocalStateError::NotWritable`] says before the path it couldn't write, for a parent that
/// reads a child's error text. It goes on to the path, so it isn't read from a refusal that only
/// says something is not writable, such as a namespace on the Hub.
pub const NOT_WRITABLE: &str = "is not writable: cannot write";

impl std::fmt::Display for LocalStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotWritable { home, path, source } => {
                match home {
                    Some(home) => write!(f, "AGIT_HOME ({}) {NOT_WRITABLE}", home.display())?,
                    None => write!(f, "agit's local state {NOT_WRITABLE}")?,
                }
                write!(f, " {}: {source}", path.display())
            }
            Self::Contended { what, path } => write!(
                f,
                "{CONTENDED} {what} ({}) and did not release it within {} seconds",
                path.display(),
                LOCK_WAIT.as_secs()
            ),
        }
    }
}

impl std::error::Error for LocalStateError {}

impl LocalStateError {
    /// The next steps a person or agent can take, one per line.
    pub fn hints(&self) -> Vec<String> {
        match self {
            Self::NotWritable { home, .. } => {
                let target = home
                    .as_ref()
                    .map(|home| home.display().to_string())
                    .unwrap_or_else(|| "AGIT_HOME".into());
                vec![
                    "this commonly happens when agit runs inside an agent sandbox that only allows writes to the workspace".into(),
                    format!("allow writes to {target}, or run this agit command outside the sandbox"),
                ]
            }
            Self::Contended { path, .. } => {
                let mut hints = vec![
                    "agit lock files are advisory OS locks that are released when the holding process exits; do not delete them".into(),
                    "wait for the other agit command to finish, or stop it, then retry".into(),
                ];
                if cfg!(unix) {
                    hints.push(format!(
                        "`lsof {}` shows which process holds it",
                        path.display()
                    ));
                }
                hints
            }
        }
    }
}

/// The hints of the first local state failure in an error chain.
pub fn hints(error: &anyhow::Error) -> Vec<String> {
    find(error).map(LocalStateError::hints).unwrap_or_default()
}

/// The first local state failure in an error chain.
pub fn find(error: &anyhow::Error) -> Option<&LocalStateError> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<LocalStateError>())
}

/// Classify a filesystem failure at `path`. A refused write names `AGIT_HOME`; any other failure
/// keeps its own wording, with `action` saying what was attempted.
pub fn io_failure(action: &str, path: &Path, error: std::io::Error) -> anyhow::Error {
    if matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
    ) {
        anyhow::Error::new(LocalStateError::NotWritable {
            home: super::config::agit_home().ok(),
            path: path.to_owned(),
            source: error,
        })
    } else {
        anyhow::Error::new(error).context(format!("{action} {}", path.display()))
    }
}

/// Whether a failed `try_lock` means another process holds the lock.
pub fn is_contended(error: &std::io::Error) -> bool {
    error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

/// Take an exclusive lock on `file`, waiting at most [`LOCK_WAIT`] for another process to
/// release it. A lock held past the wait is reported as [`LocalStateError::Contended`] instead of
/// blocking the command without bound.
pub fn lock_exclusive(file: &std::fs::File, path: &Path, what: &'static str) -> crate::Result<()> {
    let started = Instant::now();
    let deadline = started + LOCK_WAIT;
    loop {
        match fs2::FileExt::try_lock_exclusive(file) {
            Ok(()) => {
                diagnose_wait(what, path, started, "acquired");
                return Ok(());
            }
            Err(error) if is_contended(&error) => {
                if Instant::now() >= deadline {
                    diagnose_wait(what, path, started, "gave up");
                    return Err(LocalStateError::Contended {
                        what,
                        path: path.to_owned(),
                    }
                    .into());
                }
                std::thread::sleep(LOCK_POLL);
            }
            Err(error) => return Err(io_failure("cannot lock", path, error)),
        }
    }
}

/// Take a lock whose holder may keep it while it waits for a person, such as a session link or
/// branch lock held across an import's questions. The wait has no bound, because failing would
/// turn a slow answer into a failed settlement; a wait that outlasts [`LOCK_NOTICE`] says once
/// which lock it waits for and that the file must not be deleted, instead of hanging silently.
pub fn lock_patiently(
    file: &std::fs::File,
    path: &Path,
    what: &str,
    exclusive: bool,
) -> crate::Result<()> {
    let attempt = || {
        if exclusive {
            fs2::FileExt::try_lock_exclusive(file)
        } else {
            fs2::FileExt::try_lock_shared(file)
        }
    };
    let started = Instant::now();
    let notice = started + LOCK_NOTICE;
    loop {
        match attempt() {
            Ok(()) => {
                diagnose_wait(what, path, started, "acquired");
                return Ok(());
            }
            Err(error) if is_contended(&error) && Instant::now() < notice => {
                std::thread::sleep(LOCK_POLL);
            }
            Err(error) if is_contended(&error) => break,
            Err(error) => return Err(io_failure("cannot lock", path, error)),
        }
    }
    let holder = if cfg!(unix) {
        format!("; `lsof {}` shows the holder", path.display())
    } else {
        String::new()
    };
    crate::warn(&format!(
        "waiting for another agit process to release {what} ({}); it is released when that process exits, so do not delete it{holder}",
        path.display()
    ));
    let locked = if exclusive {
        fs2::FileExt::lock_exclusive(file)
    } else {
        fs2::FileExt::lock_shared(file)
    };
    diagnose_wait(
        what,
        path,
        started,
        if locked.is_ok() { "acquired" } else { "failed" },
    );
    locked.map_err(|error| io_failure("cannot lock", path, error))
}

/// A lock taken without waiting, released when the guard drops.
///
/// agit opens every file close-on-exec, and spawning a child returns only once the child has
/// exec'd, so a holder's own children never keep its lock. The one other copy of the open file
/// description is in a child that some thread has forked and that has not exec'd yet. `flock`
/// belongs to the description, so a guard that only closed its descriptor would leave the lock
/// held by that copy, and the next taker would report a finished holder as busy. Dropping
/// therefore unlocks explicitly, which releases the lock however many copies remain open.
pub struct HeldLock(std::fs::File);

impl HeldLock {
    /// Take an exclusive lock on `file`; contention reads as `busy`.
    pub fn try_exclusive(
        file: std::fs::File,
        path: &Path,
        busy: &'static str,
    ) -> crate::Result<Self> {
        Self::take(file, path, busy, fs2::FileExt::try_lock_exclusive)
    }

    /// Take a shared lock on `file`; contention reads as `busy`.
    pub fn try_shared(file: std::fs::File, path: &Path, busy: &'static str) -> crate::Result<Self> {
        Self::take(file, path, busy, fs2::FileExt::try_lock_shared)
    }

    /// A second descriptor for the same open file description, as a forked child holds one.
    #[cfg(test)]
    pub(crate) fn forked_copy(&self) -> std::fs::File {
        self.0.try_clone().unwrap()
    }

    /// Only contention means another holder: any other failure keeps its own message, since
    /// retrying would not help.
    fn take(
        file: std::fs::File,
        path: &Path,
        busy: &'static str,
        attempt: fn(&std::fs::File) -> std::io::Result<()>,
    ) -> crate::Result<Self> {
        match attempt(&file) {
            Ok(()) => Ok(Self(file)),
            Err(error) if is_contended(&error) => Err(anyhow::Error::new(error).context(busy)),
            Err(error) => Err(io_failure("cannot lock", path, error)),
        }
    }
}

impl Drop for HeldLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refused write must read as a local problem naming the home, and a held lock must forbid
    /// deleting it; a classifier that folds both into one generic I/O message loses both.
    #[test]
    fn refused_writes_name_the_home_and_contention_forbids_deleting_the_lock() {
        let refused = io_failure(
            "cannot open",
            Path::new("/state/credentials.lock"),
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        );
        let local = find(&refused).expect("a refused write is a local state failure");
        assert!(matches!(local, LocalStateError::NotWritable { .. }));
        assert!(refused.to_string().contains("is not writable"), "{refused}");
        assert!(hints(&refused).iter().any(|hint| hint.contains("sandbox")));

        let other = io_failure(
            "cannot open",
            Path::new("/state/credentials.lock"),
            std::io::Error::from(std::io::ErrorKind::NotFound),
        );
        assert!(find(&other).is_none());
        assert!(
            other.to_string().contains("/state/credentials.lock"),
            "{other}"
        );

        let held = anyhow::Error::new(LocalStateError::Contended {
            what: "the credential lock",
            path: "/state/credentials.lock".into(),
        });
        assert!(held.to_string().contains("another agit process"), "{held}");
        assert!(
            hints(&held)
                .iter()
                .any(|hint| hint.contains("do not delete"))
        );
    }

    /// Dropping a [`HeldLock`] must release the lock while a copy of its open file description
    /// stays open, as it does in a child forked and not yet exec'd. A guard that only closes its
    /// descriptor leaves the lock with the copy, and the next taker sees a busy holder that has
    /// already finished.
    #[test]
    fn a_dropped_lock_is_free_while_a_forked_copy_of_its_descriptor_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let open = || {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .unwrap()
        };
        let held = HeldLock::try_exclusive(open(), &path, "busy; retry").unwrap();
        let forked = held.forked_copy();
        let contended = HeldLock::try_shared(open(), &path, "busy; retry")
            .err()
            .expect("an exclusive lock refuses a shared taker");
        assert_eq!(contended.to_string(), "busy; retry");

        drop(held);
        HeldLock::try_shared(open(), &path, "busy; retry").unwrap();
        drop(forked);
    }
}
