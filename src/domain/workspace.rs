//! workspace: the binding between a local code directory and a repo.
//!
//! # Design (mapped exactly onto the PRD's "local layout" and "`agit switch`" sections)
//!
//! It lives in `~/.agit/workspaces/<id>.json`; **not one byte is written inside the code repo**.
//! A workspace records the directory's canonical path, the bound repo (`owner/name`), and the
//! branch pinned by `agit switch`.
//!
//! It comes fourth in resolution order: explicit argument → `AGIT_SESSION` → harness environment
//! variable → workspace pin → cwd match. The pin is **per-directory** (every terminal in that
//! directory sees the same pin), and `AGIT_SESSION` always outranks it, so parallel sessions in
//! one directory do not pollute each other.
//!
//! The id is the first 16 hex of the canonical path's SHA-256: one path is always one file, and
//! the path content does not leak.

use crate::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    /// The directory's canonical path (the source of identity).
    pub dir: String,
    /// The bound repo, `owner/name`.
    pub repo: String,
    /// The branch pinned by `agit switch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<String>,
}

pub fn dir() -> Result<PathBuf> {
    Ok(crate::infra::config::agit_home()?.join("workspaces"))
}

/// Directory → workspace file. When canonicalization fails (the directory does not exist), the
/// given path is used unchanged.
pub fn path_for(dir: &Path) -> Result<PathBuf> {
    Ok(path_for_in(&self::dir()?, dir))
}

fn path_for_in(root: &Path, dir: &Path) -> PathBuf {
    let canon = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let mut h = Sha256::new();
    h.update(canon.to_string_lossy().as_bytes());
    let id = &hex::encode(h.finalize())[..16];
    root.join(format!("{id}.json"))
}

pub fn read(dir: &Path) -> Option<Workspace> {
    read_in(&self::dir().ok()?, dir)
}

fn read_in(root: &Path, dir: &Path) -> Option<Workspace> {
    serde_json::from_str(&std::fs::read_to_string(path_for_in(root, dir)).ok()?).ok()
}

pub fn write(dir: &Path, ws: &Workspace) -> Result<()> {
    write_in(&self::dir()?, dir, ws)
}

fn write_in(root: &Path, dir: &Path, ws: &Workspace) -> Result<()> {
    let p = path_for_in(root, dir);
    std::fs::create_dir_all(root)?;
    std::fs::write(&p, format!("{}\n", serde_json::to_string_pretty(ws)?))?;
    Ok(())
}

/// Bind a directory to a repo (called at init / clone; an existing pin is kept).
///
/// Refused when the directory is already bound to a **different** repo, unless `rebind`: the
/// binding is a single value per directory, parallel sessions in one directory each running
/// `init`/`clone` would rewrite it back and forth, and zero-argument commands rely on it to look
/// up "which repo is this". Changing the binding must be an explicit decision.
pub fn bind(dir: &Path, repo: &str, rebind: bool) -> Result<()> {
    bind_in(&self::dir()?, dir, repo, rebind)
}

fn bind_in(root: &Path, dir: &Path, repo: &str, rebind: bool) -> Result<()> {
    let canon = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let mut ws = read_in(root, dir).unwrap_or(Workspace {
        dir: canon.to_string_lossy().to_string(),
        repo: repo.to_string(),
        pinned: None,
    });
    if ws.repo != repo && !rebind {
        anyhow::bail!(
            "this directory is already bound to {}; keep working there, or rebind it explicitly with `--rebind`",
            ws.repo
        );
    }
    if ws.repo != repo {
        // The pin is a branch name in the old repo; once the repo changes it has no referent.
        ws.pinned = None;
    }
    ws.repo = repo.to_string();
    write_in(root, dir, &ws)
}

/// Pin / unpin (`--unbind` passes None). The directory must already be bound.
pub fn pin(dir: &Path, branch: Option<&str>) -> Result<()> {
    pin_in(&self::dir()?, dir, branch)
}

fn pin_in(root: &Path, dir: &Path, branch: Option<&str>) -> Result<()> {
    let mut ws = read_in(root, dir).ok_or_else(|| {
        anyhow::anyhow!(
            "this directory is not bound to any repo. Run `agit init <name>` or `agit clone <owner/repo>` first."
        )
    })?;
    ws.pinned = branch.map(str::to_string);
    write_in(root, dir, &ws)
}

/// Look up the branch pinned for a directory.
pub fn pinned(dir: &Path) -> Option<(String, String)> {
    let ws = read(dir)?;
    Some((ws.repo, ws.pinned?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_pin_roundtrip() {
        // The process environment is untouched (parallel tests would overwrite each other);
        // root is passed in directly.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspaces");
        let work = tmp.path().join("proj");
        std::fs::create_dir_all(&work).unwrap();
        bind_in(&root, &work, "me/payments", false).unwrap();
        pin_in(&root, &work, Some("refund-fix")).unwrap();
        assert_eq!(
            read_in(&root, &work).and_then(|w| w.pinned.map(|p| (w.repo, p))),
            Some(("me/payments".to_string(), "refund-fix".to_string()))
        );
        pin_in(&root, &work, None).unwrap();
        let ws = read_in(&root, &work).unwrap();
        assert_eq!(ws.pinned, None);
        assert_eq!(ws.repo, "me/payments");
    }
}
