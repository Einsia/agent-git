//! The merge transaction lock: the target branch stays locked between `agit merge` starting and
//! the merge landing.
//!
//! A lock is one JSON file in the repo's `.git/AGIT_MERGE_TX` (source branch, fork point, target
//! branch head). It lives in a file and not in memory: the merge agent is another process, maybe
//! in another terminal; and `.git/` is not part of the tree, so the lock never enters history
//! with a commit.
//!
//! Behavior while the lock is held (PRD, "merge" section):
//! * an ordinary `agit commit` on the **target branch** is always rejected, pointing at
//!   `merge --status`;
//! * automatic settlement from that merge agent session's hooks (`commit --from-hook`) is
//!   silently skipped;
//! * `merge --continue` creates the merge commit once its checks pass and unlocks; `--abort`
//!   discards and unlocks.
//!
//! # The granularity is one branch, not one repo
//!
//! The lock file is repo-level (one repo runs one merge at a time), but the only branch it blocks
//! is [`Tx::target`]. Testing whether the lock file exists rejects ordinary commits on every
//! other branch of the same repo for as long as the transaction runs, and the error text names
//! that unrelated branch as "locked". The gate goes through [`locking`]; [`is_locked`] answers
//! only "is a transaction running".

use crate::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const LOCK_FILE: &str = "AGIT_MERGE_TX";

/// The marker a merge agent session carries, of the form `<owner>/<name>@<target>`.
///
/// `agit merge` injects it into the **child process's** environment when it launches the merge
/// agent (never into its own), and it is the authoritative declaration that "this session is the
/// merge agent" — an order of magnitude more precise than the lock file, which can say only
/// "this repo has a merge running", never "it runs in my session".
pub const ENV: &str = "AGIT_MERGE_TX";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tx {
    /// A preparation belongs to this transaction instance even when another uses identical refs.
    #[serde(default)]
    pub generation: Option<String>,
    /// The target branch (the merge lands on it).
    pub target: String,
    /// The source ref (what the merge draws from).
    pub source: String,
    /// The source repo's full identity; a new transaction always writes it, and a lock file
    /// without it reads back as empty.
    #[serde(default)]
    pub source_repo: Option<String>,
    /// The branch the source ref resolved against. `@#n` and a repo-only source need it to
    /// re-resolve free of the cwd.
    #[serde(default)]
    pub source_branch: Option<String>,
    /// The unique common Git ancestor, or an empty string when the histories are unrelated.
    /// An absent ancestor does not establish a turn-count baseline.
    pub base: String,
    /// The target branch head when the transaction started (what the CAS compares against).
    pub target_head: String,
    /// The commit the source resolved to when the transaction started.
    pub source_head: String,
    /// The source events the merge agent has picked (the ref text in `#n` / `#n.k` form, one
    /// per entry).
    #[serde(default)]
    pub picked: Vec<String>,
    /// The merge agent's merge_summary text.
    #[serde(default)]
    pub summary: Option<String>,
}

impl Tx {
    /// Mutable progress belongs to the same creation and frozen source/target selection.
    pub fn same_instance(&self, other: &Self) -> bool {
        self.generation == other.generation
            && self.target == other.target
            && self.target_head == other.target_head
            && self.source == other.source
            && self.source_head == other.source_head
            && self.source_repo == other.source_repo
            && self.source_branch == other.source_branch
            && self.base == other.base
    }

    /// The pick list (the raw ref text, for example `B#3..#5`).
    pub fn picked_refs(&self) -> &[String] {
        &self.picked
    }
    pub fn picked_count(&self) -> usize {
        self.picked.len()
    }
    pub fn has_summary(&self) -> bool {
        self.summary
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
    }
    pub fn summary_text(&self) -> String {
        self.summary.clone().unwrap_or_default()
    }
    /// Append to the picks (deduplicated).
    pub fn pick_more(&mut self, refs: &[String]) {
        for r in refs {
            if !self.picked.contains(r) {
                self.picked.push(r.clone());
            }
        }
    }
    /// Remove picks; returns how many were removed.
    pub fn drop(&mut self, refs: &[String]) -> usize {
        let before = self.picked.len();
        self.picked.retain(|p| !refs.contains(p));
        before - self.picked.len()
    }
    pub fn set_summary(&mut self, text: String) {
        self.summary = Some(text);
    }
}

fn lock_path(repo_root: &Path) -> PathBuf {
    // One lock per repo: a path built from a session branch's worktree must still land in the
    // shared git directory.
    crate::domain::repo::common_git_dir(repo_root).join(LOCK_FILE)
}

