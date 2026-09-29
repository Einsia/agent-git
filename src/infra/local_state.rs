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

impl std::fmt::Display for LocalStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotWritable { home, path, source } => {
                match home {
                    Some(home) => write!(f, "AGIT_HOME ({}) is not writable", home.display())?,
                    None => f.write_str("agit's local state is not writable")?,
                }
                write!(f, ": cannot write {}: {source}", path.display())
            }
            Self::Contended { what, path } => write!(
                f,
                "another agit process is holding {what} ({}) and did not release it within {} seconds",
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
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match fs2::FileExt::try_lock_exclusive(file) {
            Ok(()) => return Ok(()),
            Err(error) if is_contended(&error) => {
                if Instant::now() >= deadline {
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
    let notice = Instant::now() + LOCK_NOTICE;
    loop {
        match attempt() {
            Ok(()) => return Ok(()),
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
    if exclusive {
        fs2::FileExt::lock_exclusive(file)
    } else {
        fs2::FileExt::lock_shared(file)
    }
    .map_err(|error| io_failure("cannot lock", path, error))
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
}
