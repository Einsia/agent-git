//! What became of a session branch's newest upload on this computer, and the line that says so
//! when a session agit launched ends.
//!
//! An automatic upload runs in the settlement of a turn, which for a session agit launched is a
//! detached process with no terminal: without a record, a failed upload of such a session is told
//! to nobody. The record names the branch tip the upload was for, whether the upload is running,
//! went through or failed, and why in a fixed class, never an error's text. It is local and never
//! sent, and each upload overwrites it rather than adding to a history. It lives under
//! `projects/uploads/` in agit's home, beside the projects' capture state, in files named by
//! digests, so no repository or branch name is in a path. Dropping a branch from uploads removes
//! its record, and unbinding a project removes every record of its repository.
//!
//! A turn whose settlement waits apart from the runtime for its reply has a marker there too, from
//! its Stop until the turn is on its branch: without it, the end of the session would count that
//! turn as never saved. The marker is named by a digest of the conversation and holds only the
//! settling process's id, so a settlement that crashed is told from one still running.

use super::project::UploadFailure;
use crate::domain::repo::Repo;
use crate::domain::store::Store;
use crate::domain::{link, refs};
use crate::infra::config;
use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Outcome {
    /// `attempt` tells this start from another upload of the same tip, which can overlap it.
    Started {
        tip: String,
        attempt: String,
    },
    Uploaded {
        tip: String,
    },
    Failed {
        tip: String,
        reason: UploadFailure,
    },
}

impl Outcome {
    fn tip(&self) -> &str {
        match self {
            Self::Started { tip, .. } | Self::Uploaded { tip } | Self::Failed { tip, .. } => tip,
        }
    }
}

fn digest(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))[..24].into()
}

fn uploads_dir() -> crate::Result<PathBuf> {
    Ok(config::agit_home()?.join("projects").join("uploads"))
}

fn repository_dir(repository: &str) -> crate::Result<PathBuf> {
    Ok(uploads_dir()?.join(digest(repository)))
}

/// The files of one branch's record.
struct Paths {
    record: PathBuf,
    /// Held shared by each running upload.
    running: PathBuf,
    /// Held exclusively while the record is read and replaced, so two uploads that finish
    /// together cannot both read a start and then overwrite each other.
    writing: PathBuf,
}

fn paths(repository: &str, branch: &str) -> crate::Result<Paths> {
    let base = repository_dir(repository)?.join(digest(branch));
    Ok(Paths {
        record: base.with_extension("json"),
        running: base.with_extension("lock"),
        writing: base.with_extension("write"),
    })
}

/// Runs `change` while holding the record's write lock. Without the lock it does not run.
fn under_write_lock<T>(writing: &Path, change: impl FnOnce() -> T) -> Option<T> {
    let file = config::state_file_options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(writing)
        .ok()?;
    fs2::FileExt::lock_exclusive(&file).ok()?;
    let changed = change();
    let _ = fs2::FileExt::unlock(&file);
    Some(changed)
}

fn read(record: &Path) -> Option<Outcome> {
    serde_json::from_slice(&std::fs::read(record).ok()?).ok()
}

fn write(record: &Path, outcome: &Outcome) -> crate::Result<()> {
    use std::io::Write as _;
    let directory = record.parent().context("upload record has no directory")?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(&serde_json::to_vec(outcome)?)?;
    file.persist(record).map_err(|error| error.error)?;
    Ok(())
}

/// Whether some process holds the lock at `path`; looking takes it for an instant only.
fn held(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            let _ = fs2::FileExt::unlock(&file);
            false
        }
        Err(error) => crate::infra::local_state::is_contended(&error),
    }
}

/// An upload whose start is recorded. Until it is finished or dropped it holds its lock shared,
/// which tells a running upload apart from one whose process ended before saying how it went.
pub(crate) struct Upload {
    paths: Paths,
    started: Outcome,
    running: std::fs::File,
}

