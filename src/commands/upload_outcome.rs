//! What became of a session branch's newest automatic upload on this computer, and the lines that
//! say so: at the end of a turn, when a session agit launched ends, and in `agit status`.
//!
//! An automatic upload runs in the settlement of a turn, which for a session agit launched is a
//! detached process with no terminal: without a record, a failed upload of such a session is told
//! to nobody. Every automatic upload runs through [`recorded`] or [`recorded_async`], which record
//! the branch tip the upload is for, whether the upload is running, went through or failed, and
//! why in a fixed class, never an error's text. An upload whose start can't be recorded doesn't
//! run, and a note says so instead. The record is local and never sent, and each upload overwrites
//! it rather than adding to a history. It lives under `projects/uploads/` in agit's home, beside
//! the projects' capture state, in files named by digests, so no repository or branch name is in a
//! path. Dropping a branch from uploads removes its record, and unbinding a project removes every
//! record of its repository.
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
    /// `attempt` is the start this failure finished, so the turn-end line tells it once.
    Failed {
        tip: String,
        reason: UploadFailure,
        #[serde(default)]
        attempt: String,
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

/// Where the notes of a repository's uploads that didn't run are kept: in its checkout's Git
/// directory, which the turn an upload is for was just written to, so a note can be written when
/// the record can't.
fn skipped_dir(repository: &str) -> crate::Result<PathBuf> {
    let (owner, name) = super::parse_slug(repository)?;
    Ok(config::repo_dir(&owner, &name)?
        .join(".git")
        .join("agit-upload-skipped"))
}

/// The files of one branch's record.
struct Paths {
    record: PathBuf,
    /// Held shared by each running upload.
    running: PathBuf,
    /// Held exclusively while the record is read and replaced, so two uploads that finish
    /// together cannot both read a start and then overwrite each other.
    writing: PathBuf,
    /// The attempt the turn-end line last told.
    told: PathBuf,
}

impl Paths {
    fn of(record: PathBuf) -> Self {
        Self {
            running: record.with_extension("lock"),
            writing: record.with_extension("write"),
            told: record.with_extension("told"),
            record,
        }
    }
}

fn paths(repository: &str, branch: &str) -> crate::Result<Paths> {
    Ok(Paths::of(
        repository_dir(repository)?
            .join(digest(branch))
            .with_extension("json"),
    ))
}

/// Where one branch's upload record is kept, with the note that says an upload of it didn't run.
pub(crate) struct Place {
    paths: Paths,
    skipped: PathBuf,
    /// `<owner/repo>@<branch>`, which a recorded privacy wait names.
    target: String,
}

impl Place {
    pub(crate) fn of(repository: &str, branch: &str) -> crate::Result<Self> {
        Ok(Self {
            paths: paths(repository, branch)?,
            skipped: skipped_dir(repository)?
                .join(digest(branch))
                .with_extension("json"),
            target: format!("{repository}@{branch}"),
        })
    }

    /// A place outside agit's home, for tests that must not write to it.
    #[cfg(test)]
    pub(crate) fn at(records: &Path, skipped: &Path) -> Self {
        Self {
            paths: Paths::of(records.join("record.json")),
            skipped: skipped.join("skipped.json"),
            target: "test/repository@branch".to_owned(),
        }
    }

    /// The state the record holds, as it names it.
    #[cfg(test)]
    pub(crate) fn state(&self) -> Option<String> {
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&self.paths.record).ok()?).ok()?;
        record["state"].as_str().map(str::to_owned)
    }
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

