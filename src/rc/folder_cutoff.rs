//! The conversations a folder already held when the person turned its uploads on: when `agit rc
//! start --here` bound it as a project, or when `agit on` turned it back on after `agit off`.
//!
//! The daemon takes a terminal conversation of a bound folder in once someone opens it on the web.
//! A conversation that existed before the person said yes stays out, later turns included, until
//! the person saves it themselves (`agit save`). The record lives in the daemon's folder, one file
//! per canonical folder, readable by its owner only. `agit off` keeps it; it goes only with the
//! folder's binding.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    /// The canonical folder, so a record is never applied to another one by a hash collision.
    root: String,
    /// `runtime:session-id`, as a folder's project policy keys its sessions.
    sessions: BTreeSet<String>,
}

fn key(runtime: &str, native: &str) -> String {
    format!("{runtime}:{native}")
}

fn canonical(root: &Path) -> Result<PathBuf> {
    root.canonicalize()
        .with_context(|| format!("cannot resolve {}", root.display()))
}

fn path(root: &Path) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let digest = hex::encode(Sha256::digest(root.to_string_lossy().as_bytes()));
    Ok(super::local_rc_dir()?
        .join("folder-cutoffs")
        .join(format!("{}.json", &digest[..24])))
}

fn read(root: &Path) -> Result<Option<Record>> {
    let path = path(root)?;
    let body = match std::fs::read(&path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("cannot read the folder's cutoff record"),
    };
    let record: Record =
        serde_json::from_slice(&body).context("the folder's cutoff record is unreadable")?;
    anyhow::ensure!(
        record.version == 1 && record.root == root.to_string_lossy(),
        "the folder's cutoff record names another folder"
    );
    Ok(Some(record))
}

fn directory(root: &Path) -> Result<PathBuf> {
    let path = path(root)?;
    let directory = path.parent().context("cutoff record has no folder")?;
    #[cfg(not(windows))]
    crate::infra::config::create_state_dir(directory)?;
    #[cfg(windows)]
    std::fs::create_dir_all(directory)?;
    Ok(directory.to_owned())
}

/// Holds `root`'s record for one read, change and write, across processes: `agit on` adding
/// conversations and `agit save` letting one in would otherwise each write back what the other
/// did not see, and a conversation could lose its place in the record.
fn locked<T>(root: &Path, change: impl FnOnce() -> Result<T>) -> Result<T> {
    directory(root)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path(root)?.with_extension("lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    change()
}

fn write(root: &Path, record: &Record) -> Result<()> {
    let path = path(root)?;
    let directory = directory(root)?;
    let body = serde_json::to_vec(record)?;
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    std::io::Write::write_all(temporary.as_file_mut(), &body)?;
    temporary.as_file().sync_all()?;
    temporary.persist(&path)?;
    Ok(())
}

/// Records `sessions` (`runtime:session-id`) as the conversations `root` held at the yes, beside
/// any recorded at an earlier one.
pub(crate) fn record(root: &Path, sessions: impl IntoIterator<Item = String>) -> Result<()> {
    let root = canonical(root)?;
    locked(&root, || {
        let mut kept = read(&root)?
            .map(|record| record.sessions)
            .unwrap_or_default();
        kept.extend(sessions);
        write(
            &root,
            &Record {
                version: 1,
                root: root.to_string_lossy().into_owned(),
                sessions: kept,
            },
        )
    })
}

/// Whether the conversation `native` of `runtime` stays out of `root`'s uploads: it was there
/// before the yes. A record that can't be read keeps every conversation of the folder out.
pub(crate) fn leaves_out(root: &Path, runtime: &str, native: &str) -> bool {
    let Ok(root) = canonical(root) else {
        return false;
    };
    match read(&root) {
        Ok(Some(record)) => record.sessions.contains(&key(runtime, native)),
        Ok(None) => false,
        Err(_) => true,
    }
}

/// [`leaves_out`] for a conversation recorded in `repo`, judged by every folder whose
/// conversations that device-local repository holds: what a publication knows is the repository,
/// not the folder. Such a conversation is held back as if automatic upload were off for it: it
/// stays on this computer, and saving it lets it go. When the folders can't be read, every
/// conversation of the repository is held back.
pub(crate) fn holds_back(repo: &crate::domain::repo::Repo, runtime: &str, native: &str) -> bool {
    match super::local_repository::folders_of(repo) {
        Ok(folders) => folders
            .iter()
            .any(|folder| leaves_out(folder, runtime, native)),
        Err(_) => true,
    }
}

/// The person saved this conversation, which ran in `cwd`, themselves: from now on it is taken in
/// like any other, in every bound folder that holds `cwd`.
pub(crate) fn admit(cwd: &Path, runtime: &str, native: &str) -> Result<()> {
    let cwd = canonical(cwd)?;
    let directory = super::local_rc_dir()?.join("folder-cutoffs");
    let entries = match std::fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("cannot read the folders' cutoff records"),
    };
    for entry in entries {
        let entry = entry?.path();
        if entry
            .extension()
            .is_none_or(|extension| extension != "json")
        {
            continue;
        }
        let body = std::fs::read(entry)?;
        let Ok(record) = serde_json::from_slice::<Record>(&body) else {
            continue;
        };
        let root = PathBuf::from(&record.root);
        if !cwd.starts_with(&root) {
            continue;
        }
        locked(&root, || {
            let Some(mut record) = read(&root)? else {
                return Ok(());
            };
            if record.sessions.remove(&key(runtime, native)) {
                write(&root, &record)?;
            }
            Ok(())
        })?;
    }
    Ok(())
}