/// Records that an upload of `repository@branch` at `tip` starts. Recording never holds an upload
/// back: `None` means it runs unrecorded.
pub(crate) fn start(repository: &str, branch: &str, tip: &str) -> Option<Upload> {
    start_at(paths(repository, branch).ok()?, tip)
}

fn start_at(paths: Paths, tip: &str) -> Option<Upload> {
    config::create_state_dir(paths.record.parent()?).ok()?;
    let running = config::state_file_options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&paths.running)
        .ok()?;
    fs2::FileExt::lock_shared(&running).ok()?;
    let started = Outcome::Started {
        tip: tip.to_owned(),
        attempt: uuid::Uuid::new_v4().to_string(),
    };
    under_write_lock(&paths.writing, || write(&paths.record, &started))?.ok()?;
    Some(Upload {
        paths,
        started,
        running,
    })
}

impl Upload {
    /// Records how the upload went, over its own start only: a record another upload started
    /// since, of this tip or another, or one removed because the branch was dropped or its project
    /// unbound, stays as it is.
    pub(crate) fn finish(self, result: Result<(), UploadFailure>) {
        let Self {
            paths,
            started,
            running,
        } = self;
        let tip = started.tip().to_owned();
        under_write_lock(&paths.writing, || {
            if read(&paths.record).as_ref() == Some(&started) {
                let outcome = match result {
                    Ok(()) => Outcome::Uploaded { tip },
                    Err(reason) => Outcome::Failed { tip, reason },
                };
                let _ = write(&paths.record, &outcome);
            }
        });
        let _ = fs2::FileExt::unlock(&running);
    }
}

/// Removes the record of `repository@branch`, or with no branch every record of `repository`.
/// Each record goes under its write lock, as a finishing upload replaces it, so an upload that
/// read its own start before the removal cannot write the record back. The lock files stay: they
/// hold nothing, and removing one that is held would let the next upload lock a new file while
/// the old holder still writes.
pub(crate) fn forget(repository: &str, branch: Option<&str>) -> crate::Result<()> {
    let records: Vec<Paths> = match branch {
        Some(branch) => vec![paths(repository, branch)?],
        None => match std::fs::read_dir(repository_dir(repository)?) {
            Ok(entries) => entries
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<Vec<_>>>()?
                .into_iter()
                .filter(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "json")
                })
                .map(|record| Paths {
                    running: record.with_extension("lock"),
                    writing: record.with_extension("write"),
                    record,
                })
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        },
    };
    for paths in &records {
        let removed = under_write_lock(&paths.writing, || std::fs::remove_file(&paths.record))
            .unwrap_or_else(|| std::fs::remove_file(&paths.record));
        match removed {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot remove {}", paths.record.display()));
            }
        }
    }
    Ok(())
}

/// Set on a detached settlement whose Stop hook wrote a marker for it.
pub(crate) const SETTLING_ENV: &str = "AGIT_SETTLING";

/// Who settles a turn that waits apart from the runtime for its reply.
#[derive(Serialize, Deserialize)]
struct Settling {
    pid: u32,
}

fn settling_path(runtime: &str, session: &str) -> crate::Result<PathBuf> {
    Ok(uploads_dir()?
        .join("settling")
        .join(format!("{}.json", digest(&format!("{runtime}\0{session}")))))
}

/// Where the Stop hook of `runtime`'s conversation `session` marks a detached settlement, when the
/// runtime is one agit launched: only such a runtime's end reports on its turns.
pub(crate) fn settling_marker(runtime: &str, session: &str) -> Option<PathBuf> {
    std::env::var_os("AGIT_SESSION")?;
    let path = settling_path(runtime, session).ok()?;
    config::create_state_dir(path.parent()?).ok()?;
    Some(path)
}