/// Replaces the file at `path` whole, whatever it held before.
fn replace(path: &Path, bytes: &[u8]) -> crate::Result<()> {
    use std::io::Write as _;
    let directory = path.parent().context("upload record has no directory")?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(bytes)?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn write(record: &Path, outcome: &Outcome) -> crate::Result<()> {
    replace(record, &serde_json::to_vec(outcome)?)
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

/// What a push that didn't go through reported, for [`recorded`] to class.
pub(crate) struct Refusal {
    exit_code: Option<i32>,
    message: String,
    /// The conversation whose capture may hold a privacy wait for this upload.
    session: Option<String>,
}

impl Refusal {
    /// A push child's exit status and the error it reported.
    pub(crate) fn child(exit_code: Option<i32>, message: String) -> Self {
        Self {
            exit_code,
            message,
            session: None,
        }
    }

    /// The refusal of an upload of `session`'s capture, whose privacy wait it may be.
    pub(crate) fn of_session(mut self, session: &str) -> Self {
        self.session = Some(session.to_owned());
        self
    }

    /// What a finished push child reported: its own error lines, from its JSON envelope when it
    /// printed one and from its error output otherwise.
    pub(crate) fn of(output: &std::process::Output) -> Self {
        let message = super::auto_push::failure_reason(&output.stdout).map_or_else(
            || String::from_utf8_lossy(&output.stderr).into_owned(),
            |(reason, _)| reason,
        );
        Self::child(output.status.code(), message)
    }

    /// A push that ended without a report of its own: it could not start, it was cancelled, or
    /// what it reported could not be confirmed.
    pub(crate) fn unexplained() -> Self {
        Self::child(None, String::new())
    }

    /// The one place a recorded upload's failure is classed. A policy exit counts as a privacy
    /// wait only when a wait is recorded for the conversation or for `target`; otherwise the turn
    /// would tell of a wait that `agit status` can't show.
    fn class(&self, target: &str) -> UploadFailure {
        UploadFailure::reported(self.exit_code, &self.message)
            .waiting_on(self.session.as_deref(), Some(target))
    }
}

/// Why an upload that can't be recorded doesn't run, as the end of a launched session says it.
pub(crate) const UNRECORDED: &str = "this computer couldn't record the upload's outcome";

/// An automatic upload that didn't run, because its start couldn't be recorded: run unrecorded,
/// a failure of it would be told to nobody. A note says so instead, and the next upload whose start
/// can be recorded runs as usual.
#[derive(Debug)]
pub(crate) struct Unrecorded(anyhow::Error);

impl std::fmt::Display for Unrecorded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{UNRECORDED}: {:#}", self.0)
    }
}

impl std::error::Error for Unrecorded {}

/// Runs the automatic upload `run` of the branch `place` keeps the record of, at `tip`, and
/// records how it went: `judge` says what `run` reported when it didn't go through. Without a
/// recorded start it does not run.
pub(crate) fn recorded<T>(
    place: crate::Result<Place>,
    tip: &str,
    run: impl FnOnce() -> T,
    judge: impl FnOnce(&T) -> Option<Refusal>,
) -> Result<T, Unrecorded> {
    let place = place.map_err(Unrecorded)?;
    let target = place.target.clone();
    let upload = place.start(tip).map_err(Unrecorded)?;
    let ran = run();
    upload.finish(judge(&ran).map_or(Ok(()), |refusal| Err(refusal.class(&target))));
    Ok(ran)
}

/// [`recorded`] for an upload that runs on an async runtime, with the record's reads and writes
/// on its blocking pool.
pub(crate) async fn recorded_async<T>(
    place: crate::Result<Place>,
    tip: String,
    run: impl std::future::Future<Output = T>,
    judge: impl FnOnce(&T) -> Option<Refusal>,
) -> Result<T, Unrecorded> {
    let place = place.map_err(Unrecorded)?;
    let target = place.target.clone();
    let upload = tokio::task::spawn_blocking(move || place.start(&tip))
        .await
        .map_err(|error| Unrecorded(error.into()))?
        .map_err(Unrecorded)?;
    let ran = run.await;
    let result = judge(&ran).map_or(Ok(()), |refusal| Err(refusal.class(&target)));
    let _ = tokio::task::spawn_blocking(move || upload.finish(result)).await;
    Ok(ran)
}

/// The note of an upload that didn't run because its start couldn't be recorded.
#[derive(Serialize, Deserialize)]
struct Skipped {
    tip: String,
    attempt: String,
}

/// An upload whose start is recorded. Until it is finished or dropped it holds its lock shared,
/// which tells a running upload apart from one whose process ended before saying how it went.
struct Upload {
    paths: Paths,
    tip: String,
    attempt: String,
    running: std::fs::File,
}

impl Place {
    /// Records that an upload at `tip` starts, over whatever the record file held, readable or
    /// not. A start that can't be recorded leaves the note instead; one that can removes it.
    fn start(self, tip: &str) -> crate::Result<Upload> {
        let lock = self.note_lock();
        match self.paths.start(tip) {
            Ok(upload) => {
                let remove = || std::fs::remove_file(&self.skipped);
                let _ = under_write_lock(&lock, remove).unwrap_or_else(remove);
                Ok(upload)
            }
            Err(error) => {
                let note = Skipped {
                    tip: tip.to_owned(),
                    attempt: uuid::Uuid::new_v4().to_string(),
                };
                if let Some(directory) = self.skipped.parent() {
                    let _ = config::create_state_dir(directory);
                }
                if let Ok(bytes) = serde_json::to_vec(&note) {
                    // Without its lock, as on a filesystem that takes none, the note is written
                    // all the same: an upload that didn't run is told rather than kept quiet.
                    let write = || replace(&self.skipped, &bytes);
                    let _ = under_write_lock(&lock, write).unwrap_or_else(write);
                }
                Err(error)
            }
        }
    }

    /// Held exclusively while the note is written, or read and removed, so a removal decided on
    /// one note never takes another written meanwhile. It sits beside the note, which is replaced
    /// whole on every write, so the lock is never replaced with it.
    fn note_lock(&self) -> PathBuf {
        self.skipped.with_extension("lock")
    }

    /// What the newest automatic upload of the branch left that didn't go through, with the
    /// attempt it belongs to and where telling it is marked: beside the note for an upload that
    /// didn't run, as the record's directory may be what kept it from running. An unreadable
    /// record or note is an error, never nothing.
    fn newest(&self) -> crate::Result<Option<(Undone, String, PathBuf)>> {
        match std::fs::read(&self.skipped) {
            Ok(bytes) => {
                let attempt = serde_json::from_slice::<Skipped>(&bytes)
                    .map(|note| note.attempt)
                    .unwrap_or_default();
                return Ok(Some((
                    Undone::Unrecorded,
                    attempt,
                    self.skipped.with_extension("told"),
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).context("the note of an upload that didn't run can't be read");
            }
        }
        let bytes = match std::fs::read(&self.paths.record) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("the upload record can't be read"),
        };
        let outcome: Outcome =
            serde_json::from_slice(&bytes).context("the upload record can't be read")?;
        let told = self.paths.told.clone();
        Ok(match outcome {
            // An agit that still attempted uploads with `push.auto` off saved its refusal in this
            // class. With it off nothing is attempted, so this failure says nothing now. Reading
            // changes nothing: `agit status` and the menu bar's listing read here, and
            // [`clear_failure_with_upload_off`] removes the record when a turn is saved.
            Outcome::Failed {
                reason: UploadFailure::Other,
                ..
            } if self.automatic_upload_off() => None,
            Outcome::Failed {
                reason, attempt, ..
            } => Some((Undone::Failed(reason), attempt, told)),
            Outcome::Started { attempt, .. } if !held(&self.paths.running) => {
                Some((Undone::Ended, attempt, told))
            }
            Outcome::Started { .. } | Outcome::Uploaded { .. } => None,
        })
    }

    /// Whether automatic publication is off for this record's repository, in its checkout here.
    fn automatic_upload_off(&self) -> bool {
        let Some((repository, _)) = self.target.split_once('@') else {
            return false;
        };
        let Ok((owner, name)) = super::parse_slug(repository) else {
            return false;
        };
        let Some(repo) = config::repo_dir(&owner, &name).ok().and_then(Repo::open) else {
            return false;
        };
        super::auto_push::publication_allowed(&repo, repository).is_ok_and(|allowed| !allowed)
    }

    fn undone(&self) -> crate::Result<Option<Undone>> {
        Ok(self.newest()?.map(|(undone, ..)| undone))
    }

    /// [`Place::newest`] once per attempt: telling it marks it told.
    fn unreported(&self) -> Option<Undone> {
        let (undone, attempt, told) = self.newest().ok()??;
        if std::fs::read_to_string(&told).ok().as_deref() == Some(attempt.as_str()) {
            return None;
        }
        let _ = replace(&told, attempt.as_bytes());
        Some(undone)
    }

    /// The Hub acknowledged a push of the branch whose commit `covers` a tip when it is that tip
    /// or a descendant of it. An upload of a covered tip that failed, ended without saying how it
    /// went or didn't run no longer stands for the branch, so its record or note goes. One of a
    /// tip the commit doesn't hold stays, as a push frozen before that turn was saved never
    /// uploaded it; so does the record of an upload still running, whose own outcome is recorded
    /// over it. Each is read, judged and removed under the lock it is written under, and without
    /// that lock it stays.
    fn acknowledge(self, covers: impl Fn(&str) -> bool) {
        under_write_lock(&self.note_lock(), || {
            let note = std::fs::read(&self.skipped)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Skipped>(&bytes).ok());
            if note.is_some_and(|note| covers(&note.tip)) {
                let _ = std::fs::remove_file(&self.skipped);
            }
        });
        let paths = self.paths;
        under_write_lock(&paths.writing, || {
            let stale = match read(&paths.record) {
                Some(Outcome::Failed { tip, .. }) => covers(&tip),
                Some(Outcome::Started { tip, .. }) => !held(&paths.running) && covers(&tip),
                Some(Outcome::Uploaded { .. }) | None => false,
            };
            if stale {
                let _ = std::fs::remove_file(&paths.record);
            }
        });
    }

    /// Whether the newest upload is one at `tip` that didn't run.
    fn skipped_at(&self, tip: &str) -> bool {
        std::fs::read(&self.skipped)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Skipped>(&bytes).ok())
            .is_some_and(|note| note.tip == tip)
    }
}

