//! Managed inputs keep their immutable payloads and owner-scoped delivery references.

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const VERSION: u32 = 1;
const LEGACY_MANAGED: &str = "owned-v1";
pub(in crate::rc) const MANAGED: &str = "owned-v2";
const RECORD_LIMIT: u64 = 128 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Owner {
    Local,
    Cloud {
        hub: String,
        account: String,
        device: String,
        enrollment: u64,
        machine: String,
        certificate_sha256: String,
    },
}

impl Owner {
    pub(crate) fn device(device: &agit_peer::cloud::Device) -> Self {
        Self::Cloud {
            hub: device.owner.issuer.clone(),
            account: device.owner.account_id.clone(),
            device: device.id.clone(),
            enrollment: device.credential_epoch,
            machine: device.machine_id.clone(),
            certificate_sha256: hex::encode(Sha256::digest(device.certificate.as_der())),
        }
    }
}

/// A typed reference names only one managed set, never an arbitrary filesystem path.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reference {
    key: String,
    nonce: String,
}

impl Reference {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        ensure!(valid_key(&self.key), "invalid attachment identity");
        uuid::Uuid::parse_str(&self.nonce).context("invalid attachment identity")?;
        Ok(())
    }
}

// Older custody records include expiry and reader fields. Only delivery and owner
// metadata participates in the current format.
#[derive(Serialize, Deserialize)]
struct Record {
    version: u32,
    reference: Reference,
    owner: Owner,
    session: String,
    input: String,
    retired: bool,
    reclaimed: bool,
    conversation: Option<String>,
    waiting: BTreeSet<String>,
    #[serde(default)]
    revision: Option<uuid::Uuid>,
}

fn valid_key(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn ordinary(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
    }
    #[cfg(not(windows))]
    {
        !metadata.file_type().is_symlink()
    }
}

fn directory(path: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_dir() && ordinary(&metadata),
            "invalid attachment directory"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::infra::config::create_state_dir(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && ordinary(&metadata),
        "invalid attachment directory"
    );
    Ok(())
}

fn lock(root: &Path) -> anyhow::Result<File> {
    directory(root)?;
    directory(&root.join(LEGACY_MANAGED))?;
    let path = root.join(LEGACY_MANAGED).join("custody.lock");
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        ensure!(
            metadata.is_file() && ordinary(&metadata),
            "invalid attachment coordination file"
        );
    }
    let mut options = crate::infra::config::state_file_options();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "invalid attachment coordination file"
    );
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(file)
}

fn set_path(root: &Path, reference: &Reference) -> anyhow::Result<PathBuf> {
    reference.validate()?;
    for managed in [MANAGED, LEGACY_MANAGED] {
        let directory = root.join(managed);
        if !directory.try_exists()? {
            continue;
        }
        verify_managed_directory(&directory)?;
        let path = directory.join(&reference.key);
        if path.try_exists()? && read_record(&path)?.reference == *reference {
            return Ok(path);
        }
    }
    Ok(root.join(MANAGED).join(&reference.key))
}

fn verify_managed_directory(path: &Path) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && ordinary(&metadata),
        "invalid attachment directory"
    );
    Ok(())
}

fn read_record(path: &Path) -> anyhow::Result<Record> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && ordinary(&metadata),
        "invalid attachment set"
    );
    let file = path.join("custody.json");
    let metadata = std::fs::symlink_metadata(&file)?;
    ensure!(
        metadata.is_file() && ordinary(&metadata) && metadata.len() <= RECORD_LIMIT,
        "invalid attachment custody"
    );
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let opened = options.open(file)?;
    let mut bytes = Vec::new();
    opened.take(RECORD_LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= RECORD_LIMIT,
        "attachment custody exceeds its bound"
    );
    let record: Record = serde_json::from_slice(&bytes)?;
    ensure!(record.version == VERSION, "unsupported attachment custody");
    record.reference.validate()?;
    ensure!(
        path.file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|name| name == record.reference.key),
        "attachment custody identity changed"
    );
    Ok(record)
}

