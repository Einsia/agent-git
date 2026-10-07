//! Spill control lifecycle metadata without copying messages out of the native transcript.

use anyhow::{Context, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

const MAX_EVENTS: usize = 1024;
const MAX_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    #[serde(default)]
    events: Vec<Value>,
    ack_seq: Option<u64>,
}

#[derive(Serialize)]
pub(crate) struct Reply {
    events: Vec<Value>,
}

pub(crate) fn exchange(session: &str, generation: &str) -> crate::Result<Reply> {
    let request: Request = serde_json::from_reader(std::io::stdin().take(MAX_BYTES))?;
    let (_, parent) =
        super::process(std::process::id()).context("cannot verify the native event caller")?;
    let registration =
        super::read_registration(&super::directory()?.join(format!("{parent}.json")))?;
    // A delayed spool request cannot register an old generation after a native session switch.
    ensure!(
        registration.session == session
            && registration.generation == generation
            && registration.process.pid == parent
            && registration.is_registered(),
        "native event caller is not the registered writer"
    );
    let directory = super::directory()?.join("events").join(generation);
    crate::infra::config::create_state_dir(&directory)?;
    let metadata = std::fs::symlink_metadata(&directory)?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "native event directory must be private to its owner"
    );
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(directory.join("queue.lock"))?;
    lock.lock_exclusive()?;
    ensure!(
        registration.is_registered(),
        "native event generation changed"
    );
    exchange_in(&directory, request)
}

fn sequence(event: &Value) -> crate::Result<u64> {
    event["seq"]
        .as_u64()
        .context("native event sequence is missing")
}

fn validate(events: &[Value]) -> crate::Result<()> {
    ensure!(
        events.len() <= MAX_EVENTS,
        "native event batch is too large"
    );
    let mut previous = None;
    for event in events {
        let seq = sequence(event)?;
        ensure!(
            seq > 0
                && seq <= 9_007_199_254_740_991
                && previous.is_none_or(|previous| seq == previous + 1),
            "native event batch is not consecutive"
        );
        previous = Some(seq);
        let fields: &[&str] = match event["kind"].as_str() {
            Some("turn_started") => &["turn"],
            Some("turn_completed") => &["turn", "reason", "duration_ms"],
            Some("tool_started") => &["id", "tool"],
            Some(
                "tool_completed"
                | "compaction_started"
                | "compaction_completed"
                | "approval_resolved",
            ) => &["id"],
            _ => anyhow::bail!("unknown native lifecycle event"),
        };
        for (key, value) in event.as_object().context("invalid native event")? {
            ensure!(
                matches!(key.as_str(), "seq" | "kind")
                    || (fields.contains(&key.as_str())
                        && if key == "duration_ms" {
                            value.is_u64()
                        } else {
                            value.as_str().is_some_and(|value| value.len() <= 512)
                        }),
                "native lifecycle journal accepts metadata only"
            );
        }
    }
    Ok(())
}

fn read<T: serde::de::DeserializeOwned>(path: &Path) -> crate::Result<T> {
    let file = crate::rc::native_inbox::open_regular(path)?;
    ensure!(
        file.metadata()?.len() < MAX_BYTES,
        "native event record is too large"
    );
    Ok(serde_json::from_reader(file.take(MAX_BYTES))?)
}

fn write(path: &Path, value: &impl Serialize) -> crate::Result<()> {
    let directory = path
        .parent()
        .context("native event record has no directory")?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut file, value)?;
    file.flush()?;
    ensure!(
        file.as_file().metadata()?.len() < MAX_BYTES,
        "native event record is too large"
    );
    file.as_file().sync_all()?;
    file.persist(path)?;
    crate::rc::native_inbox::sync_directory(directory)
}

fn exchange_in(directory: &Path, request: Request) -> crate::Result<Reply> {
    validate(&request.events)?;
    let ack_path = directory.join("ack.json");
    let saved: u64 = match read(&ack_path) {
        Ok(saved) => saved,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            0
        }
        Err(error) => return Err(error),
    };
    let acknowledged = saved.max(request.ack_seq.unwrap_or(0));
    if let (Some(first), Some(last)) = (request.events.first(), request.events.last()) {
        let first = sequence(first)?;
        let last = sequence(last)?;
        if last > acknowledged {
            let path = directory.join(format!("{first:020}-{last:020}.json"));
            if path.exists() {
                ensure!(
                    read::<Vec<Value>>(&path)? == request.events,
                    "native event replay changed"
                );
            } else {
                write(&path, &request.events)?;
            }
        }
    }
    // Persist acknowledgement before removing pages so a lost reply cannot resurrect events.
    if acknowledged != saved {
        write(&ack_path, &acknowledged)?;
    }
    let mut pages = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.len() == 46
            && name.is_ascii()
            && name.ends_with(".json")
            && name.as_bytes()[20] == b'-'
        {
            let first = name[..20].parse::<u64>()?;
            let last = name[21..41].parse::<u64>()?;
            if last <= acknowledged {
                std::fs::remove_file(entry.path())?;
            } else {
                pages.push((first, last, entry.path()));
            }
        }
    }
    crate::rc::native_inbox::sync_directory(directory)?;
    pages.sort_by_key(|page| page.0);
    let events = if let Some((first, last, path)) = pages.first() {
        let events: Vec<Value> = read(path)?;
        validate(&events)?;
        ensure!(
            events.first().map(sequence).transpose()? == Some(*first)
                && events.last().map(sequence).transpose()? == Some(*last),
            "native event page identity changed"
        );
        events
            .into_iter()
            .filter(|event| event["seq"].as_u64().unwrap() > acknowledged)
            .collect()
    } else {
        vec![]
    };
    Ok(Reply { events })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lifecycle_pages_survive_receipt_loss_and_partial_ack_without_copying_messages() {
        let directory = tempfile::tempdir().unwrap();
        let page = |start| {
            (start..start + MAX_EVENTS as u64)
            .map(|seq| json!({"seq":seq,"kind":"turn_completed","turn":format!("turn-{seq}"),"reason":"answer"}))
            .collect::<Vec<_>>()
        };
        let first = page(1);
        let second = page(1025);
        for events in [&first, &first, &second] {
            assert_eq!(
                exchange_in(
                    directory.path(),
                    Request {
                        events: events.clone(),
                        ack_seq: None
                    }
                )
                .unwrap()
                .events,
                first
            );
        }
        let reply = exchange_in(
            directory.path(),
            Request {
                ack_seq: Some(512),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(reply.events, first[512..]);
        for _ in 0..2 {
            let reply = exchange_in(
                directory.path(),
                Request {
                    ack_seq: Some(1024),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(reply.events, second);
        }
        assert!(
            exchange_in(
                directory.path(),
                Request {
                    ack_seq: Some(2048),
                    ..Default::default()
                }
            )
            .unwrap()
            .events
            .is_empty()
        );
        assert!(
            exchange_in(
                directory.path(),
                Request {
                    events: first,
                    ack_seq: None
                }
            )
            .unwrap()
            .events
            .is_empty()
        );
        assert!(
            validate(&[
                json!({"seq":1,"kind":"turn_started","turn":"turn","text":"private message"})
            ])
            .is_err()
        );
        assert!(
            validate(&[
                json!({"seq":1,"kind":"turn_started"}),
                json!({"seq":3,"kind":"turn_completed"})
            ])
            .is_err()
        );
    }
}
