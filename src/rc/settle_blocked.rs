//! Conversations whose finished turns wait to be saved because the daemon couldn't start its
//! saving step, so that `agit status` can say so. A daemon left unable to start its own image
//! (an update replaced it on a mount shared from a host) runs on with every save failing
//! quietly; this record is how the person hears of it. The daemon restarts into the installed
//! build once the busy turn ends, which saves them.
//!
//! Each conversation's record holds its target and when the wait began, never content. It is an
//! owner-only file of its own under AGIT_HOME, so sessions and both daemon namespaces never
//! rewrite each other's: a wait is made whole or not at all and keeps its first start, and
//! clearing one removes only that file. It is best effort (losing it hides the note, never a
//! turn), and is cleared once a save of that session runs.

use std::collections::BTreeMap;

/// Since when one conversation's turns wait.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Waiting {
    /// Seconds since the Unix epoch when the first save couldn't start.
    pub since: u64,
}

/// One wait as its file holds it.
#[derive(serde::Serialize, serde::Deserialize)]
struct Record {
    target: String,
    since: u64,
}

const NAMESPACE: &str = "settle-blocked";

/// A target's file, named by a digest of the target since a branch may hold any character.
fn file_name(target: &str) -> String {
    let digest = blake3::hash(target.as_bytes()).to_hex();
    format!("{}.json", &digest[..32])
}

/// Every conversation, by its `owner/repo@branch`, whose turns wait. Reading never makes the
/// directory, so `agit status` leaves nothing behind.
pub fn load() -> BTreeMap<String, Waiting> {
    let Ok(directory) = super::namespace_dir(NAMESPACE) else {
        return BTreeMap::new();
    };
    let Ok(entries) = std::fs::read_dir(directory) else {
        return BTreeMap::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| std::fs::read(entry.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
        .map(|record| {
            (
                record.target,
                Waiting {
                    since: record.since,
                },
            )
        })
        .collect()
}

/// Note that `target`'s save couldn't start; an earlier start of the same wait is kept. The
/// record is written whole under a name of its own and linked into place, which fails when one
/// is already there.
pub fn record(target: &str) {
    let Ok(directory) = super::state_dir(NAMESPACE) else {
        return;
    };
    let path = directory.join(file_name(target));
    if path.exists() {
        return;
    }
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let Ok(bytes) = serde_json::to_vec(&Record {
        target: target.to_owned(),
        since,
    }) else {
        return;
    };
    let temporary = directory.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    let written = crate::infra::config::state_file_options()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .and_then(|mut file| std::io::Write::write_all(&mut file, &bytes));
    if written.is_ok() {
        let _ = std::fs::hard_link(&temporary, &path);
    }
    let _ = std::fs::remove_file(&temporary);
}

/// Note that a save of `target` ran.
pub fn clear(target: &str) {
    if let Ok(directory) = super::namespace_dir(NAMESPACE) {
        let _ = std::fs::remove_file(directory.join(file_name(target)));
    }
}

#[cfg(test)]
mod tests {
    /// A wait is recorded once, keeps when it began, and ends when its conversation saves;
    /// another conversation's wait stays.
    #[test]
    fn a_wait_keeps_its_start_and_ends_when_its_conversation_saves() {
        let home = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(home.path(), || {
            super::record("alice/notes@talk");
            let first = super::load()["alice/notes@talk"].since;
            super::record("alice/notes@talk");
            super::record("alice/app@main");
            assert_eq!(super::load()["alice/notes@talk"].since, first);
            super::clear("alice/notes@talk");
            assert_eq!(super::load().keys().collect::<Vec<_>>(), ["alice/app@main"]);
        });
    }

    /// Sessions recording and clearing at once, as both daemons' sessions do, lose none of each
    /// other's waits and bring back none that was cleared. Run in a child process whose own
    /// AGIT_HOME every thread shares.
    #[test]
    fn waits_recorded_at_once_by_many_sessions_are_all_kept() {
        const CHILD: &str = "AGIT_TEST_SETTLE_BLOCKED_HOME";
        let Ok(home) = std::env::var(CHILD) else {
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "rc::settle_blocked::tests::waits_recorded_at_once_by_many_sessions_are_all_kept",
                    "--nocapture",
                ])
                .env(CHILD, home.path())
                .env("AGIT_HOME", home.path())
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed;"));
            return;
        };
        assert_eq!(std::env::var("AGIT_HOME").as_deref(), Ok(home.as_str()));
        for round in 0..20 {
            let cleared = format!("gone/{round}@main");
            super::record(&cleared);
            let threads: Vec<_> = (0..16)
                .map(|n| {
                    let cleared = cleared.clone();
                    std::thread::spawn(move || {
                        if n == 0 {
                            super::clear(&cleared);
                        }
                        super::record(&format!("alice/r{round}@s{n}"));
                    })
                })
                .collect();
            for thread in threads {
                thread.join().unwrap();
            }
            let waiting = super::load();
            for n in 0..16 {
                assert!(
                    waiting.contains_key(&format!("alice/r{round}@s{n}")),
                    "round {round}, {n}"
                );
            }
            assert!(!waiting.contains_key(&cleared), "round {round}");
        }
    }
}