fn write_record(path: &Path, record: &Record) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(record)?;
    ensure!(
        bytes.len() as u64 <= RECORD_LIMIT,
        "attachment custody exceeds its bound"
    );
    let mut temporary = tempfile::Builder::new()
        .prefix(".custody-")
        .tempfile_in(path)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path.join("custody.json"))
        .map_err(|error| error.error)?;
    sync_directory(path)?;
    Ok(())
}

fn sync_directory(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub(crate) fn publish(
    root: &Path,
    owner: Owner,
    session: &str,
    input: &str,
    prepared: &[(String, String, Vec<u8>)],
) -> anyhow::Result<(Reference, PathBuf)> {
    let legacy = root
        .join(hex::encode(Sha256::digest(session.as_bytes())))
        .join(input);
    if legacy.try_exists()? {
        return Err(super::Unavailable::Legacy.into());
    }
    let key = hex::encode(Sha256::digest(serde_json::to_vec(&(
        &owner, session, input,
    ))?));
    let _lock = lock(root)?;
    owner_current(root, &owner)?;
    let managed = root.join(MANAGED);
    directory(&managed)?;
    let current = managed.join(&key);
    let previous = root.join(LEGACY_MANAGED).join(&key);
    let path = if !current.try_exists()? && previous.try_exists()? {
        previous
    } else {
        current
    };
    let record = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            ensure!(metadata.is_dir(), "invalid attachment set");
            let mut record = read_record(&path)?;
            ensure!(
                record.owner == owner && record.session == session && record.input == input,
                "attachment custody identity changed"
            );
            if record.retired || record.reclaimed {
                return Err(super::Unavailable::Expired.into());
            }
            let payload = path.join("files");
            let payload_metadata = std::fs::symlink_metadata(&payload)?;
            ensure!(
                payload_metadata.is_dir() && ordinary(&payload_metadata),
                "invalid attachment payload"
            );
            ensure!(
                std::fs::read_dir(&payload)?
                    .collect::<std::io::Result<Vec<_>>>()?
                    .len()
                    == prepared.len(),
                "input id already has different attachments"
            );
            for (_, name, bytes) in prepared {
                let file = payload.join(name);
                let metadata = std::fs::symlink_metadata(&file)?;
                ensure!(
                    metadata.is_file()
                        && ordinary(&metadata)
                        && metadata.len() == bytes.len() as u64
                        && std::fs::read(file)? == *bytes,
                    "attachment content changed"
                );
            }
            record.revision = Some(uuid::Uuid::new_v4());
            write_record(&path, &record)?;
            record
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let temporary = tempfile::Builder::new()
                .prefix(".incoming-")
                .tempdir_in(root.join(MANAGED))?;
            let record = Record {
                version: VERSION,
                reference: Reference {
                    key,
                    nonce: uuid::Uuid::new_v4().to_string(),
                },
                owner,
                session: session.to_owned(),
                input: input.to_owned(),
                revision: Some(uuid::Uuid::new_v4()),
                retired: false,
                reclaimed: false,
                conversation: None,
                waiting: BTreeSet::new(),
            };
            write_record(temporary.path(), &record)?;
            let payload = temporary.path().join("files");
            directory(&payload)?;
            for (_, filename, bytes) in prepared {
                let mut output = crate::infra::config::state_file_options()
                    .create_new(true)
                    .write(true)
                    .open(payload.join(filename))?;
                output.write_all(bytes)?;
                output.sync_all()?;
            }
            std::fs::rename(temporary.path(), &path)?;
            sync_directory(&root.join(MANAGED))?;
            record
        }
        Err(error) => return Err(error.into()),
    };
    Ok((record.reference, path.join("files")))
}

