//! Managed input sets keep identity and consumer proofs beside their immutable payloads.

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::OnceLock,
};

const VERSION: u32 = 1;
const MANAGED: &str = "owned-v1";
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Consumer {
    daemon: String,
    generation: u64,
    session: String,
    process: u32,
    process_start: Option<i64>,
    possible_delivery: bool,
    incomplete_release: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    reference: Reference,
    owner: Owner,
    session: String,
    input: String,
    created_ms: i64,
    delivered: bool,
    retry_deadline: Option<i64>,
    retired: bool,
    reclaimed: bool,
    conversation: Option<String>,
    consumers: BTreeMap<String, Consumer>,
    waiting: BTreeSet<String>,
}

/// Custody remains with the staging worker even if its async caller is cancelled.
pub(crate) struct Attempt {
    root: PathBuf,
    reference: Reference,
    token: String,
    queued: bool,
}

fn incarnation() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| uuid::Uuid::new_v4().to_string())
}

fn process_start() -> Option<i64> {
    #[cfg(unix)]
    {
        crate::rc::native_claude::started_ms(std::process::id())
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn process_gone(consumer: &Consumer) -> bool {
    #[cfg(unix)]
    {
        if let Some(started) = consumer.process_start {
            if let Some(current) = crate::rc::native_claude::started_ms(consumer.process) {
                return current != started;
            }
            if let Ok(pid) = i32::try_from(consumer.process) {
                return pid > 0
                    && unsafe { libc::kill(pid, 0) } != 0
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            }
        }
    }
    let _ = consumer;
    false
}

fn valid_key(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn directory(path: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(metadata.is_dir(), "invalid attachment directory"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::infra::config::create_state_dir(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    ensure!(
        std::fs::symlink_metadata(path)?.is_dir(),
        "invalid attachment directory"
    );
    Ok(())
}

fn lock(root: &Path) -> anyhow::Result<File> {
    directory(root)?;
    directory(&root.join(MANAGED))?;
    let path = root.join(MANAGED).join("custody.lock");
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
    Ok(root.join(MANAGED).join(&reference.key))
}

fn read_record(path: &Path) -> anyhow::Result<Record> {
    ensure!(
        std::fs::symlink_metadata(path)?.is_dir(),
        "invalid attachment set"
    );
    let file = path.join("custody.json");
    let metadata = std::fs::symlink_metadata(&file)?;
    ensure!(
        metadata.is_file() && metadata.len() <= RECORD_LIMIT,
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
            .is_some_and(|name| name == record.reference.key || name.starts_with(".incoming-")),
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

fn change<T>(
    root: &Path,
    reference: &Reference,
    operation: impl FnOnce(&mut Record) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let _lock = lock(root)?;
    let path = set_path(root, reference)?;
    let mut record = read_record(&path)?;
    ensure!(
        &record.reference == reference,
        "attachment set was replaced"
    );
    let result = operation(&mut record)?;
    write_record(&path, &record)?;
    Ok(result)
}

fn sync_directory(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn expire(path: &Path, record: &mut Record, now: i64) -> anyhow::Result<bool> {
    if record.reclaimed {
        return Ok(true);
    }
    if path
        .parent()
        .context("attachment parent is missing")?
        .join(format!("retired-{}", owner_key(&record.owner)?))
        .try_exists()?
    {
        record.retired = true;
        write_record(path, record)?;
    }
    if !record.consumers.is_empty() || !record.waiting.is_empty() {
        return Ok(false);
    }
    if record.delivered && record.retry_deadline.is_none() {
        record.retry_deadline = Some(now.saturating_add(crate::rc::waiting_messages::MAX_AGE_MS));
        write_record(path, record)?;
    }
    if record.retired
        || !record.delivered
        || record.retry_deadline.is_some_and(|until| now >= until)
    {
        if !record.delivered && !record.retired {
            std::fs::remove_dir_all(path)?;
        } else {
            // Keep an identity tombstone so a retry cannot silently recreate expired inputs.
            record.reclaimed = true;
            write_record(path, record)?;
            remove_payload(path)?;
        }
        sync_directory(path.parent().context("attachment parent is missing")?)?;
        return Ok(true);
    }
    Ok(false)
}

fn remove_payload(path: &Path) -> anyhow::Result<()> {
    let payload = path.join("files");
    match std::fs::symlink_metadata(&payload) {
        Ok(metadata) => {
            ensure!(metadata.is_dir(), "invalid attachment payload");
            std::fs::remove_dir_all(payload)?;
            sync_directory(path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub(crate) fn publish(
    root: &Path,
    owner: Owner,
    session: &str,
    input: &str,
    generation: u64,
    prepared: &[(String, String, Vec<u8>)],
) -> anyhow::Result<(Attempt, PathBuf)> {
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
    let path = root.join(MANAGED).join(&key);
    let token = uuid::Uuid::new_v4().to_string();
    let consumer = Consumer {
        daemon: incarnation().to_owned(),
        generation,
        session: session.to_owned(),
        process: std::process::id(),
        process_start: process_start(),
        possible_delivery: false,
        incomplete_release: false,
    };
    let now = chrono::Utc::now().timestamp_millis();
    let (mut record, created) = match std::fs::symlink_metadata(&path) {
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
            if record.consumers.is_empty()
                && record.waiting.is_empty()
                && record
                    .retry_deadline
                    .is_some_and(|deadline| now >= deadline)
            {
                expire(&path, &mut record, now)?;
                return Err(super::Unavailable::Expired.into());
            }
            let payload = path.join("files");
            ensure!(
                std::fs::symlink_metadata(&payload)?.is_dir(),
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
                        && metadata.len() == bytes.len() as u64
                        && std::fs::read(file)? == *bytes,
                    "attachment content changed"
                );
            }
            (record, false)
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
                created_ms: now,
                delivered: false,
                retry_deadline: None,
                retired: false,
                reclaimed: false,
                conversation: None,
                consumers: BTreeMap::from([(token.clone(), consumer.clone())]),
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
            (record, true)
        }
        Err(error) => return Err(error.into()),
    };
    record.consumers.insert(token.clone(), consumer);
    if !created {
        write_record(&path, &record)?;
    }
    Ok((
        Attempt {
            root: root.to_owned(),
            reference: record.reference,
            token,
            queued: false,
        },
        path.join("files"),
    ))
}

impl Attempt {
    pub(crate) fn reference(&self) -> Reference {
        self.reference.clone()
    }

    /// Persist uncertainty before a command can become visible to its executor.
    pub(crate) fn prepare_delivery(&self) -> anyhow::Result<()> {
        change(&self.root, &self.reference, |record| {
            ensure!(!record.retired, "attachment owner has retired");
            record
                .consumers
                .get_mut(&self.token)
                .context("attachment consumer disappeared")?
                .possible_delivery = true;
            Ok(())
        })
    }

    pub(crate) fn queued(&mut self) {
        self.queued = true;
    }

    pub(crate) fn never_ran(mut self) {
        self.queued = false;
    }

    fn refuse(&self) -> anyhow::Result<()> {
        let _lock = lock(&self.root)?;
        let path = set_path(&self.root, &self.reference)?;
        let mut record = read_record(&path)?;
        ensure!(
            record.reference == self.reference,
            "attachment set was replaced"
        );
        record.consumers.remove(&self.token);
        write_record(&path, &record)?;
        expire(&path, &mut record, chrono::Utc::now().timestamp_millis())?;
        Ok(())
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        if !self.queued && self.refuse().is_err() {
            eprintln!("agitd: attachment cleanup is incomplete");
        }
    }
}

fn records(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(root.join(MANAGED))? {
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

pub(crate) fn release_generation(
    root: &Path,
    session: &str,
    generation: u64,
    now: i64,
) -> anyhow::Result<()> {
    if !root.join(MANAGED).try_exists()? {
        return Ok(());
    }
    let _lock = lock(root)?;
    for path in records(root)? {
        let mut record = read_record(&path)?;
        let released = |consumer: &Consumer| {
            consumer.daemon == incarnation()
                && consumer.session == session
                && consumer.generation == generation
        };
        record.delivered |= record
            .consumers
            .values()
            .any(|c| released(c) && c.possible_delivery);
        record.consumers.retain(|_, consumer| !released(consumer));
        write_record(&path, &record)?;
        expire(&path, &mut record, now)?;
    }
    Ok(())
}

pub(crate) fn retain_generation(root: &Path, session: &str, generation: u64) -> anyhow::Result<()> {
    if !root.join(MANAGED).try_exists()? {
        return Ok(());
    }
    let _lock = lock(root)?;
    for path in records(root)? {
        let mut record = read_record(&path)?;
        let mut changed = false;
        for consumer in record.consumers.values_mut() {
            if consumer.daemon == incarnation()
                && consumer.session == session
                && consumer.generation == generation
            {
                consumer.incomplete_release = true;
                changed = true;
            }
        }
        if changed {
            write_record(&path, &record)?;
        }
    }
    Ok(())
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
            expire(&path, &mut record, chrono::Utc::now().timestamp_millis())?;
        }
    }
    Ok(result)
}

pub(crate) fn sweep(root: &Path, now: i64) -> anyhow::Result<()> {
    if !root.join(MANAGED).try_exists()? {
        return Ok(());
    }
    let _lock = lock(root)?;
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(root.join(MANAGED))? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| valid_key(name) || name.starts_with(".incoming-"))
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    static CURSORS: OnceLock<std::sync::Mutex<BTreeMap<PathBuf, PathBuf>>> = OnceLock::new();
    let mut cursors = CURSORS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let start = cursors
        .get(root)
        .map(|cursor| paths.partition_point(|path| path <= cursor))
        .unwrap_or(0);
    let batch: Vec<_> = paths[start..]
        .iter()
        .chain(paths[..start].iter())
        .take(64)
        .cloned()
        .collect();
    if let Some(last) = batch.last() {
        cursors.insert(root.to_owned(), last.clone());
    }
    drop(cursors);
    let mut incomplete = false;
    for path in batch {
        let result = (|| -> anyhow::Result<()> {
            let mut record = read_record(&path)?;
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".incoming-"))
            {
                if record
                    .consumers
                    .values()
                    .all(|consumer| !consumer.possible_delivery && process_gone(consumer))
                {
                    std::fs::remove_dir_all(&path)?;
                } else if record
                    .consumers
                    .values()
                    .any(|consumer| consumer.daemon != incarnation())
                {
                    incomplete = true;
                }
                return Ok(());
            }
            if record.reclaimed {
                return remove_payload(&path);
            }
            if record.retired {
                remove_retired_waiting(root, &record.reference, &record.waiting)?;
                record.waiting.clear();
            }
            let daemon = root
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .context("invalid attachment namespace")?;
            for conversation in record.waiting.clone() {
                let waiting = crate::rc::waiting_messages::load_checked_in(daemon, &conversation)?;
                if !waiting
                    .iter()
                    .flat_map(|item| &item.attachments)
                    .any(|reference| reference == &record.reference)
                {
                    record.waiting.remove(&conversation);
                }
            }
            record
                .consumers
                .retain(|_, consumer| consumer.possible_delivery || !process_gone(consumer));
            incomplete |= record
                .consumers
                .values()
                .any(|consumer| consumer.daemon != incarnation() || consumer.incomplete_release);
            write_record(&path, &record)?;
            expire(&path, &mut record, now)?;
            Ok(())
        })();
        incomplete |= result.is_err();
    }
    ensure!(!incomplete, "attachment cleanup is incomplete");
    Ok(())
}

/// A restored message gains a live consumer before its durable waiting reference is removed.
pub(crate) fn protect_waiting(
    root: &Path,
    owner: &Owner,
    session: &str,
    generation: u64,
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
        let token = format!("{}:{session}:{generation}", incarnation());
        record.consumers.insert(
            token,
            Consumer {
                daemon: incarnation().to_owned(),
                session: session.to_owned(),
                generation,
                process: std::process::id(),
                process_start: process_start(),
                possible_delivery: true,
                incomplete_release: false,
            },
        );
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
            if !root.join(MANAGED).try_exists()? {
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
                expire(&path, &mut record, chrono::Utc::now().timestamp_millis())?;
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
    if !root.join(MANAGED).try_exists()? {
        return Ok(());
    }
    let _lock = lock(root)?;
    let marker = root
        .join(MANAGED)
        .join(format!("retired-{}", owner_key(&owner)?));
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
    sync_directory(&root.join(MANAGED))?;
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
        expire(&path, &mut record, chrono::Utc::now().timestamp_millis())?;
    }
    Ok(())
}

fn owner_key(owner: &Owner) -> anyhow::Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(owner)?)))
}

fn owner_current(root: &Path, owner: &Owner) -> anyhow::Result<()> {
    ensure!(
        !root
            .join(MANAGED)
            .join(format!("retired-{}", owner_key(owner)?))
            .try_exists()?,
        "attachment enrollment has retired"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // A missing or failed waiting-store transition cannot release an input, and neither a
    // new consumer nor a scan grants another retry interval after the first proven release.
    #[test]
    fn durable_references_and_exact_owner_retirement_bound_input_lifetime() {
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
                    1,
                    &files,
                )
            };
            let (mut attempt, payload) = make(owner.clone(), "session")?;
            attempt.prepare_delivery()?;
            attempt.queued();
            let reference = attempt.reference();
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
            release_generation(&root, "session", 1, 10)?;
            let path = set_path(&root, &reference)?;
            assert_eq!(read_record(&path)?.retry_deadline, None);
            let waiting_path =
                crate::rc::namespace_dir("rc/waiting-messages")?.join("conversation.json");
            std::fs::write(&waiting_path, b"{broken")?;
            assert!(sweep(&root, i64::MAX).is_err());
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
            protect_waiting(
                &root,
                &owner,
                "session",
                2,
                conversation,
                std::slice::from_ref(&reference),
            )?;
            // Unrelated, unreadable custody must not block a durable queue transition.
            let unrelated = root.join(MANAGED).join("0".repeat(64));
            std::fs::create_dir(&unrelated)?;
            crate::rc::waiting_messages::save(conversation, &[])?;
            assert!(!read_record(&path)?.waiting.contains(conversation));
            std::fs::remove_dir(unrelated)?;
            release_generation(&root, "session", 1, 10)?;
            assert!(
                payload.is_dir(),
                "an older generation cannot release the new consumer"
            );
            release_generation(&root, "session", 2, 20)?;
            let deadline = read_record(&path)?.retry_deadline.unwrap();
            assert_eq!(deadline, 20 + crate::rc::waiting_messages::MAX_AGE_MS);
            protect_waiting(
                &root,
                &owner,
                "session",
                3,
                conversation,
                std::slice::from_ref(&reference),
            )?;
            sweep(&root, deadline + 1)?;
            assert!(payload.is_dir());
            release_generation(&root, "session", 3, deadline + 2)?;
            assert!(!payload.exists());
            assert_eq!(read_record(&path)?.retry_deadline, Some(deadline));

            let (mut inactive, inactive_files) = make(owner.clone(), "inactive")?;
            inactive.prepare_delivery()?;
            inactive.queued();
            release_generation(&root, "inactive", 1, chrono::Utc::now().timestamp_millis())?;
            let (mut uncertain, uncertain_files) = make(owner.clone(), "uncertain")?;
            uncertain.prepare_delivery()?;
            uncertain.queued();
            let (mut local, local_files) = make(Owner::Local, "local")?;
            local.prepare_delivery()?;
            local.queued();
            let (mut reused, reused_files) = make(owner.clone(), "reused")?;
            reused.prepare_delivery()?;
            reused.queued();
            let reused_reference = reused.reference();
            let reused_path = set_path(&root, &reused_reference)?;
            let reused_input = read_record(&reused_path)?.input;
            let retired_waiting = crate::rc::waiting_messages::Stored {
                attachments: vec![inactive.reference()],
                ..stored.clone()
            };
            let local_waiting = crate::rc::waiting_messages::Stored {
                attachments: vec![local.reference()],
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
            let (mut retry, _) = publish(&root, owner.clone(), "reused", &reused_input, 2, &files)?;
            retry.prepare_delivery()?;
            retry.queued();
            let (mut concurrent, concurrent_files) = make(owner, "concurrent")?;
            concurrent.prepare_delivery()?;
            concurrent.queued();
            retirement.finish();
            assert!(reused_files.is_dir());
            assert!(
                !read_record(&reused_path)?.retired,
                "a concurrent consumer changes the snapshot's retirement authority"
            );
            assert!(!inactive_files.exists());
            assert!(crate::rc::waiting_messages::load("retired-waiting").is_empty());
            assert_eq!(
                crate::rc::waiting_messages::load("local-waiting"),
                vec![local_waiting]
            );
            assert!(
                crate::rc::waiting_messages::save("retired-waiting", &[retired_waiting]).is_err()
            );
            assert!(
                uncertain_files.is_dir(),
                "uncertain process cleanup retains the input"
            );
            assert!(local_files.is_dir());
            assert!(
                concurrent_files.is_dir(),
                "a later login's new set is outside the snapshot"
            );
            release_generation(&root, "uncertain", 1, chrono::Utc::now().timestamp_millis())?;
            assert!(
                !uncertain_files.exists(),
                "retired ownership cancels the retry interval"
            );
            assert!(local_files.is_dir());
            assert!(concurrent_files.is_dir());
            Ok(())
        })
        .unwrap();
    }
}
