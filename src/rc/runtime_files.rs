//! Filesystem discovery keeps bounded header reads and resumable directory progress.

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BinaryHeap;
use std::path::{Component, Path, PathBuf};

const PREFIX: &str = "files:";
const CHUNK: usize = 100;

#[derive(Serialize, Deserialize)]
struct Directory {
    path: PathBuf,
    after: Option<PathBuf>,
    pending: Vec<PathBuf>,
    exhausted: bool,
    #[serde(default)]
    stamp: Option<u64>,
}

impl Directory {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            after: None,
            pending: Vec::new(),
            exhausted: false,
            stamp: None,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Sweep {
    stack: Vec<Directory>,
    reliable: bool,
}

pub(super) struct Page {
    pub threads: Vec<crate::adapter::codex_index::Thread>,
    pub next_cursor: Option<String>,
    pub reliable: bool,
}

pub(super) fn cursor(value: Option<&str>) -> bool {
    value.is_some_and(|value| value.starts_with(PREFIX))
}

fn normal(path: &Path) -> bool {
    path.components()
        .all(|part| matches!(part, Component::Normal(_)))
}

fn stamp(path: &Path) -> Option<u64> {
    std::fs::symlink_metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos()
        .try_into()
        .ok()
}

pub(super) fn page(home: &Path, after: Option<&str>, limit: usize) -> crate::Result<Page> {
    ensure!(
        (1..=500).contains(&limit),
        "invalid filesystem discovery page size"
    );
    let mut sweep = if let Some(after) = after {
        ensure!(
            after.len() <= 256 * 1024,
            "filesystem discovery cursor is too large"
        );
        let sweep: Sweep = serde_json::from_str(
            after
                .strip_prefix(PREFIX)
                .context("invalid filesystem discovery cursor")?,
        )?;
        ensure!(
            sweep.stack.len() <= 16,
            "filesystem discovery cursor is too deep"
        );
        for directory in &sweep.stack {
            ensure!(
                normal(&directory.path) && directory.path.starts_with("sessions"),
                "filesystem cursor escaped the session directory"
            );
            ensure!(
                directory.pending.len() <= CHUNK,
                "filesystem cursor has too many entries"
            );
            for name in directory.pending.iter().chain(directory.after.iter()) {
                ensure!(
                    normal(name) && name.components().count() == 1,
                    "invalid filesystem cursor entry"
                );
            }
        }
        sweep
    } else {
        Sweep {
            stack: vec![Directory::new(PathBuf::from("sessions"))],
            reliable: true,
        }
    };
    let mut threads = Vec::new();
    let mut visited = 0;
    while visited < limit && !sweep.stack.is_empty() {
        let directory = sweep.stack.last_mut().unwrap();
        if directory.pending.is_empty() {
            if directory.exhausted {
                sweep.reliable &= directory.stamp.is_some()
                    && directory.stamp == stamp(&home.join(&directory.path));
                sweep.stack.pop();
                continue;
            }
            let path = home.join(&directory.path);
            let current_stamp = stamp(&path);
            if directory.stamp.is_none() {
                directory.stamp = current_stamp;
            }
            sweep.reliable &= current_stamp.is_some() && current_stamp == directory.stamp;
            let listing = (|| -> crate::Result<(Vec<PathBuf>, bool)> {
                let metadata = std::fs::symlink_metadata(&path)?;
                ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "native directory is not a regular directory"
                );
                ensure!(
                    path.canonicalize()?.starts_with(home),
                    "native directory escaped its source"
                );
                let mut names = BinaryHeap::new();
                let mut count = 0;
                for (seen, entry) in std::fs::read_dir(&path)?.enumerate() {
                    ensure!(
                        seen < crate::adapter::native_snapshot::MAX_LOOKUP_ENTRIES,
                        "native directory enumeration budget exhausted"
                    );
                    let name = PathBuf::from(entry?.file_name());
                    if directory.after.as_ref().is_some_and(|after| &name <= after) {
                        continue;
                    }
                    count += 1;
                    names.push(name);
                    if names.len() > CHUNK {
                        names.pop();
                    }
                }
                let mut names = names.into_sorted_vec();
                names.reverse();
                Ok((names, count <= CHUNK))
            })();
            match listing {
                Ok((pending, exhausted)) => {
                    directory.pending = pending;
                    directory.exhausted = exhausted;
                    directory.after = directory
                        .pending
                        .first()
                        .cloned()
                        .or(directory.after.take());
                }
                Err(_) => {
                    sweep.reliable = false;
                    sweep.stack.pop();
                }
            }
            visited += 1;
            continue;
        }
        let name = directory.pending.pop().unwrap();
        let relative = directory.path.join(name);
        let path = home.join(&relative);
        visited += 1;
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {
                if sweep.stack.len() == 16 {
                    sweep.reliable = false;
                } else {
                    sweep.stack.push(Directory::new(relative));
                }
            }
            // Cold rollouts are listed from their compressed carrier without restoring them.
            Ok(metadata)
                if metadata.is_file() && crate::adapter::codex_rollout::is_carrier(&path) =>
            {
                match header(home, &path) {
                    Ok(thread) => threads.push(thread),
                    Err(_) => sweep.reliable = false,
                }
            }
            Ok(_) => {}
            Err(_) => sweep.reliable = false,
        }
    }
    let next_cursor = (!sweep.stack.is_empty())
        .then(|| serde_json::to_string(&sweep).map(|cursor| format!("{PREFIX}{cursor}")))
        .transpose()?;
    Ok(Page {
        threads,
        next_cursor,
        reliable: sweep.reliable,
    })
}