impl Paths {
    fn start(self, tip: &str) -> crate::Result<Upload> {
        let directory = self
            .record
            .parent()
            .context("upload record has no directory")?;
        config::create_state_dir(directory)
            .context("the upload records' directory can't be created")?;
        let running = config::state_file_options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.running)
            .context("the upload's lock can't be opened")?;
        fs2::FileExt::lock_shared(&running).context("the upload's lock can't be taken")?;
        let attempt = uuid::Uuid::new_v4().to_string();
        let started = Outcome::Started {
            tip: tip.to_owned(),
            attempt: attempt.clone(),
        };
        under_write_lock(&self.writing, || write(&self.record, &started))
            .context("the upload record's lock can't be taken")?
            .context("the upload record can't be written")?;
        Ok(Upload {
            paths: self,
            tip: tip.to_owned(),
            attempt,
            running,
        })
    }
}

impl Upload {
    /// Records how the upload went, over its own start only: a record another upload started
    /// since, of this tip or another, or one removed because the branch was dropped or its project
    /// unbound, stays as it is.
    fn finish(self, result: Result<(), UploadFailure>) {
        let Self {
            paths,
            tip,
            attempt,
            running,
        } = self;
        let started = Outcome::Started {
            tip: tip.clone(),
            attempt: attempt.clone(),
        };
        under_write_lock(&paths.writing, || {
            if read(&paths.record).as_ref() == Some(&started) {
                let outcome = match result {
                    Ok(()) => Outcome::Uploaded { tip },
                    Err(reason) => Outcome::Failed {
                        tip,
                        reason,
                        attempt,
                    },
                };
                let _ = write(&paths.record, &outcome);
            }
        });
        let _ = fs2::FileExt::unlock(&running);
    }
}