/// Marks the process `pid` as settling a turn. The hook writes it before the settlement can read
/// its payload, so the settlement never removes a marker before it is written, and before the
/// hook returns, so the runtime's end never reads the conversation before it is marked.
pub(crate) fn mark_settling(marker: &Path, pid: u32) -> bool {
    serde_json::to_vec(&Settling { pid })
        .ok()
        .is_some_and(|bytes| {
            tempfile::NamedTempFile::new_in(marker.parent().unwrap_or(marker))
                .and_then(|mut file| {
                    use std::io::Write as _;
                    file.write_all(&bytes)?;
                    file.persist(marker).map_err(|error| error.error)
                })
                .is_ok()
        })
}

pub(crate) fn unmark(marker: &Path) {
    let _ = std::fs::remove_file(marker);
}

static SETTLING: Mutex<Option<PathBuf>> = Mutex::new(None);

/// In a detached settlement its hook marked: the marker to remove once the turn is on its branch.
pub(crate) fn settling_from_environment(runtime: &str, session: &str) {
    if std::env::var_os(SETTLING_ENV).is_none() {
        return;
    }
    if let Ok(path) = settling_path(runtime, session) {
        *SETTLING.lock().unwrap_or_else(PoisonError::into_inner) = Some(path);
    }
}

/// The turn this process was settling is on its branch, or its settlement ended without it. A
/// marker a later Stop wrote for another settlement stays.
pub(crate) fn landed() {
    let Some(path) = SETTLING
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
    else {
        return;
    };
    if read_settling(&path).is_some_and(|settling| settling.pid == std::process::id()) {
        unmark(&path);
    }
}

fn read_settling(path: &Path) -> Option<Settling> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Whether a detached settlement still saves a turn of `runtime`'s conversation `session`. A
/// settlement that ended without removing its marker, as one that crashed does, is told by its
/// process being gone; the conversation's runtime has ended, so no new settlement of it starts,
/// and the marker goes.
fn settling(runtime: &str, session: &str) -> bool {
    let Ok(path) = settling_path(runtime, session) else {
        return false;
    };
    if !path.exists() {
        return false;
    }
    if read_settling(&path).is_some_and(|settling| alive(settling.pid)) {
        return true;
    }
    unmark(&path);
    false
}

#[cfg(unix)]
pub(crate) fn alive(pid: u32) -> bool {
    libc::pid_t::try_from(pid).is_ok_and(|pid| pid > 0 && unsafe { libc::kill(pid, 0) } == 0)
}

#[cfg(windows)]
pub(crate) fn alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if process.is_null() {
        return false;
    }
    let state = unsafe { WaitForSingleObject(process, 0) };
    unsafe { CloseHandle(process) };
    state == WAIT_TIMEOUT
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn alive(_: u32) -> bool {
    false
}

/// A runtime agit launched for `repository@branch`, with the branch tip at its start, so that
/// its end can tell which turns it saved.
pub(crate) struct Launch {
    repository: String,
    owner: String,
    name: String,
    branch: String,
    directory: PathBuf,
    tip: Option<String>,
}

pub(crate) fn launching(repository: &str, branch: &str) -> Option<Launch> {
    let (owner, name) = super::parse_slug(repository).ok()?;
    let directory = config::repo_dir(&owner, &name).ok()?;
    let tip = super::auto_push::branch_tip(&directory, branch);
    Some(Launch {
        repository: repository.to_owned(),
        owner,
        name,
        branch: branch.to_owned(),
        directory,
        tip,
    })
}