pub(super) fn header(
    home: &Path,
    path: &Path,
) -> crate::Result<crate::adapter::codex_index::Thread> {
    let path = path.canonicalize()?;
    ensure!(
        path.starts_with(home),
        "native transcript escaped its source"
    );
    let metadata = std::fs::metadata(&path)?;
    ensure!(
        metadata.is_file(),
        "native transcript is not a regular file"
    );
    let line = crate::adapter::codex_rollout::first_line(
        &path,
        crate::adapter::codex_rollout::HEADER_LIMIT,
    )?;
    ensure!(line.ends_with(b"\n"), "native header is incomplete");
    let record: serde_json::Value = serde_json::from_slice(&line)?;
    ensure!(record["type"] == "session_meta", "native header is invalid");
    let payload = &record["payload"];
    let id = payload["id"]
        .as_str()
        .context("native header has no identity")?;
    uuid::Uuid::parse_str(id)?;
    let cwd = payload["cwd"]
        .as_str()
        .context("native header has no directory")?;
    ensure!(
        Path::new(cwd).is_absolute(),
        "native directory must be absolute"
    );
    let updated_at_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_millis()).ok());
    Ok(crate::adapter::codex_index::Thread {
        id: id.into(),
        // The plain name, as Codex's index names a cold rollout; locating restores it.
        rollout_path: crate::adapter::codex_rollout::plain_path(&path),
        cwd: Some(cwd.into()),
        gist: None,
        thread_source: None,
        source: payload.get("source").map(serde_json::Value::to_string),
        updated_at_ms,
        title: None,
        name: None,
    })
}