/// Whether this repo has an open transaction.
///
/// **Not a write gate**: it does not know which branch is locked. Use [`locking`] to decide
/// whether a branch is writable right now. It exists for one purpose — the exclusion test for
/// "one repo runs one merge at a time".
pub fn is_locked(repo_root: &Path) -> bool {
    lock_path(repo_root).exists()
}

/// Whether `branch` is the branch some transaction has locked; if it is, hands back that
/// transaction.
///
/// Returns the transaction itself and not a bool: the error text has to say which branch is
/// locked and what the source is — "{branch} is locked" is false when the lock is on another
/// branch.
pub fn locking(repo_root: &Path, branch: &str) -> Option<Tx> {
    read(repo_root)
        .ok()
        .flatten()
        .filter(|tx| tx.target == branch)
}

/// Whether automatic settlement from hooks suspends for this call.
///
/// The design suspends **a merge agent session carrying the `AGIT_MERGE_TX` marker**, so the test
/// reads that marker from the environment first — suspension is scoped to that one session and
/// not to the whole repo (other sessions on the same machine settle automatically as usual). With
/// the marker unreadable it falls back to the lock file, because a hook process does not
/// necessarily inherit the merge agent's environment; the fallback too blocks only the target
/// branch.
pub fn hook_suspended(repo_root: &Path, branch: &str) -> bool {
    match suspended_by(std::env::var(ENV).ok().as_deref(), branch) {
        Some(v) => v,
        None => locking(repo_root, branch).is_some(),
    }
}

/// The pure-function core of the marker test: `None` = the marker is unusable, hand over to the
/// fallback.
///
/// Splitting on `@` uses `split_once` and not `rsplit_once`: a slug is `owner/name` and holds no
/// `@`, while a branch name may (`feat@2`). Same approach as `AGIT_SESSION`.
fn suspended_by(marker: Option<&str>, branch: &str) -> Option<bool> {
    let m = marker?;
    let (slug, target) = m.split_once('@')?;
    if slug.is_empty() || target.is_empty() {
        return None;
    }
    Some(target == branch)
}

