//! Sessions whose settlement waits for the privacy check, so that `agit status` can say so: a
//! settlement started by a hook runs quietly, and a wait nobody sees could last for as long as its
//! cause does. The record is best effort; losing it hides the note, never content.

use std::collections::BTreeMap;

const FILE: &str = "privacy-pending.json";

/// Why one session's settlement waits, and since when.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Waiting {
    pub reason: String,
    pub turns: usize,
    /// Seconds since the Unix epoch when the wait began.
    pub since: u64,
    /// The size of a record the privacy check cannot divide. Such a wait does not end by
    /// itself: settling again meets the same record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused_bytes: Option<u64>,
}

fn path() -> Option<std::path::PathBuf> {
    crate::infra::config::agit_home()
        .ok()
        .map(|home| home.join(FILE))
}

/// Every session that waits.
pub fn load() -> BTreeMap<String, Waiting> {
    path()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn store(waiting: &BTreeMap<String, Waiting>) {
    let Some(path) = path() else {
        return;
    };
    let Ok(bytes) = serde_json::to_vec(waiting) else {
        return;
    };
    let temporary = path.with_extension(format!("json.{}", std::process::id()));
    if std::fs::write(&temporary, bytes).is_ok() && std::fs::rename(&temporary, &path).is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
}

/// Note that `session`'s settlement waits; an earlier start of the same wait is kept.
pub fn record(session: &str, reason: &str, turns: usize, refused_bytes: Option<u64>) {
    let mut waiting = load();
    let since = waiting
        .get(session)
        .map(|entry| entry.since)
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs())
        });
    waiting.insert(
        session.to_owned(),
        Waiting {
            reason: reason.to_owned(),
            turns,
            since,
            refused_bytes,
        },
    );
    store(&waiting);
}

/// Note that `session` settled what it had.
pub fn clear(session: &str) {
    let mut waiting = load();
    if waiting.remove(session).is_some() {
        store(&waiting);
    }
}
