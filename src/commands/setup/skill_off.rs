//! The runtimes whose agit Skill the person turned off with `agit setup --remove-skill`. The record
//! is local configuration in `AGIT_HOME`: setup, the refresh `agit upgrade` runs, `agit doctor` and
//! the setup notice read it, and it never leaves this computer.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::Context;

const FILE: &str = "skill-off.json";

fn path() -> Option<PathBuf> {
    crate::infra::config::agit_home()
        .ok()
        .map(|home| home.join(FILE))
}

/// The runtimes whose Skill is off. An absent or unreadable record turns none off, so it can only
/// bring the Skill back, never hide one the person wants.
pub(crate) fn read() -> BTreeSet<String> {
    path()
        .and_then(|path| crate::domain::storage::read_bytes_capped(&path, 4096).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Record `off` in place of what was there, removing the record once nothing is off.
pub(crate) fn write(off: &BTreeSet<String>) -> crate::Result<()> {
    let path = path().context("AGIT_HOME is not set")?;
    if off.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        };
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("json.{}", std::process::id()));
    std::fs::write(&temporary, serde_json::to_vec(off)?)?;
    std::fs::rename(&temporary, &path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })?;
    Ok(())
}
