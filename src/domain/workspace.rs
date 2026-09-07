//! A workspace binding records the Agent repo chosen for a project directory.
//! It supports setup and status display; it never selects a session branch for a command.

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

/// Bind a directory to a repo at init or clone.
///
/// Refused when the directory is already bound to a **different** repo, unless `rebind`: the
/// binding is a single value per directory, parallel sessions in one directory each running
/// `init`/`clone` would rewrite the recorded routing back and forth. Changing it must be explicit.
pub fn bind(dir: &Path, repo: &str, rebind: bool) -> Result<()> {
    bind_in(&self::dir()?, dir, repo, rebind)
}

fn bind_in(root: &Path, dir: &Path, repo: &str, rebind: bool) -> Result<()> {
    let canon = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let mut ws = read_in(root, dir).unwrap_or(Workspace {
        dir: canon.to_string_lossy().to_string(),
        repo: repo.to_string(),
    });
    if ws.repo != repo && !rebind {
        anyhow::bail!(
            "this directory is already bound to {}; keep working there, or rebind it explicitly with `--rebind`",
            ws.repo
        );
    }
    ws.repo = repo.to_string();
    write_in(root, dir, &ws)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_legacy_pin_does_not_survive_a_binding_write() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("workspaces");
        let work = tmp.path().join("project");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            path_for_in(&root, &work),
            serde_json::json!({
                "dir": work, "repo": "me/payments", "pinned": "old-session"
            })
            .to_string(),
        )
        .unwrap();
        assert_eq!(read_in(&root, &work).unwrap().repo, "me/payments");
        bind_in(&root, &work, "me/payments", false).unwrap();
        let stored = std::fs::read_to_string(path_for_in(&root, &work)).unwrap();
        assert!(!stored.contains("pinned"));
        assert!(bind_in(&root, &work, "me/other", false).is_err());
    }
}
