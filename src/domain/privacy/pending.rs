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

fn path(file: &str) -> Option<std::path::PathBuf> {
    crate::infra::config::agit_home()
        .ok()
        .map(|home| home.join(file))
}

fn read<T: serde::de::DeserializeOwned + Default>(file: &str) -> T {
    path(file)
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Every session that waits.
pub fn load() -> BTreeMap<String, Waiting> {
    read(FILE)
}

fn write<T: serde::Serialize>(file: &str, waiting: &T) {
    let Some(path) = path(file) else {
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
        .unwrap_or_else(now);
    waiting.insert(
        session.to_owned(),
        Waiting {
            reason: reason.to_owned(),
            turns,
            since,
            refused_bytes,
        },
    );
    write(FILE, &waiting);
}

/// Note that `session` settled what it had.
pub fn clear(session: &str) {
    let mut waiting = load();
    if waiting.remove(session).is_some() {
        write(FILE, &waiting);
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The steps that end a wait for `reason`, worded for the person who reads it, or `None` when the
/// wait ends by itself. A reason about this device's rules or about the content does not end by
/// waiting, so it names what the person can do; any other reason is the worker's state, and the
/// next attempt retries.
/// Whether a wait's reason says the privacy worker could not run at all: it did not start, broke,
/// or is held back after either. Such a wait ends by itself on a later attempt; one that keeps
/// recurring points at a fault `agit doctor` names.
pub fn worker_unavailable(reason: &str) -> bool {
    matches!(reason, "spawn_failed" | "backoff" | "protocol_error")
}

/// What to do when a [`worker_unavailable`] wait keeps recurring.
pub const WORKER_UNAVAILABLE_STEP: &str = "if it keeps failing, run `agit doctor`";

pub fn steps(reason: &str) -> Option<Vec<String>> {
    match reason {
        "device_rules" => Some(super::service::unreadable_rule_hints().unwrap_or_else(|_| {
            vec!["run `agit doctor` to see which privacy rules cannot be read".into()]
        })),
        "legacy_rules" => Some(vec![
            "an older secret list cannot be read whole, so a value it blocks could pass; run `agit doctor` to see what cannot be read".into(),
        ]),
        "oversize" => Some(vec![
            "a record in this conversation is too large to check, so it can't be published; it stays on this computer. To stop uploading this conversation, and only it, run `agit project drop` inside it; its local history stays".into(),
        ]),
        "findings_cap" => Some(vec![
            "a record holds more secret values than one check replaces, so waiting does not end this; `agit scan` shows where they are".into(),
        ]),
        _ => None,
    }
}

/// Uploads that wait for the privacy check, by the `owner/repo@branch` they push: nothing of such
/// a push was sent. An automatic upload has nobody watching it, so `agit status` is where its wait
/// shows. The record is best effort, like the settlement record.
pub mod upload {
    use std::collections::BTreeMap;

    const FILE: &str = "publication-pending.json";

    /// Why one target's upload waits, and since when.
    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub struct Waiting {
        pub reason: String,
        /// Seconds since the Unix epoch when the wait began.
        pub since: u64,
    }

    /// Every target whose upload waits.
    pub fn load() -> BTreeMap<String, Waiting> {
        super::read(FILE)
    }

    /// Note that `target`'s upload waits; an earlier start of the same wait is kept.
    pub fn record(target: &str, reason: &str) {
        let mut waiting = load();
        let since = waiting
            .get(target)
            .map(|entry| entry.since)
            .unwrap_or_else(super::now);
        waiting.insert(
            target.to_owned(),
            Waiting {
                reason: reason.to_owned(),
                since,
            },
        );
        super::write(FILE, &waiting);
    }

    /// Note that `branch` of the repository known here as any of `repositories` uploaded.
    pub fn clear_uploaded(repositories: &[&str], branch: &str) {
        let mut waiting = load();
        let before = waiting.len();
        waiting.retain(|target, _| !uploaded(target, repositories, branch));
        if waiting.len() != before {
            super::write(FILE, &waiting);
        }
    }

    /// Whether the wait recorded for `target` ends when `branch` of the repository known here as
    /// any of `repositories` lands. A wait is recorded under the target the push was asked for,
    /// which for a device-local checkout is its local name while the Hub's destination has
    /// another, so either name ends it. Matched by the destination's name alone, such a wait is
    /// never cleared, and status lists it after the upload landed.
    fn uploaded(target: &str, repositories: &[&str], branch: &str) -> bool {
        target.split_once('@').is_some_and(|(repository, waited)| {
            waited == branch && repositories.contains(&repository)
        })
    }

    #[cfg(test)]
    mod tests {
        /// A wait recorded under a device-local checkout's name ends when its branch lands under
        /// the Hub destination's name; another branch's wait, and another repository's, stay.
        #[test]
        fn a_wait_ends_when_its_branch_lands_under_either_name() {
            let known = ["alice/notes", "local/project-1"];
            assert!(super::uploaded("local/project-1@talk", &known, "talk"));
            assert!(super::uploaded("alice/notes@talk", &known, "talk"));
            assert!(!super::uploaded("local/project-1@other", &known, "talk"));
            assert!(!super::uploaded("alice/app@talk", &known, "talk"));
        }
    }
}
