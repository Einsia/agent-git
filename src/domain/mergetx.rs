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
    /// The fork-point commit (a same-repo merge-base, or the last commit of the common prefix
    /// of the cross-repo hash chain).
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

pub fn lock(repo_root: &Path, tx: &Tx) -> Result<()> {
    std::fs::write(
        lock_path(repo_root),
        format!("{}\n", serde_json::to_string_pretty(tx)?),
    )?;
    Ok(())
}

pub fn unlock(repo_root: &Path) -> Result<()> {
    match std::fs::remove_file(lock_path(repo_root)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
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