/// Why a branch's newest automatic upload didn't go through. A class of [`UploadFailure`] is told
/// in its own words wherever this is, so a new class needs nothing beyond its own description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Undone {
    /// It ran and failed.
    Failed(UploadFailure),
    /// Its process ended before it recorded how it went.
    Ended,
    /// It didn't run, because its start couldn't be recorded.
    Unrecorded,
}

impl Undone {
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Self::Failed(reason) => reason.describe(),
            Self::Ended => "it stopped before it recorded how it went; the next turn tries again",
            Self::Unrecorded => {
                "this computer couldn't record the upload's outcome, so it didn't run; the next turn tries again"
            }
        }
    }

    /// The name `agit status --json` gives the state.
    pub(crate) fn state(self) -> &'static str {
        match self {
            Self::Failed(_) => "failed",
            Self::Ended => "ended",
            Self::Unrecorded => "unrecorded",
        }
    }

    pub(crate) fn reason(self) -> Option<UploadFailure> {
        match self {
            Self::Failed(reason) => Some(reason),
            Self::Ended | Self::Unrecorded => None,
        }
    }
}

/// The newest automatic upload of `repository@branch` when it didn't go through and the turn-end
/// line has not told it yet; telling it marks it told, so each upload is told once.
pub(crate) fn unreported(repository: &str, branch: &str) -> Option<Undone> {
    Place::of(repository, branch).ok()?.unreported()
}