pub(super) fn locate(
    home: &Path,
    native: &str,
) -> crate::Result<crate::adapter::codex_index::Thread> {
    let source =
        crate::adapter::native_snapshot::lookup_codex_rollout_in(home, native, Default::default())?;
    let thread = header(home, &source.path)?;
    ensure!(
        thread.id == native,
        "native transcript identifies another conversation"
    );
    Ok(thread)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Most dormant rollouts are compressed, and real headers outgrow a small read. A sweep
    /// that skipped compressed carriers would report complete coverage without them and the
    /// catalog would delete their rows; one that restored them would decode every dormant
    /// transcript on each sweep. While both carriers of one rollout exist, it is listed once.
    #[test]
    fn discovery_lists_cold_rollouts_once_without_restoring_them() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().canonicalize().unwrap();
        let directory = home.join("sessions/2026/10/01");
        std::fs::create_dir_all(&directory).unwrap();
        let rollout = |id: &str| {
            let header = serde_json::json!({"type":"session_meta","payload":{
                "id":id,"cwd":home,"base_instructions":{"text":"x".repeat(64 * 1024)}}});
            format!("{header}\n{{\"type\":\"event_msg\"}}\n").into_bytes()
        };
        let compress = |bytes: &[u8]| {
            ruzstd::encoding::compress_to_vec(bytes, ruzstd::encoding::CompressionLevel::Fastest)
        };
        let cold = uuid::Uuid::from_u128(1).to_string();
        let plain = directory.join(format!("rollout-2026-10-01T00-00-00-{cold}.jsonl"));
        std::fs::write(
            directory.join(format!("rollout-2026-10-01T00-00-00-{cold}.jsonl.zst")),
            compress(&rollout(&cold)),
        )
        .unwrap();
        let both = uuid::Uuid::from_u128(2).to_string();
        let name = format!("rollout-2026-10-01T00-00-00-{both}.jsonl");
        std::fs::write(directory.join(&name), rollout(&both)).unwrap();
        std::fs::write(
            directory.join(format!("{name}.zst")),
            compress(&rollout(&both)),
        )
        .unwrap();
        let result = page(&home, None, 100).unwrap();
        assert!(result.next_cursor.is_none() && result.reliable);
        let mut threads: Vec<_> = result
            .threads
            .into_iter()
            .map(|thread| (thread.id, thread.rollout_path))
            .collect();
        threads.sort();
        assert_eq!(
            threads,
            [(cold, plain.clone()), (both, directory.join(name))]
        );
        assert!(!plain.exists());
    }

    #[test]
    fn discovery_resumes_across_directory_pages_and_retains_incomplete_coverage() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().canonicalize().unwrap();
        let directory = home.join("sessions/2026/09/26");
        std::fs::create_dir_all(&directory).unwrap();
        let mut expected = std::collections::BTreeSet::new();
        for index in 0..205 {
            let id = uuid::Uuid::from_u128(index + 1).to_string();
            let path = directory.join(format!("rollout-{id}.jsonl"));
            let mut contents = serde_json::to_vec(&serde_json::json!({"type":"session_meta","payload":{"id":id,"cwd":home,"source":"cli"}})).unwrap();
            contents.extend_from_slice(b"\n\xffinvalid transcript tail");
            std::fs::write(path, contents).unwrap();
            expected.insert(id);
        }
        std::fs::write(directory.join("incomplete.jsonl"), "{\"type\":").unwrap();
        let mut cursor = None;
        let mut found = std::collections::BTreeSet::new();
        let mut pages = 0;
        loop {
            let result = page(&home, cursor.as_deref(), 17).unwrap();
            assert!(result.threads.len() <= 17);
            for thread in result.threads {
                assert!(
                    found.insert(thread.id),
                    "a resumed sweep duplicated a transcript"
                );
            }
            pages += 1;
            cursor = result.next_cursor;
            if cursor.is_none() {
                assert!(
                    !result.reliable,
                    "an incomplete native header cannot establish complete coverage"
                );
                break;
            }
            assert!(pages < 100, "filesystem cursor did not converge");
        }
        assert_eq!(found, expected);
        let id = expected.first().unwrap();
        assert_eq!(locate(&home, id).unwrap().id, *id);
        assert!(page(&home, Some("files:{\"stack\":[{\"path\":\"../other\",\"pending\":[],\"after\":null,\"exhausted\":false}],\"reliable\":true}"), 17).is_err());
    }
}