/// What became of the newest saved turn's upload.
enum Status {
    Uploaded,
    Uploading,
    Failed(&'static str),
    NotUploaded(&'static str),
    /// Nothing on this computer says: its settlement has not decided yet, decided it elsewhere,
    /// or the process uploading it ended without saying how it went.
    Unknown,
}

impl Launch {
    /// One line, read from local state alone, on what became of the turns this launch saved. It
    /// names the repository, which the person chose and which the session's start already named,
    /// and never a branch, a path or anything of the conversation. `None` when the launch saved
    /// no turn and none is being saved.
    pub(crate) fn ended(&self) -> Option<String> {
        let repo = Repo::open(&self.directory)?;
        let saving = self.saving();
        let now = super::auto_push::branch_tip(&self.directory, &self.branch);
        let turns =
            |tip: Option<&str>| tip.map_or(Some(0), |tip| refs::count_turns(&repo, tip).ok());
        let saved = if now == self.tip {
            0
        } else {
            turns(now.as_deref())?.saturating_sub(turns(self.tip.as_deref())?)
        };
        if saved == 0 {
            return saving
                .then(|| "agit: the last turn of this conversation is still being saved.".into());
        }
        let (turns, them) = if saved == 1 {
            ("1 turn".to_owned(), "it")
        } else {
            (format!("{saved} turns"), "them")
        };
        let repository = &self.repository;
        let line = match self.status(&repo, now.as_deref()?) {
            Status::Uploaded => {
                format!(
                    "agit: saved {turns} of this conversation and uploaded {them} to {repository}"
                )
            }
            Status::Uploading => format!(
                "agit: saved {turns} of this conversation; uploading {them} to {repository} in the background"
            ),
            Status::Failed(reason) => format!(
                "agit: saved {turns} of this conversation on this computer; the upload to {repository} failed: {reason}"
            ),
            Status::NotUploaded(reason) => format!(
                "agit: saved {turns} of this conversation on this computer; not uploaded: {reason}"
            ),
            Status::Unknown => format!("agit: saved {turns} of this conversation"),
        };
        Some(if saving {
            format!("{line}; the last turn is still being saved.")
        } else {
            format!("{line}.")
        })
    }

    /// Whether a turn of the branch is being written now, waits apart from the runtime for its
    /// reply before it is, or waits in the queue of the project that saves its folder.
    fn saving(&self) -> bool {
        let Ok(root) = config::store_root() else {
            return false;
        };
        let store = Store::at(root);
        link::branch_busy(&store, &self.repository, &self.branch)
            || link::active_for_branch(&store, &self.owner, &self.name, &self.branch)
                .iter()
                .any(|claim| {
                    settling(&claim.source, &claim.session_id)
                        || claim.cwd.as_deref().is_some_and(|cwd| {
                            super::project::capture_pending(
                                &claim.source,
                                &claim.session_id,
                                Path::new(cwd),
                            )
                        })
                })
    }

    /// The record speaks only for the tip it was written for; an older one says nothing about the
    /// turns saved after it.
    fn status(&self, repo: &Repo, tip: &str) -> Status {
        if let Ok(Paths {
            record, running, ..
        }) = paths(&self.repository, &self.branch)
            && let Some(outcome) = read(&record).filter(|outcome| outcome.tip() == tip)
        {
            return match outcome {
                Outcome::Uploaded { .. } => Status::Uploaded,
                Outcome::Failed { reason, .. } => Status::Failed(reason.describe()),
                Outcome::Started { .. } if held(&running) => Status::Uploading,
                Outcome::Started { .. } => Status::Unknown,
            };
        }
        match left_out(repo, &self.repository, &self.branch) {
            Some(reason) => Status::NotUploaded(reason),
            None => Status::Unknown,
        }
    }
}

/// Why settlement leaves `repository@branch` out of automatic uploads, judged as it judges it.
fn left_out(repo: &Repo, repository: &str, branch: &str) -> Option<&'static str> {
    if !repo.auto_push_enabled().ok()? {
        return Some("automatic upload is off for this repository");
    }
    super::project::auto_push_blocker(repo, repository, branch)
        .ok()?
        .map(super::project::Blocker::describe)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(directory: &Path) -> Paths {
        Paths {
            record: directory.join("record.json"),
            running: directory.join("record.lock"),
            writing: directory.join("record.write"),
        }
    }

    /// Uploads of one branch overlap: a settlement's upload and a save's, or two turns' uploads
    /// whose tips differ. Each finishes only over its own start, so the record ends with the outcome
    /// of the upload that started last. One that matched on the tip alone would let an earlier
    /// failure of the same tip stand after a later success, and one that read and replaced the
    /// record unguarded could overwrite a newer start.
    #[test]
    fn overlapping_uploads_leave_the_outcome_of_the_last_one_started() {
        let directory = tempfile::tempdir().unwrap();
        let record = || read(&at(directory.path()).record);

        let failing = start_at(at(directory.path()), "tip-1").unwrap();
        let succeeding = start_at(at(directory.path()), "tip-1").unwrap();
        failing.finish(Err(UploadFailure::Hub));
        succeeding.finish(Ok(()));
        assert_eq!(
            record(),
            Some(Outcome::Uploaded {
                tip: "tip-1".into()
            })
        );

        let older = start_at(at(directory.path()), "tip-2").unwrap();
        let newer = start_at(at(directory.path()), "tip-3").unwrap();
        older.finish(Err(UploadFailure::Hub));
        assert!(
            matches!(record(), Some(Outcome::Started { tip, .. }) if tip == "tip-3"),
            "an older upload does not overwrite a newer start"
        );
        newer.finish(Err(UploadFailure::TooLarge));
        assert_eq!(
            record(),
            Some(Outcome::Failed {
                tip: "tip-3".into(),
                reason: UploadFailure::TooLarge,
            })
        );
    }

    /// Dropping a branch or unbinding its project removes its record for good: an upload still
    /// running then finishes without writing it back, and the lock files it holds are left where
    /// they are, so a later upload locks the same file the running one holds.
    #[test]
    fn a_forgotten_record_is_not_written_back_by_an_upload_still_running() {
        const CHILD: &str = "AGIT_UPLOAD_OUTCOME_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // The record lives under the agit home, which is read from the environment; a child
            // with its own home keeps the other tests' homes out of it.
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "commands::upload_outcome::tests::a_forgotten_record_is_not_written_back_by_an_upload_still_running",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("AGIT_HOME", home.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        {
            let running = start("alice/app", "work", "tip-1").unwrap();
            let files = paths("alice/app", "work").unwrap();
            forget("alice/app", Some("work")).unwrap();
            assert!(!files.record.exists());
            assert!(files.running.exists() && files.writing.exists());
            running.finish(Ok(()));
            assert!(
                !files.record.exists(),
                "a running upload wrote a forgotten record back"
            );

            let running = start("alice/app", "other", "tip-2").unwrap();
            forget("alice/app", None).unwrap();
            running.finish(Err(UploadFailure::Hub));
            assert!(!paths("alice/app", "other").unwrap().record.exists());

            // A finish that read its own start holds the write lock until it has replaced the
            // record. A removal that does not wait for that lock returns while it is held, and the
            // replacement then writes the forgotten record back.
            let _running = start("alice/app", "late", "tip-3").unwrap();
            let files = paths("alice/app", "late").unwrap();
            let finishing = config::state_file_options()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&files.writing)
                .unwrap();
            fs2::FileExt::lock_exclusive(&finishing).unwrap();
            let (done, forgotten) = std::sync::mpsc::channel();
            let forgetting = std::thread::spawn(move || {
                forget("alice/app", Some("late")).unwrap();
                done.send(()).unwrap();
            });
            assert!(
                forgotten
                    .recv_timeout(std::time::Duration::from_millis(200))
                    .is_err(),
                "a record was removed while a finishing upload held its write lock"
            );
            write(
                &files.record,
                &Outcome::Uploaded {
                    tip: "tip-3".into(),
                },
            )
            .unwrap();
            fs2::FileExt::unlock(&finishing).unwrap();
            forgetting.join().unwrap();
            assert!(!files.record.exists());
        }
    }
}