pub fn read(repo_root: &Path) -> Result<Option<Tx>> {
    match std::fs::read_to_string(lock_path(repo_root)) {
        Ok(t) => Ok(Some(serde_json::from_str(&t)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Serialize transaction publication, progress, cancellation, and launch admission.
/// The handle is not inherited by spawned runtimes, so cancellation never waits for their lifetime.
pub struct ControlGuard {
    path: PathBuf,
    _file: std::fs::File,
}

impl ControlGuard {
    pub fn acquire(repo_root: &Path) -> Result<Self> {
        use anyhow::Context as _;

        let path = lock_path(repo_root);
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options
            .open(path.with_extension("control"))
            .context("cannot open merge transaction control")?;
        fs2::FileExt::lock_exclusive(&file).context("cannot lock merge transaction control")?;
        Ok(Self { path, _file: file })
    }

    pub fn read(&self) -> Result<Option<Tx>> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Ok(Some(serde_json::from_str(&text)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn publish(&self, tx: &Tx, replace: bool) -> Result<()> {
        use anyhow::Context as _;
        use std::io::Write as _;

        let mut pending = tempfile::NamedTempFile::new_in(
            self.path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("the transaction has no parent directory"))?,
        )?;
        writeln!(pending, "{}", serde_json::to_string_pretty(tx)?)?;
        pending.as_file().sync_all()?;
        if replace {
            pending
                .persist(&self.path)
                .map_err(|error| error.error)
                .context("cannot update the merge transaction")?;
        } else {
            pending.persist_noclobber(&self.path).map_err(|error| error.error)
                .context("cannot open the merge transaction without replacing existing state; inspect `agit merge --status`")?;
        }
        Ok(())
    }

    pub fn write(&self, tx: &Tx) -> Result<()> {
        self.publish(tx, true)
    }

    pub fn remove(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

/// Publish a complete transaction only when no transaction already owns the repository.
/// Admission must not use the update path, which intentionally replaces existing progress.
pub fn create(repo_root: &Path, tx: &Tx) -> Result<()> {
    ControlGuard::acquire(repo_root)?.publish(tx, false)
}

pub fn lock(repo_root: &Path, tx: &Tx) -> Result<()> {
    ControlGuard::acquire(repo_root)?.write(tx)
}

pub fn unlock(repo_root: &Path) -> Result<()> {
    ControlGuard::acquire(repo_root)?.remove()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_read_unlock_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let git = d.path().join(".git");
        std::fs::create_dir_all(&git).unwrap();
        assert!(!is_locked(d.path()));
        let tx = Tx {
            generation: None,
            target: "a".into(),
            source: "b".into(),
            source_repo: None,
            source_branch: None,
            base: "x".into(),
            target_head: "y".into(),
            source_head: "z".into(),
            picked: vec![],
            summary: None,
        };
        lock(d.path(), &tx).unwrap();
        assert!(is_locked(d.path()));
        assert_eq!(read(d.path()).unwrap().unwrap().source, "b");
        unlock(d.path()).unwrap();
        assert!(!is_locked(d.path()));
    }

    fn tx_on(target: &str) -> Tx {
        Tx {
            generation: None,
            target: target.into(),
            source: "b".into(),
            source_repo: None,
            source_branch: None,
            base: "x".into(),
            target_head: "y".into(),
            source_head: "z".into(),
            picked: vec![],
            summary: None,
        }
    }

    #[test]
    fn creation_preserves_existing_progress_and_unreadable_state() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join(".git")).unwrap();
        let mut original = tx_on("target");
        original.picked.push("source#1".into());
        original.summary = Some("preserve the reconciliation".into());
        create(directory.path(), &original).unwrap();
        let path = lock_path(directory.path());
        let before = std::fs::read(&path).unwrap();
        assert!(create(directory.path(), &tx_on("other")).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        original.picked.push("source#2".into());
        lock(directory.path(), &original).unwrap();
        assert_eq!(
            read(directory.path()).unwrap().unwrap().picked,
            original.picked
        );
        std::fs::write(&path, "unreadable transaction").unwrap();
        assert!(create(directory.path(), &tx_on("other")).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"unreadable transaction");
    }

    #[test]
    fn concurrent_creators_publish_only_the_winning_transaction() {
        use std::sync::{Arc, Barrier};

        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join(".git")).unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let winners = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|index| {
                    let barrier = barrier.clone();
                    let root = directory.path();
                    scope.spawn(move || {
                        let target = format!("target-{index}");
                        let mut transaction = tx_on(&target);
                        transaction.picked.push(format!("source-{index}#1"));
                        transaction.summary = Some(format!("intent from {index}"));
                        barrier.wait();
                        create(root, &transaction).is_ok().then_some(transaction)
                    })
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(winners.len(), 1);
        let actual = read(directory.path()).unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(&winners[0]).unwrap()
        );
    }

    #[test]
    fn legacy_transactions_remain_readable_without_generation() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join(".git")).unwrap();
        let mut legacy = serde_json::to_value(tx_on("target")).unwrap();
        legacy.as_object_mut().unwrap().remove("generation");
        std::fs::write(
            lock_path(directory.path()),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();
        let control = ControlGuard::acquire(directory.path()).unwrap();
        let mut transaction = control.read().unwrap().unwrap();
        assert!(transaction.generation.is_none());
        transaction.pick_more(&["source#1".into()]);
        control.write(&transaction).unwrap();
        assert_eq!(control.read().unwrap().unwrap().picked, transaction.picked);
        control.remove().unwrap();
        assert!(control.read().unwrap().is_none());
    }

    /// The lock blocks the target branch only: other branches of the same repo stay writable.
    #[test]
    fn the_lock_only_covers_the_target_branch() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".git")).unwrap();
        lock(d.path(), &tx_on("main")).unwrap();

        assert!(
            locking(d.path(), "main").is_some(),
            "the target branch must be blocked"
        );
        assert!(
            locking(d.path(), "other").is_none(),
            "the transaction locks main; blocking other along with it is the bug itself"
        );
        // The exclusion test still sees this transaction.
        assert!(is_locked(d.path()));
    }

    /// Hook suspension reads the session marker first: the scope is that merge agent, not the
    /// whole repo.
    #[test]
    fn hook_suspension_reads_the_session_marker_first() {
        assert_eq!(suspended_by(Some("me/repo@main"), "main"), Some(true));
        assert_eq!(suspended_by(Some("me/repo@main"), "other"), Some(false));
        // A branch name holding `@` still splits correctly (a slug has no `@`, so split from
        // the left).
        assert_eq!(suspended_by(Some("me/repo@feat@2"), "feat@2"), Some(true));
        // An unusable marker → None, and the caller falls back to the lock file.
        assert_eq!(suspended_by(None, "main"), None);
        assert_eq!(suspended_by(Some("garbage"), "main"), None);
        assert_eq!(suspended_by(Some("me/repo@"), "main"), None);
    }

    /// With no marker the fallback is the lock file, and the fallback too blocks only the
    /// target branch.
    #[test]
    fn hook_suspension_falls_back_to_the_lock_file_per_branch() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".git")).unwrap();
        lock(d.path(), &tx_on("main")).unwrap();
        assert!(locking(d.path(), "main").is_some());
        assert!(locking(d.path(), "side").is_none());
    }
}