/// The folder's binding went: its record goes with it, so it can't outlive it.
pub(crate) fn clear(root: &Path) -> Result<()> {
    let root = canonical(root)?;
    locked(&root, || match std::fs::remove_file(path(&root)?) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `agit on` adding conversations and `agit save` letting others in, at the same time, lose
    /// none of each other's changes. Each writer holds the folder's record for its whole read,
    /// change and write; one that wrote back what it read earlier would drop what another added.
    #[test]
    fn concurrent_changes_to_a_folders_record_lose_nothing() {
        if crate::rc::in_isolated_test(
            "rc::folder_cutoff::tests::concurrent_changes_to_a_folders_record_lose_nothing",
        ) {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("AGIT_HOME", home.path()) };
        let folder = tempfile::tempdir().unwrap();
        let root = folder.path().to_owned();
        let saved: Vec<String> = (0..40).map(|n| format!("saved-{n}")).collect();
        record(&root, saved.iter().map(|id| format!("codex:{id}"))).unwrap();
        let start = std::sync::Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|worker| {
                let (root, start, saved) = (root.clone(), start.clone(), saved.clone());
                std::thread::spawn(move || {
                    start.wait();
                    for n in 0..5 {
                        if worker % 2 == 0 {
                            record(&root, [format!("codex:kept-{worker}-{n}")]).unwrap();
                        } else {
                            admit(&root, "codex", &saved[worker * 5 + n]).unwrap();
                        }
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        for worker in (0..8).step_by(2) {
            for n in 0..5 {
                assert!(
                    leaves_out(&root, "codex", &format!("kept-{worker}-{n}")),
                    "a conversation lost its place in the record"
                );
            }
        }
        for worker in (1..8).step_by(2) {
            for n in 0..5 {
                assert!(
                    !leaves_out(&root, "codex", &saved[worker * 5 + n]),
                    "a saved conversation came back"
                );
            }
        }
    }

    /// A conversation the folder held at the yes stays out, and only that one; saving it, in the
    /// folder or below it, lets it in; the binding going clears the record; an unreadable record
    /// keeps everything out.
    #[test]
    fn a_folders_earlier_conversations_stay_out_until_saved_or_unbound() {
        let home = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(home.path(), || {
            let folder = tempfile::tempdir().unwrap();
            let root = folder.path();
            assert!(!leaves_out(root, "claude-code", "old"));
            record(root, ["claude-code:old".to_owned()]).unwrap();
            assert!(leaves_out(root, "claude-code", "old"));
            assert!(!leaves_out(root, "claude-code", "new"));
            assert!(!leaves_out(root, "codex", "old"), "keyed by runtime too");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(path(&canonical(root).unwrap()).unwrap())
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600);
            }
            let below = root.join("below");
            std::fs::create_dir(&below).unwrap();
            admit(&below, "claude-code", "old").unwrap();
            assert!(!leaves_out(root, "claude-code", "old"));
            record(root, ["claude-code:old".to_owned()]).unwrap();
            clear(root).unwrap();
            assert!(!leaves_out(root, "claude-code", "old"));
            record(root, ["claude-code:old".to_owned()]).unwrap();
            std::fs::write(path(&canonical(root).unwrap()).unwrap(), b"not json").unwrap();
            assert!(leaves_out(root, "claude-code", "anything"), "fails closed");
        });
    }
}