fn entries(root: &Path, managed: &str) -> anyhow::Result<Vec<PathBuf>> {
    let directory = root.join(managed);
    if !directory.try_exists()? {
        return Ok(Vec::new());
    }
    verify_managed_directory(&directory)?;
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_name().to_str().is_some_and(valid_key) {
            ensure!(
                entry.file_type()?.is_dir(),
                "invalid managed attachment entry"
            );
            paths.push(entry.path());
        }
    }
    Ok(paths)
}

fn records(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut paths = entries(root, MANAGED)?;
    paths.extend(entries(root, LEGACY_MANAGED)?);
    Ok(paths)
}

/// A legacy queue can be rewritten only while no managed set can claim its old record.
pub(crate) fn migrate_legacy_waiting<T>(
    root: &Path,
    conversation: &str,
    persist: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let _lock = lock(root)?;
    for path in records(root)? {
        let record = read_record(&path)?;
        ensure!(
            !record.waiting.contains(conversation)
                && record.conversation.as_deref() != Some(conversation),
            "legacy waiting record has uncertain managed references"
        );
    }
    persist()
}

/// The waiting-file update runs between adding references and retiring the old references.
pub(crate) fn update_waiting<T>(
    root: &Path,
    conversation: &str,
    references: &[Reference],
    persist: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let _lock = lock(root)?;
    let daemon = root
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .context("invalid attachment namespace")?;
    let previous: BTreeSet<_> = crate::rc::waiting_messages::load_checked_in(daemon, conversation)?
        .into_iter()
        .flat_map(|item| item.attachments)
        .collect();
    let next: BTreeSet<_> = references.iter().cloned().collect();
    let mut additions = Vec::new();
    for reference in next.difference(&previous) {
        let path = set_path(root, reference)?;
        let mut record = read_record(&path)?;
        ensure!(
            &record.reference == reference && !record.retired && !record.reclaimed,
            "attachment reference is unavailable"
        );
        ensure!(
            record
                .conversation
                .as_deref()
                .is_none_or(|known| known == conversation),
            "attachment conversation identity changed"
        );
        record.conversation = Some(conversation.to_owned());
        record.waiting.insert(conversation.to_owned());
        additions.push((path, record));
    }
    for (path, record) in &additions {
        write_record(path, record)?;
    }
    let result = persist()?;
    for reference in previous.difference(&next) {
        let path = set_path(root, reference)?;
        let mut record = read_record(&path)?;
        ensure!(
            &record.reference == reference,
            "attachment identity changed"
        );
        if record.waiting.remove(conversation) {
            write_record(&path, &record)?;
        }
    }
    Ok(result)
}

/// Recheck ownership before a queued input is handed to a resumed conversation.
pub(crate) fn authorize_waiting(
    root: &Path,
    owner: &Owner,
    conversation: &str,
    references: &[Reference],
) -> anyhow::Result<()> {
    let _lock = lock(root)?;
    owner_current(root, owner)?;
    for reference in references {
        let path = set_path(root, reference)?;
        let mut record = read_record(&path)?;
        ensure!(
            &record.reference == reference
                && &record.owner == owner
                && !record.retired
                && !record.reclaimed
                && record.conversation.as_deref() == Some(conversation),
            "attachment reference is unavailable"
        );
        record.revision = Some(uuid::Uuid::new_v4());
        write_record(&path, &record)?;
    }
    Ok(())
}

/// The snapshot names existing sets only; a concurrent sign-in cannot add cleanup targets.
#[derive(Default)]
pub(crate) struct Retirement(Vec<(PathBuf, Reference, Owner, Vec<u8>)>);

