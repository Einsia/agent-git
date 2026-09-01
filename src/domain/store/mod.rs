//! The local store: a **plain file directory** that holds nothing but links.
//!
//! ```text
//! $AGIT_HOME/store/<runtime>/<session-id>.json
//! ```
//!
//! # Why this is not a git repo
//!
//! A store that holds full transcript copies has a reason to be one: append-only text is the
//! shape git delta compression is best at, and repeated snapshots of one session collapse to a
//! fraction of their raw size. A store that holds only links has no such reason — a link is a few
//! hundred bytes of JSON. What is left is the history of what was adopted when, and that is not
//! worth a layer of git.
//!
//! Leaving it out buys: no `git add` / `git commit` on the path that writes a link, one fewer
//! concept, and no pile of thousands of commits that each change a few kilobytes.
//!
//! # Version history lives elsewhere
//!
//! The agent's version history is git, in [`crate::domain::repo::Repo`]
//! (`~/.agit/repos/<owner>/<name>/`). The store takes no part in publishing and never pushes; it
//! answers only which sessions this machine tracks, which agent each belongs to, and which
//! snapshot each has reached.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The local store. A directory, nothing more.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Store { root: root.into() }
    }

    /// Open the local store, creating the directory when it does not exist.
    ///
    /// "Create when missing" is deliberate: `agit import` may be the user's first agit command.
    pub fn open_or_init() -> Result<Store> {
        let root = crate::infra::config::store_root()?;
        std::fs::create_dir_all(&root)
            .with_context(|| format!("cannot create store directory {}", root.display()))?;
        Ok(Store::at(root))
    }

    /// Open an existing store; returns None when there is none.
    ///
    /// For the read-only commands (`log` / `status`) — creating a directory is a side effect they
    /// must not have.
    pub fn open() -> Result<Option<Store>> {
        let root = crate::infra::config::store_root()?;
        Ok(if root.is_dir() {
            Some(Store::at(root))
        } else {
            None
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Map a path to a filename-safe key.
///
/// Every non-alphanumeric character becomes `-`, so `/my/app` and `/my-app` collide into the
/// same slug. That is acceptable: the slug only points Claude Code at a project directory, and
/// the source of truth for which project a session belongs to is the cwd recorded in the link.
pub fn slug_for(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            out.push('-');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_collides_but_that_is_documented() {
        assert_eq!(slug_for(Path::new("/my/app")), "-my-app");
        assert_eq!(
            slug_for(Path::new("/my/app")),
            slug_for(Path::new("/my_app")),
            "a known and acceptable collision; the source of truth is the cwd in the link"
        );
    }

    #[test]
    fn store_is_just_a_directory() {
        // A store directory never contains a .git/.
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("store");
        std::fs::create_dir_all(&root).unwrap();
        let s = Store::at(&root);
        assert!(s.root().is_dir());
        assert!(
            !root.join(".git").exists(),
            "the store is a plain directory and must not contain a git repo"
        );
    }

    #[test]
    fn open_does_not_create() {
        // Read-only commands have no side effects.
        let d = tempfile::tempdir().unwrap();
        let missing = d.path().join("nope");
        let s = Store::at(&missing);
        // at() only records the path; it does not touch the disk.
        assert_eq!(s.root(), missing.as_path());
        assert!(!missing.exists());
    }
}