/// The branches of `repository` whose newest automatic upload didn't go through, or why one's
/// record can't be read. `branches` lists the checkout's local branches; it is asked only when the
/// repository has a record or a note, since only a branch's name finds its record.
pub(crate) fn undone_in(
    repository: &str,
    branches: impl FnOnce() -> crate::Result<Vec<String>>,
) -> crate::Result<Vec<(String, crate::Result<Undone>)>> {
    let mut any = false;
    for directory in [repository_dir(repository)?, skipped_dir(repository)?] {
        match std::fs::read_dir(&directory) {
            Ok(mut entries) => any |= entries.next().is_some(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("the upload records can't be read"),
        }
    }
    if !any {
        return Ok(Vec::new());
    }
    let mut found = Vec::new();
    for branch in branches()? {
        match Place::of(repository, &branch).and_then(|place| place.undone()) {
            Ok(None) => {}
            Ok(Some(undone)) => found.push((branch, Ok(undone))),
            Err(error) => found.push((branch, Err(error))),
        }
    }
    Ok(found)
}

/// Removes what an agit that still attempted uploads with `push.auto` off left for this branch: the
/// class its refusal was saved in. Called where a turn is saved and automatic upload is off.
pub(crate) fn clear_failure_with_upload_off(repository: &str, branch: &str) {
    let Ok(place) = Place::of(repository, branch) else {
        return;
    };
    under_write_lock(&place.paths.writing, || {
        if matches!(
            read(&place.paths.record),
            Some(Outcome::Failed {
                reason: UploadFailure::Other,
                ..
            })
        ) {
            let _ = std::fs::remove_file(&place.paths.record);
        }
    });
}

/// The Hub acknowledged `repository@branch` at the local commit `source`, whoever pushed it:
/// `repository` is the local checkout the branch's uploads are recorded by, whichever repository
/// on the Hub the push published to. See [`Place::acknowledge`] for what that clears.
pub(crate) fn acknowledged(repo: &Repo, repository: &str, branch: &str, source: &str) {
    let Ok(place) = Place::of(repository, branch) else {
        return;
    };
    place.acknowledge(|tip| {
        tip == source
            || repo
                .git_opt(&["merge-base", "--is-ancestor", tip, source])
                .is_some()
    });
}

/// Removes the record of `repository@branch`, or with no branch every record of `repository`,
/// with the notes of its uploads that didn't run. Each record goes under its write lock, as a
/// finishing upload replaces it, so an upload that read its own start before the removal cannot
/// write the record back. The lock files stay: they hold nothing, and removing one that is held
/// would let the next upload lock a new file while the old holder still writes.
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
                .map(Paths::of)
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
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
        let _ = std::fs::remove_file(&paths.told);
    }
    let skipped = skipped_dir(repository).map(|directory| match branch {
        Some(branch) => {
            let note = directory.join(digest(branch)).with_extension("json");
            let _ = std::fs::remove_file(note.with_extension("told"));
            std::fs::remove_file(note)
        }
        // The notes' locks stay, as the records' do.
        None => match std::fs::read_dir(&directory) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_none_or(|extension| extension != "lock"))
                .try_for_each(std::fs::remove_file),
            Err(error) => Err(error),
        },
    });
    match skipped {
        Ok(Err(error)) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).context("cannot remove the notes of uploads that didn't run")
        }
        _ => Ok(()),
    }
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
        let place = Place::of(&self.repository, &self.branch);
        if let Ok(place) = &place {
            if place.skipped_at(tip) {
                return Status::NotUploaded(UNRECORDED);
            }
            if let Some(outcome) = read(&place.paths.record).filter(|outcome| outcome.tip() == tip)
            {
                return match outcome {
                    Outcome::Uploaded { .. } => Status::Uploaded,
                    Outcome::Failed { reason, .. } => Status::Failed(reason.describe()),
                    Outcome::Started { .. } if held(&place.paths.running) => Status::Uploading,
                    Outcome::Started { .. } => Status::Unknown,
                };
            }
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
        Paths::of(directory.join("record.json"))
    }

    /// A failure an agit that still attempted uploads with `push.auto` off recorded goes, along
    /// with what it would tell, once that agit is replaced; any other failure stays, and so does
    /// that one while automatic upload is on, when it is a failure that needs telling.
    #[test]
    fn a_failure_recorded_with_automatic_upload_off_is_dropped() {
        const CHILD: &str = "AGIT_UPLOAD_OUTCOME_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "commands::upload_outcome::tests::a_failure_recorded_with_automatic_upload_off_is_dropped",
                ])
                .env(CHILD, "1")
                .env("AGIT_HOME", home.path())
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }
        let repo = Repo::init(&config::repo_dir("alice", "app").unwrap()).unwrap();
        let recorded = |branch: &str, reason: UploadFailure| {
            let place = Place::of("alice/app", branch).unwrap();
            std::fs::create_dir_all(place.paths.record.parent().unwrap()).unwrap();
            let failed = Outcome::Failed {
                tip: "t".into(),
                reason,
                attempt: "a".into(),
            };
            write(&place.paths.record, &failed).unwrap();
        };
        let exists = |branch: &str| {
            Place::of("alice/app", branch)
                .unwrap()
                .paths
                .record
                .exists()
        };

        repo.set_auto_push(Some(true)).unwrap();
        recorded("on", UploadFailure::Other);
        assert_eq!(
            unreported("alice/app", "on"),
            Some(Undone::Failed(UploadFailure::Other))
        );
        assert!(exists("on"));

        repo.set_auto_push(Some(false)).unwrap();
        recorded("off", UploadFailure::Other);
        recorded("hub", UploadFailure::Hub);
        assert_eq!(unreported("alice/app", "off"), None);
        assert!(exists("off"), "reading changes nothing");
        assert_eq!(
            unreported("alice/app", "hub"),
            Some(Undone::Failed(UploadFailure::Hub))
        );
        clear_failure_with_upload_off("alice/app", "off");
        clear_failure_with_upload_off("alice/app", "hub");
        assert!(!exists("off"));
        assert!(exists("hub"));
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

        let failing = at(directory.path()).start("tip-1").unwrap();
        let succeeding = at(directory.path()).start("tip-1").unwrap();
        failing.finish(Err(UploadFailure::Hub));
        succeeding.finish(Ok(()));
        assert_eq!(
            record(),
            Some(Outcome::Uploaded {
                tip: "tip-1".into()
            })
        );

        let older = at(directory.path()).start("tip-2").unwrap();
        let newer = at(directory.path()).start("tip-3").unwrap();
        older.finish(Err(UploadFailure::Hub));
        assert!(
            matches!(record(), Some(Outcome::Started { tip, .. }) if tip == "tip-3"),
            "an older upload does not overwrite a newer start"
        );
        newer.finish(Err(UploadFailure::TooLarge));
        assert!(
            matches!(
                record(),
                Some(Outcome::Failed { tip, reason: UploadFailure::TooLarge, .. }) if tip == "tip-3"
            ),
            "{:?}",
            record()
        );
    }

    /// An automatic upload whose start can't be recorded doesn't run, and that is told rather
    /// than kept quiet: the turn-end line says it once and `agit status` lists it, until an upload
    /// whose start is recorded runs as usual. A record file that is corrupt or unreadable is no
    /// such reason: the start replaces it. A runner that pushed unrecorded would let a failure go
    /// untold; one that stopped quietly would leave every later turn on this computer with nothing
    /// said; one that read the record before starting would stop for good on a damaged file.
    ///
    /// A failure or a skip stays until the Hub acknowledges a commit that holds its turn. A push
    /// frozen at an older commit can be acknowledged after a newer turn's upload failed, or while
    /// a newer turn's skip is being written; clearing on any acknowledgement, or deciding on one
    /// note and removing another, would erase that turn though it never went up.
    #[test]
    fn an_upload_that_cant_be_recorded_doesnt_run_and_says_so() {
        let directory = tempfile::tempdir().unwrap();
        let blocked = directory.path().join("blocked");
        std::fs::write(&blocked, b"").unwrap();
        let records = blocked.join("uploads");
        let notes = directory.path().join("checkout");
        let place = || Place::at(&records, &notes);
        let pushes = std::cell::Cell::new(0);
        let push = |refusal: Option<Refusal>| {
            recorded(
                Ok(place()),
                "tip-1",
                || pushes.set(pushes.get() + 1),
                move |_| refusal,
            )
        };

        assert!(push(None).is_err());
        assert_eq!(pushes.get(), 0, "an upload whose start isn't recorded ran");
        assert_eq!(place().undone().unwrap(), Some(Undone::Unrecorded));
        assert_eq!(place().unreported(), Some(Undone::Unrecorded));
        assert_eq!(place().unreported(), None, "the same skip is told once");

        std::fs::remove_file(&blocked).unwrap();
        std::fs::create_dir_all(&records).unwrap();
        std::fs::write(at(&records).record, b"not a record").unwrap();
        assert!(push(None).is_ok());
        assert_eq!(pushes.get(), 1);
        assert_eq!(place().undone().unwrap(), None);
        assert_eq!(place().unreported(), None);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(at(&records).record, std::fs::Permissions::from_mode(0o000))
                .unwrap();
        }
        let diverged = format!(
            "remote branch work {}",
            crate::domain::privacy_git::sessions::DIVERGED
        );
        assert!(push(Some(Refusal::child(Some(1), diverged))).is_ok());
        assert_eq!(pushes.get(), 2);
        let failed = Some(Undone::Failed(UploadFailure::Diverged));
        assert_eq!(place().undone().unwrap(), failed);
        assert_eq!(place().unreported(), failed);
        assert_eq!(place().unreported(), None);

        // `tip-0` is older than `tip-1`, `tip-1` older than `tip-2`.
        let holds = |head: &'static str| {
            move |tip: &str| match head {
                "tip-0" => tip == "tip-0",
                "tip-1" => matches!(tip, "tip-0" | "tip-1"),
                _ => true,
            }
        };
        place().acknowledge(holds("tip-0"));
        assert_eq!(
            place().undone().unwrap(),
            failed,
            "an older commit's acknowledgement erased a newer turn's failure"
        );
        place().acknowledge(holds("tip-1"));
        assert_eq!(place().undone().unwrap(), None);

        std::fs::remove_dir_all(&blocked).unwrap();
        std::fs::write(&blocked, b"").unwrap();
        assert!(
            recorded(
                Ok(place()),
                "tip-2",
                || pushes.set(pushes.get() + 1),
                |_| None
            )
            .is_err()
        );
        assert_eq!(pushes.get(), 2);
        place().acknowledge(holds("tip-1"));
        assert_eq!(place().undone().unwrap(), Some(Undone::Unrecorded));
        place().acknowledge(holds("tip-2"));
        std::fs::remove_file(&blocked).unwrap();
        assert_eq!(place().undone().unwrap(), None);

        // After the acknowledgement of `tip-2` has read its note, a newer turn's upload fails to
        // be recorded and writes its own; the acknowledgement then clears what it judged. It is
        // given a bounded time to land first, which a clear outside the note's lock lets it do.
        std::fs::write(&blocked, b"").unwrap();
        assert!(
            recorded(
                Ok(place()),
                "tip-2",
                || pushes.set(pushes.get() + 1),
                |_| None
            )
            .is_err()
        );
        let (landed, landing) = std::sync::mpsc::channel();
        let newer = std::cell::RefCell::new(None);
        place().acknowledge(|tip| {
            if newer.borrow().is_none() {
                let (records, notes, landed) = (records.clone(), notes.clone(), landed.clone());
                *newer.borrow_mut() = Some(std::thread::spawn(move || {
                    let skipped = Place::at(&records, &notes).start("tip-3").is_err();
                    let _ = landed.send(());
                    skipped
                }));
                let _ = landing.recv_timeout(std::time::Duration::from_millis(200));
            }
            tip == "tip-2"
        });
        assert!(newer.into_inner().unwrap().join().unwrap());
        assert!(
            place().skipped_at("tip-3"),
            "an older commit's acknowledgement cleared a newer turn's note"
        );
        assert_eq!(place().undone().unwrap(), Some(Undone::Unrecorded));
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
            let start = |branch: &str, tip: &str| {
                Place::of("alice/app", branch).unwrap().start(tip).unwrap()
            };
            let running = start("work", "tip-1");
            let files = paths("alice/app", "work").unwrap();
            forget("alice/app", Some("work")).unwrap();
            assert!(!files.record.exists());
            assert!(files.running.exists() && files.writing.exists());
            running.finish(Ok(()));
            assert!(
                !files.record.exists(),
                "a running upload wrote a forgotten record back"
            );

            let running = start("other", "tip-2");
            forget("alice/app", None).unwrap();
            running.finish(Err(UploadFailure::Hub));
            assert!(!paths("alice/app", "other").unwrap().record.exists());

            // A finish that read its own start holds the write lock until it has replaced the
            // record. A removal that does not wait for that lock returns while it is held, and the
            // replacement then writes the forgotten record back.
            let _running = start("late", "tip-3");
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
