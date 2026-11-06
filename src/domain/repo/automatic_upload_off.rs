//! Destinations whose automatic upload the person turned off on this computer before their
//! checkout existed here.
//!
//! A destination's own `push.auto` lives in its checkout, which the first saved conversation
//! creates. Until then this record holds the person's off, and it keeps holding it for a checkout
//! created later without a setting of its own, so no creation path can inherit around it. It only
//! ever says "off": nothing here can turn an upload on.
//!
//! Each entry is keyed by the Hub and the destination, with the owner and the name folded to ASCII
//! lowercase, since a destination's name reaches it in any case. The file is private and written
//! atomically. A record that cannot be read counts as off for every destination without a setting
//! of its own, so a damaged file stops uploads rather than letting them resume.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;

const FILE: &str = "automatic-upload-off.json";
const LOCK: &str = "automatic-upload-off.lock";
const MAX_BYTES: usize = 1 << 20;

/// What this computer records about a destination's automatic upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    Off,
    Unset,
    /// The record could not be read, so whether the person turned it off is unknown.
    Unreadable,
}

impl Recorded {
    /// Whether an upload into the destination stays off on this record alone.
    pub fn holds_off(self) -> bool {
        self != Self::Unset
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    version: u32,
    off: BTreeSet<String>,
}

fn path() -> Result<PathBuf> {
    Ok(crate::infra::config::agit_home()?
        .join("preferences")
        .join(FILE))
}

fn key(hub: &str, owner: &str, name: &str) -> Result<String> {
    Ok(format!(
        "{}/{}/{}",
        crate::infra::config::hub_host_key(hub)?,
        owner.to_ascii_lowercase(),
        name.to_ascii_lowercase()
    ))
}

fn load() -> Result<Body> {
    let path = path()?;
    match crate::domain::privacy::storage::read_private(&path, MAX_BYTES) {
        Ok(bytes) => {
            let body: Body = serde_json::from_slice(&bytes)
                .with_context(|| format!("{} is malformed", path.display()))?;
            anyhow::ensure!(
                body.version == 1,
                "{} has an unknown version",
                path.display()
            );
            Ok(body)
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(Body {
                version: 1,
                off: BTreeSet::new(),
            })
        }
        Err(error) => Err(error.context(format!("{} cannot be read", path.display()))),
    }
}

/// Whether `owner/name` on the current Hub is recorded as off, or why the record can't say.
pub fn read(owner: &str, name: &str) -> Result<bool> {
    let key = key(&crate::infra::config::hub_url(), owner, name)?;
    Ok(load()?.off.contains(&key))
}

/// The record for `owner/name` on the current Hub, for a decision that must not wait on it.
pub fn recorded(owner: &str, name: &str) -> Recorded {
    match read(owner, name) {
        Ok(true) => Recorded::Off,
        Ok(false) => Recorded::Unset,
        Err(error) => {
            warn_unreadable(&error);
            Recorded::Unreadable
        }
    }
}

/// Records `owner/name` on the current Hub as off, or with `off` false drops the record.
///
/// The read, the change and the write happen under one lock on a file beside the record, which
/// the atomic replace never swaps: without it, two commands changing different destinations at
/// once each write the list they read, and the first one's change is lost.
pub fn set(owner: &str, name: &str, off: bool) -> Result<()> {
    let key = key(&crate::infra::config::hub_url(), owner, name)?;
    let record = path()?;
    let directory = record.parent().context("the record needs a directory")?;
    crate::domain::privacy::storage::private_directory(directory)?;
    let lock_path = directory.join(LOCK);
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("{} cannot be opened", lock_path.display()))?;
    crate::infra::local_state::lock_exclusive(&lock, &lock_path, "the automatic upload record")?;
    let mut body = load()?;
    let changed = if off {
        body.off.insert(key)
    } else {
        body.off.remove(&key)
    };
    if changed {
        crate::domain::privacy::storage::write_private(&record, &serde_json::to_vec(&body)?)?;
    }
    Ok(())
}

fn warn_unreadable(error: &anyhow::Error) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        #[cfg(feature = "cli")]
        crate::ui::warning(&format!(
            "automatic upload stays off for repositories without a setting of their own: the record of which ones you turned off can't be read ({error:#})"
        ));
        #[cfg(not(feature = "cli"))]
        let _ = error;
    });
}