pub(crate) fn snapshot_account(hub: &str, account: &str) -> Retirement {
    let collected = (|| -> anyhow::Result<Retirement> {
        let origin = agit_peer::client::Client::new(hub)?.origin().to_owned();
        let mut found = Vec::new();
        for daemon in ["rc", crate::rc::LOCAL_NAMESPACE] {
            let root = crate::rc::namespace_dir(daemon)?.join("task-attachments");
            if !root.join(LEGACY_MANAGED).try_exists()? {
                continue;
            }
            let _lock = lock(&root)?;
            for path in records(&root)? {
                let record = read_record(&path)?;
                if matches!(&record.owner, Owner::Cloud { hub, account: known, .. }
                    if hub == &origin && known == account)
                {
                    let revision = serde_json::to_vec(&record)?;
                    found.push((root.clone(), record.reference, record.owner, revision));
                }
            }
        }
        Ok(Retirement(found))
    })();
    collected.unwrap_or_else(|_| {
        eprintln!("agit: attachment cleanup is incomplete");
        Retirement::default()
    })
}

/// Account-wide legacy cleanup cannot drop references belonging to a different managed owner.
pub(crate) fn forget_unowned_waiting(
    root: &Path,
    daemon: &str,
    conversation: &str,
) -> anyhow::Result<()> {
    let _lock = lock(root)?;
    let waiting = crate::rc::waiting_messages::load_checked_in(daemon, conversation)?;
    let kept: Vec<_> = waiting
        .into_iter()
        .filter(|stored| !stored.attachments.is_empty())
        .collect();
    crate::rc::waiting_messages::persist_in(daemon, conversation, &kept)
}

fn remove_retired_waiting(
    root: &Path,
    reference: &Reference,
    conversations: &BTreeSet<String>,
) -> anyhow::Result<()> {
    let daemon = root
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .context("invalid attachment namespace")?;
    for conversation in conversations {
        let waiting = crate::rc::waiting_messages::load_checked_in(daemon, conversation)?;
        let kept: Vec<_> = waiting
            .into_iter()
            .filter(|stored| !stored.attachments.contains(reference))
            .collect();
        crate::rc::waiting_messages::persist_in(daemon, conversation, &kept)?;
    }
    Ok(())
}

/// Finish a revoked delivery that was still queued when retirement was interrupted.
/// The queue itself identifies the records to inspect; payload files are never opened here.
pub(crate) fn recover_retired_waiting(daemon: &str, conversation: &str) -> anyhow::Result<()> {
    let root = crate::rc::namespace_dir(daemon)?.join("task-attachments");
    let _lock = lock(&root)?;
    let mut waiting = crate::rc::waiting_messages::load_checked_in(daemon, conversation)?;
    let references: BTreeSet<_> = waiting.iter().flat_map(|item| &item.attachments).collect();
    let mut retired = BTreeSet::new();
    for reference in references {
        let path = set_path(&root, reference)?;
        let mut record = read_record(&path)?;
        ensure!(
            record.reference == *reference,
            "attachment identity changed"
        );
        if !record.retired && !retired_marker(&root, &record.owner)?.try_exists()? {
            continue;
        }
        // If the queue write fails, the durable retirement still identifies this input
        // on the next start, even after its waiting membership has been removed.
        let was_waiting = record.waiting.remove(conversation);
        if !record.retired || was_waiting {
            record.retired = true;
            write_record(&path, &record)?;
        }
        retired.insert(reference.clone());
    }
    if !retired.is_empty() {
        waiting.retain(|item| item.attachments.iter().all(|r| !retired.contains(r)));
        crate::rc::waiting_messages::persist_in(daemon, conversation, &waiting)?;
    }
    Ok(())
}

impl Retirement {
    pub(crate) fn finish(self) {
        for (root, reference, owner, revision) in self.0 {
            let result = (|| -> anyhow::Result<()> {
                let _lock = lock(&root)?;
                let path = set_path(&root, &reference)?;
                if !path.try_exists()? {
                    return Ok(());
                }
                let mut record = read_record(&path)?;
                ensure!(
                    record.reference == reference && record.owner == owner,
                    "attachment custody identity changed"
                );
                ensure!(
                    serde_json::to_vec(&record)? == revision,
                    "attachment consumers changed during the account transition"
                );
                record.retired = true;
                write_record(&path, &record)?;
                remove_retired_waiting(&root, &reference, &record.waiting)?;
                record.waiting.clear();
                write_record(&path, &record)?;
                Ok(())
            })();
            if result.is_err() {
                eprintln!("agit: attachment cleanup is incomplete");
            }
        }
    }
}

