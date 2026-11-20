//! The messages a conversation's session holds for its next turn, kept on this device so a
//! daemon restart doesn't lose them.
//!
//! A message waits for the next turn when it reaches a running conversation that can't take it at
//! a tool step: an owner's that its turn ended without, a colleague's that waits for the turn to
//! end. The session keeps each one with the authority of the request that brought it, and that
//! authority lives only as long as the request's connection. A file can't hold it, so a message
//! read back here after a restart carries no authority of its own: it goes only once its sender's
//! next request to the conversation is admitted, under that request's authority (see the
//! supervisor's `steer_queue`).
//!
//! Per conversation, one owner-only file in the running daemon's own state directory under the
//! account's AGIT_HOME, outside every capture path, holds the conversation's waiting messages in
//! order: the words, the sender's
//! account and name, the client message id and when it arrived. It is rewritten whole, through a
//! temporary file and a rename, at every change, and removed once nothing waits. It is never
//! committed, uploaded, previewed or logged. A message that hasn't gone a day after it arrived is
//! dropped and its sender told; logout or a sign-in under another account removes every file, and
//! the daemon's start removes those of conversations it no longer runs. Each daemon (the desktop
//! one and the cloud one) keeps its own: a start judges files only by its own roster, and judging
//! the other daemon's would delete messages it still keeps.

use serde::{Deserialize, Serialize};
use std::{io::Write, path::PathBuf};

const NAMESPACE: &str = "waiting-messages";

/// The daemons whose state directories may hold waiting messages.
const DAEMONS: [&str; 2] = ["rc", super::LOCAL_NAMESPACE];

/// The running daemon's own state directory, as [`super::rc_dir`] chooses it.
fn this_daemon() -> &'static str {
    if super::LOCAL_AUTHORITY.load(std::sync::atomic::Ordering::Acquire) {
        super::LOCAL_NAMESPACE
    } else {
        "rc"
    }
}

fn store(daemon: &str) -> String {
    format!("{daemon}/{NAMESPACE}")
}

/// How long a message read back after a restart waits for its sender to come back.
pub const MAX_AGE_MS: i64 = 24 * 60 * 60 * 1000;

/// One waiting message as the file keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stored {
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) attachments: Vec<super::input_attachments::Reference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_msg_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<crate::protocol::MessageSender>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// When it arrived, in wall-clock milliseconds.
    pub arrived_ms: i64,
}

