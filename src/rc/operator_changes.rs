//! Which workspaces hold a file change an operator approved.
//!
//! What the owner granted operators covers the repository as the owner knew it, so once an
//! operator has approved a file change in a workspace, granted commands there go back to the owner
//! until the owner grants a command again. The fact lives on disk, one file per workspace, written
//! before the change goes ahead: a restart must not lose it, and one that can't be read counts as
//! true.

use anyhow::Result;
use std::path::PathBuf;

fn path(workspace: &str) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let digest = hex::encode(Sha256::digest(workspace.as_bytes()));
    Ok(super::rc_dir()?
        .join("operator-changes")
        .join(&digest[..32]))
}

/// Records that an operator approved a file change in `workspace`, on disk before it returns.
pub(crate) fn record(workspace: &str) -> Result<()> {
    let path = path(workspace)?;
    let directory = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("the record has no folder"))?;
    #[cfg(not(windows))]
    crate::infra::config::create_state_dir(directory)?;
    #[cfg(windows)]
    std::fs::create_dir_all(directory)?;
    std::fs::File::create(&path)?.sync_all()?;
    // The file itself is what matters; not every filesystem can sync a folder.
    if let Ok(directory) = std::fs::File::open(directory) {
        let _ = directory.sync_all();
    }
    Ok(())
}

/// Whether `workspace` holds such a change. Only a record that certainly isn't there says no.
pub(crate) fn recorded(workspace: &str) -> bool {
    match path(workspace).map(std::fs::symlink_metadata) {
        Ok(Err(error)) => error.kind() != std::io::ErrorKind::NotFound,
        _ => true,
    }
}

/// Forgets the change once the owner grants a command again; whether there was one to forget.
pub(crate) fn clear(workspace: &str) -> Result<bool> {
    match std::fs::remove_file(path(workspace)?) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}