/// Only a committed replacement of the exact enrollment retires its attachment ownership.
pub(crate) fn retire_device(root: &Path, device: &agit_peer::cloud::Device) -> anyhow::Result<()> {
    let owner = Owner::device(device);
    if !root.join(LEGACY_MANAGED).try_exists()? {
        return Ok(());
    }
    let _lock = lock(root)?;
    let marker = retired_marker(root, &owner)?;
    let mut options = crate::infra::config::state_file_options();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(marker)?;
    file.write_all(b"retired\n")?;
    file.sync_all()?;
    sync_directory(&root.join(LEGACY_MANAGED))?;
    for path in records(root)? {
        let mut record = read_record(&path)?;
        if record.owner != owner {
            continue;
        }
        record.retired = true;
        write_record(&path, &record)?;
        remove_retired_waiting(root, &record.reference, &record.waiting)?;
        record.waiting.clear();
        write_record(&path, &record)?;
    }
    Ok(())
}

fn owner_key(owner: &Owner) -> anyhow::Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(owner)?)))
}

fn retired_marker(root: &Path, owner: &Owner) -> anyhow::Result<PathBuf> {
    Ok(root
        .join(LEGACY_MANAGED)
        .join(format!("retired-{}", owner_key(owner)?)))
}

fn owner_current(root: &Path, owner: &Owner) -> anyhow::Result<()> {
    ensure!(
        !retired_marker(root, owner)?.try_exists()?,
        "attachment enrollment has retired"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Waiting input stays protected and account retirement drops the queue without breaking
    // paths already handed to a session.
    #[test]
    fn retiring_an_owner_revokes_delivery_and_preserves_its_files() {
        let home = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(home.path(), || -> anyhow::Result<()> {
            let root = crate::rc::rc_dir()?.join("task-attachments");
            let owner = Owner::Cloud {
                hub: "https://synthetic.invalid".into(),
                account: "owner".into(),
                device: "device".into(),
                enrollment: 1,
                machine: "machine".into(),
                certificate_sha256: "synthetic".into(),
            };
            let files = vec![("file.txt".into(), "file.txt".into(), b"synthetic".to_vec())];
            let make = |owner: Owner, session: &str| {
                publish(
                    &root,
                    owner,
                    session,
                    &uuid::Uuid::new_v4().to_string(),
                    &files,
                )
            };
            let (reference, payload) = make(owner.clone(), "session")?;

            let conversation = "conversation";
            let stored = crate::rc::waiting_messages::Stored {
                message: "synthetic attachment".into(),
                attachments: vec![reference.clone()],
                client_msg_id: Some("input".into()),
                sender: None,
                by: None,
                arrived_ms: 1,
            };
            crate::rc::waiting_messages::save(conversation, std::slice::from_ref(&stored))?;
            let path = set_path(&root, &reference)?;
            let waiting_path =
                crate::rc::namespace_dir("rc/waiting-messages")?.join("conversation.json");
            std::fs::write(&waiting_path, b"{broken")?;
            assert!(crate::rc::waiting_messages::save(conversation, &[]).is_err());
            assert_eq!(std::fs::read(&waiting_path)?, b"{broken");
            assert!(payload.join("file.txt").is_file());
            assert!(
                update_waiting(&root, conversation, &[], || -> anyhow::Result<()> {
                    anyhow::bail!("synthetic persistence refusal")
                })
                .is_err()
            );
            assert!(read_record(&path)?.waiting.contains(conversation));
            crate::rc::waiting_messages::persist_in(
                "rc",
                conversation,
                std::slice::from_ref(&stored),
            )?;
            authorize_waiting(
                &root,
                &owner,
                conversation,
                std::slice::from_ref(&reference),
            )?;
            // Unrelated, unreadable custody must not block a durable queue transition.
            let unrelated = root.join(MANAGED).join("0".repeat(64));
            std::fs::create_dir(&unrelated)?;
            crate::rc::waiting_messages::save(conversation, &[])?;
            assert!(!read_record(&path)?.waiting.contains(conversation));
            std::fs::remove_dir(unrelated)?;
            let (inactive, inactive_files) = make(owner.clone(), "inactive")?;

            let (local, local_files) = make(Owner::Local, "local")?;

            let (reused_reference, reused_files) = make(owner.clone(), "reused")?;

            let reused_path = set_path(&root, &reused_reference)?;
            let reused_input = read_record(&reused_path)?.input;
            let retired_waiting = crate::rc::waiting_messages::Stored {
                attachments: vec![inactive],
                ..stored.clone()
            };
            let local_waiting = crate::rc::waiting_messages::Stored {
                attachments: vec![local],
                ..stored.clone()
            };
            crate::rc::waiting_messages::save(
                "retired-waiting",
                std::slice::from_ref(&retired_waiting),
            )?;
            crate::rc::waiting_messages::save(
                "local-waiting",
                std::slice::from_ref(&local_waiting),
            )?;
            crate::rc::waiting_messages::forget_all();
            assert_eq!(
                crate::rc::waiting_messages::load("retired-waiting"),
                vec![retired_waiting.clone()]
            );
            assert_eq!(
                crate::rc::waiting_messages::load("local-waiting"),
                vec![local_waiting.clone()]
            );
            let retirement = snapshot_account("https://synthetic.invalid", "owner");
            publish(&root, owner.clone(), "reused", &reused_input, &files)?;

            let (_, concurrent_files) = make(owner, "concurrent")?;

            retirement.finish();
            assert!(reused_files.is_dir());
            assert!(
                !read_record(&reused_path)?.retired,
                "a concurrent consumer changes the snapshot's retirement authority"
            );
            assert!(inactive_files.is_dir());
            assert!(crate::rc::waiting_messages::load("retired-waiting").is_empty());
            assert_eq!(
                crate::rc::waiting_messages::load("local-waiting"),
                vec![local_waiting]
            );
            assert!(
                crate::rc::waiting_messages::save("retired-waiting", &[retired_waiting]).is_err()
            );
            assert!(local_files.is_dir());
            assert!(
                concurrent_files.is_dir(),
                "a later login's new set is outside the snapshot"
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn retry_after_upgrade_keeps_a_past_deadline_file_at_its_original_path() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        let root = home.path();
        let input = uuid::Uuid::new_v4().to_string();
        let files = [("photo.jpg".into(), "photo.jpg".into(), vec![1, 2, 3])];
        let (reference, payload) = publish(root, Owner::Local, "session", &input, &files)?;
        let current = payload.parent().unwrap();
        let legacy = root.join(LEGACY_MANAGED).join(&reference.key);
        std::fs::rename(current, &legacy)?;
        let metadata = legacy.join("custody.json");
        let mut record: serde_json::Value = serde_json::from_slice(&std::fs::read(&metadata)?)?;
        record.as_object_mut().unwrap().remove("revision");
        record["delivered"] = true.into();
        record["retry_deadline"] = 1.into();
        record["consumers"] = serde_json::json!({});
        std::fs::write(metadata, serde_json::to_vec(&record)?)?;

        let (retry, path) = publish(root, Owner::Local, "session", &input, &files)?;
        assert_eq!(retry, reference);
        assert_eq!(path, legacy.join("files"));
        assert_eq!(std::fs::read(path.join("photo.jpg"))?, [1, 2, 3]);
        Ok(())
    }
}