impl Stored {
    pub fn expired(&self, now_ms: i64) -> bool {
        now_ms - self.arrived_ms >= MAX_AGE_MS
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    version: u32,
    waiting: Vec<Stored>,
}

const VERSION: u32 = 2;

/// A conversation id is a file name here only when it is a plain id.
fn file(directory: PathBuf, conversation: &str) -> Option<PathBuf> {
    (!conversation.is_empty()
        && conversation.len() <= 128
        && conversation
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    .then(|| directory.join(format!("{conversation}.json")))
}

/// Keeps `waiting` as what waits in `conversation`, in order; nothing waiting removes the file.
pub fn save(conversation: &str, waiting: &[Stored]) -> crate::Result<()> {
    save_in(this_daemon(), conversation, waiting)
}

fn save_in(daemon: &str, conversation: &str, waiting: &[Stored]) -> crate::Result<()> {
    migrate_legacy_in(daemon, conversation)?;
    let root = super::namespace_dir(daemon)?.join("task-attachments");
    let references: Vec<_> = waiting
        .iter()
        .flat_map(|item| item.attachments.iter().cloned())
        .collect();
    super::input_attachments::update_waiting(&root, conversation, &references, || {
        persist_in(daemon, conversation, waiting)
    })
}

pub(super) fn persist_in(
    daemon: &str,
    conversation: &str,
    waiting: &[Stored],
) -> crate::Result<()> {
    if waiting.is_empty() {
        return forget_in(daemon, conversation);
    }
    let path = file(super::state_dir(&store(daemon))?, conversation)
        .ok_or_else(|| anyhow::anyhow!("the conversation id can't name a waiting-message file"))?;
    let directory = path.parent().unwrap_or(std::path::Path::new("."));
    // A temporary file is created owner-only, and the rename replaces the file whole, so a crash
    // leaves the previous list or this one, never a part of either.
    let mut temporary = tempfile::Builder::new()
        .prefix(".waiting-")
        .tempfile_in(directory)?;
    serde_json::to_writer(
        &mut temporary,
        &File {
            version: VERSION,
            waiting: waiting.to_vec(),
        },
    )?;
    temporary.flush()?;
    temporary.as_file().sync_data()?;
    temporary.persist(&path).map_err(|error| error.error)?;
    #[cfg(unix)]
    std::fs::File::open(directory)?.sync_all()?;
    Ok(())
}

/// What waits in `conversation`, in order. A file that is missing, unreadable or of another
/// version holds nothing: losing one only ever drops messages, never sends one twice.
pub fn load(conversation: &str) -> Vec<Stored> {
    load_in(this_daemon(), conversation)
}

fn load_in(daemon: &str, conversation: &str) -> Vec<Stored> {
    migrate_legacy_in(daemon, conversation)
        .and_then(|()| load_checked_in(daemon, conversation))
        .unwrap_or_default()
}

fn migrate_legacy_in(daemon: &str, conversation: &str) -> crate::Result<()> {
    if !read_record(daemon, conversation)?.is_some_and(|record| record.version == 1) {
        return Ok(());
    }
    let root = super::namespace_dir(daemon)?.join("task-attachments");
    super::input_attachments::migrate_legacy_waiting(&root, conversation, || {
        let Some(record) = read_record(daemon, conversation)? else {
            return Ok(());
        };
        if record.version == VERSION {
            return Ok(());
        }
        anyhow::ensure!(
            record.version == 1
                && record
                    .waiting
                    .iter()
                    .all(|stored| stored.attachments.is_empty()
                        && !stored.message.contains("owned-v1")),
            "legacy waiting record has uncertain managed references"
        );
        persist_in(daemon, conversation, &record.waiting)
    })
}

fn read_record(daemon: &str, conversation: &str) -> crate::Result<Option<File>> {
    let path = file(super::namespace_dir(&store(daemon))?, conversation)
        .ok_or_else(|| anyhow::anyhow!("invalid waiting conversation identity"))?;
    super::cloud::store::read(&path, 8 * 1024 * 1024)
}

/// An unreadable or legacy waiting record cannot prove that managed references are gone.
pub(super) fn load_checked_in(daemon: &str, conversation: &str) -> crate::Result<Vec<Stored>> {
    let Some(record) = read_record(daemon, conversation)? else {
        return Ok(Vec::new());
    };
    anyhow::ensure!(
        record.version == VERSION,
        "unsupported attachment reference version"
    );
    for reference in record.waiting.iter().flat_map(|item| &item.attachments) {
        reference.validate()?;
    }
    Ok(record.waiting)
}

fn forget_in(daemon: &str, conversation: &str) -> crate::Result<()> {
    let Some(path) = file(super::namespace_dir(&store(daemon))?, conversation) else {
        return Ok(());
    };
    match std::fs::remove_file(&path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        Err(_) => return Ok(()),
        Ok(()) => {}
    }
    #[cfg(unix)]
    std::fs::File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}

/// Removes every file, every daemon's: the account the messages came under is leaving this
/// device.
pub fn forget_all() {
    for daemon in DAEMONS {
        let Ok(directory) = super::namespace_dir(&store(daemon)) else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(conversation) = name.to_str().and_then(|name| name.strip_suffix(".json"))
            else {
                continue;
            };
            let root = match super::namespace_dir(daemon) {
                Ok(directory) => directory.join("task-attachments"),
                Err(_) => continue,
            };
            let result = if root.join("owned-v1").try_exists().unwrap_or(true) {
                super::input_attachments::forget_unowned_waiting(&root, daemon, conversation)
            } else {
                forget_in(daemon, conversation)
            };
            if result.is_err() {
                eprintln!("agit: waiting-message cleanup is incomplete");
            }
        }
    }
}

/// Removes the running daemon's files of conversations it no longer runs, and of ones where
/// every message is past its age: no session would ever send them.
pub fn sweep(runs: impl Fn(&str) -> bool, now_ms: i64) {
    sweep_in(this_daemon(), runs, now_ms)
}

fn sweep_in(daemon: &str, runs: impl Fn(&str) -> bool, now_ms: i64) {
    let Ok(entries) = super::namespace_dir(&store(daemon))
        .and_then(|directory| std::fs::read_dir(directory).map_err(Into::into))
    else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let conversation = name.to_str().and_then(|name| name.strip_suffix(".json"));
        let Some(conversation) = conversation else {
            continue;
        };
        let Ok(waiting) = load_checked_in(daemon, conversation) else {
            // Keep uncertain references until an explicit durable transition retires them.
            continue;
        };
        let keep = runs(conversation) && waiting.iter().any(|stored| !stored.expired(now_ms));
        if !keep {
            let _ = save_in(daemon, conversation, &[]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONVERSATION: &str = "4e6c2b1a-0d0e-4c43-9a51-8f0e2f4a7b10";

    fn stored(message: &str, arrived_ms: i64) -> Stored {
        Stored {
            message: message.into(),
            attachments: Vec::new(),
            client_msg_id: Some(message.into()),
            sender: Some(crate::protocol::MessageSender {
                account_id: "member".into(),
                username: "m".into(),
            }),
            by: None,
            arrived_ms,
        }
    }

    /// The file is the owner's alone, in a directory only the owner can enter, and it holds the
    /// list in order; an empty list leaves no file, a sweep keeps only conversations its own
    /// daemon still runs with a message not past its age, and leaves the other daemon's files
    /// alone; leaving the account removes every daemon's. A file written in place, or readable by
    /// others, would put a colleague's words where any process of any user on the device could
    /// read them; a sweep across daemons would delete what the other one keeps, unsaid.
    #[test]
    fn the_waiting_list_is_owner_only_kept_in_order_and_goes_with_its_account() {
        let home = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(home.path(), || {
            let now = 10 * MAX_AGE_MS;
            let list = [stored("first", now), stored("second", now - 1)];
            save(CONVERSATION, &list).unwrap();
            assert_eq!(load(CONVERSATION), list);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let directory = super::super::namespace_dir(&store("rc")).unwrap();
                let mode = |path: &std::path::Path| {
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
                };
                assert_eq!(mode(&directory), 0o700);
                assert_eq!(mode(&directory.join(format!("{CONVERSATION}.json"))), 0o600);
                let names: Vec<_> = std::fs::read_dir(&directory)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect();
                assert_eq!(names.len(), 1, "no temporary file stays: {names:?}");
            }
            save(CONVERSATION, &[]).unwrap();
            assert!(load(CONVERSATION).is_empty());

            let gone = "11111111-2222-4333-8444-555555555555";
            let stale = "21111111-2222-4333-8444-555555555555";
            save(CONVERSATION, &list).unwrap();
            save(gone, &list).unwrap();
            save(stale, &[stored("old", now - MAX_AGE_MS)]).unwrap();
            sweep(|conversation| conversation != gone, now);
            assert_eq!(load(CONVERSATION), list);
            assert!(load(gone).is_empty());
            assert!(load(stale).is_empty());

            // The desktop daemon runs none of the cloud daemon's conversations.
            sweep_in(crate::rc::LOCAL_NAMESPACE, |_| false, now);
            assert_eq!(load_in("rc", CONVERSATION), list);

            let path = file(super::super::namespace_dir(&store("rc")).unwrap(), gone).unwrap();
            std::fs::write(&path, b"{broken").unwrap();
            assert!(load_checked_in("rc", gone).is_err());
            sweep(|_| false, now);
            assert!(
                path.is_file(),
                "an incomplete reference read must not authorize removal"
            );
            std::fs::write(&path, br#"{"version":999,"waiting":[]}"#).unwrap();
            assert!(load_checked_in("rc", gone).is_err());
            sweep(|_| false, now);
            assert!(path.is_file());
            assert!(save(gone, &[]).is_err());
            persist_in("rc", gone, &[]).unwrap();
            assert!(load_checked_in("rc", gone).unwrap().is_empty());

            save(CONVERSATION, &list).unwrap();
            save_in(crate::rc::LOCAL_NAMESPACE, CONVERSATION, &list).unwrap();
            forget_all();
            assert!(load_in("rc", CONVERSATION).is_empty());
            assert!(load_in(crate::rc::LOCAL_NAMESPACE, CONVERSATION).is_empty());
        });
    }
}
