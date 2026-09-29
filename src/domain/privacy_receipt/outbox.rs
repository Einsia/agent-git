//! Local publication evidence survives branch advancement independently of delivery receipts.

use super::{MAX_RECEIPT_BYTES, PublicationReceipt, Repo, SupervisorPushRequest};
use agit_peer::publication::{Executor, Notification, Receipt};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::PathBuf};

const MAX_ENTRIES: usize = 4096;

mod candidates;
mod reclaim;

pub use agit_peer::publication::Capture;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    version: u32,
    pub notification_id: String,
    pub capture: Capture,
    request: SupervisorPushRequest,
    #[serde(default)]
    pub prepared: Option<PublicationReceipt>,
    pub publication: Option<PublicationReceipt>,
    #[serde(default)]
    pub notification: Option<Notification>,
    #[serde(default)]
    pub acknowledged: Option<Receipt>,
}

impl Entry {
    /// Retrying a source retains its original notification identity and capture coordinates.
    pub fn begin(
        repo: &Repo,
        request: &mut SupervisorPushRequest,
        capture: Capture,
    ) -> Result<Self> {
        request.validate()?;
        ensure!(
            request.notification_id.is_none(),
            "publication intent is already selected"
        );
        for value in [
            &capture.session_id,
            &capture.native_session_id,
            &capture.runtime,
        ] {
            ensure!(
                !value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control),
                "invalid local publication capture"
            );
        }
        let (path, _lock) = locked_path(repo, request)?;
        if let Some(saved) = Self::family(repo, request)?.into_iter().max_by(|a, b| {
            (a.publication.is_some(), &a.notification_id)
                .cmp(&(b.publication.is_some(), &b.notification_id))
        }) {
            saved.matches(request)?;
            request.notification_id = Some(saved.notification_id.clone());
            return Ok(saved);
        }
        let directory = path.parent().context("publication directory is missing")?;
        check_capacity(directory)?;
        let notification_id = uuid::Uuid::now_v7().to_string();
        request.notification_id = Some(notification_id.clone());
        let saved = Self {
            version: 2,
            notification_id,
            capture,
            request: request.clone(),
            prepared: None,
            publication: None,
            notification: None,
            acknowledged: None,
        };
        saved.write(&saved.record_path(directory)?)?;
        Ok(saved)
    }

    pub fn load(repo: &Repo, request: &SupervisorPushRequest) -> Result<Option<Self>> {
        request.validate()?;
        if request.notification_id.is_none() {
            return Ok(Self::family(repo, request)?.into_iter().max_by(|a, b| {
                (a.publication.is_some(), &a.notification_id)
                    .cmp(&(b.publication.is_some(), &b.notification_id))
            }));
        }
        let Some(saved) = Self::read(&path(repo, request)?)? else {
            return Ok(None);
        };
        saved.matches(request)?;
        if let Some(id) = &request.notification_id {
            ensure!(
                id == &saved.notification_id,
                "publication notification identity changed"
            );
        }
        Ok(Some(saved))
    }

    pub fn request(&self) -> &SupervisorPushRequest {
        &self.request
    }

    /// Freeze public content and executor identity before the first network attempt.
    pub fn bind_notification(
        repo: &Repo,
        request: &SupervisorPushRequest,
        executor: Executor,
    ) -> Result<Notification> {
        let (path, _lock) = locked_path(repo, request)?;
        let mut saved = Self::load(repo, request)?.context("publication intent is missing")?;
        if let Some(publication) = saved.publication.as_ref().or(saved.prepared.as_ref())
            && publication.projected_session_id.is_none()
        {
            let session_id = publication.session_id(repo)?;
            for receipt in [&mut saved.prepared, &mut saved.publication]
                .into_iter()
                .flatten()
            {
                receipt.projected_session_id = Some(session_id.clone());
            }
        }
        let publication = saved
            .publication
            .as_ref()
            .or(saved.prepared.as_ref())
            .context("publication has no generated candidate")?;
        ensure!(
            !publication.mode.is_encrypted() || publication.source != publication.published,
            "private source commits cannot be notified"
        );
        ensure!(
            publication.destination.hub == executor.owner.issuer,
            "publication executor belongs to another Hub"
        );
        let notification = Notification {
            version: 1,
            notification_id: saved.notification_id.clone(),
            executor,
            repository_id: publication.destination.agent_id.clone(),
            branch: publication.branch.clone(),
            public_commit: publication.published.clone(),
            projected_session_id: publication
                .projected_session_id
                .clone()
                .context("publication has no verified projected session identity")?,
            capture: saved.capture.clone(),
        };
        notification.validate()?;
        if let Some(frozen) = &saved.notification {
            ensure!(
                frozen == &notification,
                "publication notification binding changed"
            );
        } else {
            saved.notification = Some(notification.clone());
            saved.write(&path)?;
        }
        Ok(notification)
    }

    /// The caller validates an authenticated response and its live grant before persisting it.
    pub fn acknowledge(
        repo: &Repo,
        request: &SupervisorPushRequest,
        receipt: &Receipt,
    ) -> Result<()> {
        let (path, _lock) = locked_path(repo, request)?;
        let mut saved = Self::load(repo, request)?.context("publication intent is missing")?;
        receipt.validate(
            saved
                .notification
                .as_ref()
                .context("publication notification is not bound")?,
        )?;
        if let Some(accepted) = &saved.acknowledged {
            ensure!(accepted == receipt, "durable publication receipt changed");
            return Ok(());
        }
        saved.acknowledged = Some(receipt.clone());
        if saved.publication.is_none() {
            saved.publication = saved.prepared.clone();
        }
        saved.write(&path)
    }

    /// Scanning is bounded independently of the number of entries selected for a batch.
    pub fn pending(repo: &Repo, branch: &str, native: &str, runtime: &str) -> Result<Vec<Self>> {
        Ok(Self::records(repo, branch, native, runtime)?
            .into_iter()
            .filter(|entry| entry.acknowledged.is_none())
            .collect())
    }

    /// An unacknowledged candidate keeps a publication in the delivery workflow.
    pub fn pending_for_publication(repo: &Repo, publication: &PublicationReceipt) -> Result<bool> {
        Ok(Self::scan(repo)?.into_iter().any(|entry| {
            entry.acknowledged.is_none()
                && (entry.prepared.as_ref() == Some(publication)
                    || entry.publication.as_ref() == Some(publication))
        }))
    }

    /// A controller can recover durable acknowledgements after losing the RPC response.
    pub fn records(repo: &Repo, branch: &str, native: &str, runtime: &str) -> Result<Vec<Self>> {
        Ok(Self::scan(repo)?
            .into_iter()
            .filter(|saved| {
                saved.request.branch == branch
                    && saved.capture.native_session_id == native
                    && saved.capture.runtime == runtime
            })
            .collect())
    }

    fn family(repo: &Repo, request: &SupervisorPushRequest) -> Result<Vec<Self>> {
        Ok(Self::scan(repo)?
            .into_iter()
            .filter(|saved| {
                saved.request.repository == request.repository
                    && saved.request.branch == request.branch
                    && saved.request.source == request.source
                    && saved.request.destination == request.destination
            })
            .collect())
    }

    fn scan(repo: &Repo) -> Result<Vec<Self>> {
        let directory = repo.common_dir()?.join("agit/rc-publications");
        let paths = match fs::read_dir(&directory) {
            Ok(paths) => paths,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(error) => return Err(error.into()),
        };
        let mut entries = vec![];
        for (index, path) in paths.enumerate() {
            ensure!(
                index <= MAX_ENTRIES,
                "RC publication outbox exceeds its scan limit"
            );
            let path = path?.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let Some(saved) = Self::read(&path)? else {
                continue;
            };
            saved.matches(&saved.request)?;
            ensure!(
                path == saved.record_path(&directory)?,
                "publication intent is stored under another binding"
            );
            entries.push(saved);
        }
        entries.sort_by(|a, b| a.notification_id.cmp(&b.notification_id));
        Ok(entries)
    }

    pub fn verify(
        repo: &Repo,
        request: &SupervisorPushRequest,
        receipt: &PublicationReceipt,
    ) -> Result<()> {
        request.verify(receipt)?;
        let saved = Self::load(repo, request)?.context("publication intent is missing")?;
        ensure!(
            request.notification_id.as_ref() == Some(&saved.notification_id),
            "publication request has no matching notification identity"
        );
        if let Some(published) = saved.publication.as_ref().or(saved.prepared.as_ref()) {
            ensure!(
                published == receipt,
                "publication notification content changed"
            );
        }
        Ok(())
    }

    /// A crash during transport leaves the exact candidate available for remote reconciliation.
    pub fn prepare(
        repo: &Repo,
        request: &SupervisorPushRequest,
        receipt: &PublicationReceipt,
    ) -> Result<()> {
        let (path, _lock) = locked_path(repo, request)?;
        Self::verify(repo, request, receipt)?;
        let mut saved = Self::read(&path)?.context("publication intent is missing")?;
        if saved.prepared.is_none() {
            saved.prepared = Some(receipt.clone());
            saved.write(&path)?;
        }
        Ok(())
    }

    pub fn complete(
        repo: &Repo,
        request: &SupervisorPushRequest,
        receipt: &PublicationReceipt,
    ) -> Result<()> {
        let (path, _lock) = locked_path(repo, request)?;
        Self::verify(repo, request, receipt)?;
        let mut saved = Self::read(&path)?.context("publication intent is missing")?;
        if saved.publication.is_none() {
            saved.prepared = Some(receipt.clone());
            saved.publication = Some(receipt.clone());
            saved.write(&path)?;
        }
        Ok(())
    }

    fn matches(&self, request: &SupervisorPushRequest) -> Result<()> {
        ensure!(
            matches!(self.version, 1 | 2),
            "unsupported publication outbox version"
        );
        self.request.validate()?;
        ensure!(
            self.request.notification_id.as_ref() == Some(&self.notification_id),
            "invalid publication outbox identity"
        );
        ensure!(
            self.request.repository == request.repository
                && self.request.branch == request.branch
                && self.request.source == request.source
                && self.request.destination == request.destination,
            "publication outbox binding changed"
        );
        if let Some(receipt) = &self.publication {
            self.request.verify(receipt)?;
            if let Some(prepared) = &self.prepared {
                ensure!(
                    prepared == receipt,
                    "publication differs from its prepared candidate"
                );
            }
        }
        if let Some(prepared) = &self.prepared {
            self.request.verify(prepared)?;
        }
        if let Some(notification) = &self.notification {
            notification.validate()?;
            let publication = self
                .publication
                .as_ref()
                .or(self.prepared.as_ref())
                .context("notification has no generated publication")?;
            ensure!(
                notification.notification_id == self.notification_id
                    && notification.capture == self.capture
                    && notification.repository_id == publication.destination.agent_id
                    && notification.executor.owner.issuer == publication.destination.hub
                    && notification.branch == publication.branch
                    && notification.public_commit == publication.published
                    && publication.projected_session_id.as_ref()
                        == Some(&notification.projected_session_id)
                    && (!publication.mode.is_encrypted()
                        || notification.public_commit != publication.source),
                "notification differs from the saved publication"
            );
        }
        if let Some(receipt) = &self.acknowledged {
            receipt.validate(
                self.notification
                    .as_ref()
                    .context("receipt has no notification")?,
            )?;
        }
        Ok(())
    }

    fn read(path: &std::path::Path) -> Result<Option<Self>> {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(metadata) => ensure!(
                metadata.file_type().is_file(),
                "publication intent is not a regular file"
            ),
        }
        Ok(Some(serde_json::from_slice(
            &crate::domain::storage::read_bytes_capped(path, MAX_RECEIPT_BYTES)?,
        )?))
    }

    fn record_path(&self, directory: &std::path::Path) -> Result<PathBuf> {
        let mut request = self.request.clone();
        if self.version == 1 {
            request.notification_id = None;
        }
        path_in(directory, &request)
    }

    fn write(&self, path: &std::path::Path) -> Result<()> {
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            bytes.len() <= MAX_RECEIPT_BYTES,
            "publication intent exceeds its limit"
        );
        let directory = path.parent().context("publication directory is missing")?;
        let mut staging = tempfile::NamedTempFile::new_in(directory)?;
        staging.write_all(&bytes)?;
        staging.as_file().sync_all()?;
        staging.persist(path).map_err(|error| error.error)?;
        #[cfg(unix)]
        fs::File::open(directory)?.sync_all()?;
        Ok(())
    }
}

fn path(repo: &Repo, request: &SupervisorPushRequest) -> Result<PathBuf> {
    let directory = repo.common_dir()?.join("agit/rc-publications");
    let selected = path_in(&directory, request)?;
    if request.notification_id.is_some() && !selected.try_exists()? {
        let mut legacy = request.clone();
        legacy.notification_id = None;
        let legacy = path_in(&directory, &legacy)?;
        if let Some(saved) = Entry::read(&legacy)?
            && Some(&saved.notification_id) == request.notification_id.as_ref()
        {
            return Ok(legacy);
        }
    }
    Ok(selected)
}

fn path_in(directory: &std::path::Path, request: &SupervisorPushRequest) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    let key = hex::encode(Sha256::digest(serde_json::to_vec(&(
        &request.repository,
        &request.branch,
        &request.source,
        &request.destination,
    ))?));
    let suffix = request
        .notification_id
        .as_ref()
        .map(|id| uuid::Uuid::parse_str(id).map(|id| format!(".{id}")))
        .transpose()?
        .unwrap_or_default();
    Ok(directory.join(format!("{key}{suffix}.json")))
}

fn check_capacity(directory: &std::path::Path) -> Result<()> {
    ensure!(
        fs::read_dir(directory)?.take(MAX_ENTRIES + 2).count() <= MAX_ENTRIES,
        "RC publication outbox is full; pending receipts must be resolved before publishing"
    );
    Ok(())
}

fn locked_path(repo: &Repo, request: &SupervisorPushRequest) -> Result<(PathBuf, fs::File)> {
    request.validate()?;
    let path = path(repo, request)?;
    let directory = path.parent().context("publication directory is missing")?;
    crate::infra::config::create_state_dir(directory)?;
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options.open(directory.join("mutation.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .context("another process is updating RC publication state; retry")?;
    Ok((path, lock))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Advancing a branch cannot erase an older notification or give its ID different content.
    #[test]
    fn publication_outbox_retains_history_and_refuses_conflicting_evidence() {
        publication_outbox_lifecycle(crate::domain::privacy_receipt::PublicationMode::Encrypted);
    }

    #[test]
    fn ordinary_outbox_retains_pending_evidence_until_a_matching_durable_acknowledgement() {
        publication_outbox_lifecycle(crate::domain::privacy_receipt::PublicationMode::Ordinary);
    }

    fn publication_outbox_lifecycle(mode: crate::domain::privacy_receipt::PublicationMode) {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::open_or_init(directory.path()).unwrap();
        let capture = Capture {
            session_id: "logical".into(),
            native_session_id: "native".into(),
            runtime: "claude-code".into(),
            generation: 1,
            incarnation: None,
            through_seq: None,
        };
        let mut request = SupervisorPushRequest {
            version: 1,
            request_id: uuid::Uuid::new_v4().to_string(),
            repository: "owner/project".into(),
            branch: "s/work".into(),
            source: "a".repeat(40),
            destination: crate::hub::identity::RemoteIdentity::new(
                "https://hub.example",
                "00000000-0000-0000-0000-000000000001",
            )
            .unwrap(),
            notification_id: None,
        };
        let first = Entry::begin(&repo, &mut request, capture.clone()).unwrap();
        let receipt = PublicationReceipt {
            version: if mode.is_encrypted() { 1 } else { 2 },
            mode,
            repository: request.repository.clone(),
            branch: request.branch.clone(),
            source: request.source.clone(),
            published: if mode.is_encrypted() {
                "b".repeat(40)
            } else {
                request.source.clone()
            },
            projected_session_id: Some(format!("agit-{}", "e".repeat(40))),
            destination: request.destination.clone(),
            url: "https://hub.example/owner/project.git".into(),
            policy_digest: mode.is_encrypted().then(|| "policy".into()),
            recipient: mode.is_encrypted().then(|| "recipient".into()),
        };
        Entry::prepare(&repo, &request, &receipt).unwrap();
        let interrupted = Entry::load(&repo, &request).unwrap().unwrap();
        assert_eq!(interrupted.prepared.as_ref(), Some(&receipt));
        assert!(interrupted.publication.is_none());
        Entry::complete(&repo, &request, &receipt).unwrap();
        let mut next = request.clone();
        next.source = "c".repeat(40);
        next.notification_id = None;
        let second = Entry::begin(&repo, &mut next, capture).unwrap();
        assert_ne!(first.notification_id, second.notification_id);
        let restored = Entry::load(&repo, &request).unwrap().unwrap();
        assert_eq!(restored.notification_id, first.notification_id);
        assert_eq!(restored.publication.as_ref(), Some(&receipt));
        let executor = Executor {
            owner: agit_peer::access::Principal {
                issuer: "https://hub.example".into(),
                account_id: "owner".into(),
            },
            device_id: "device".into(),
            credential_epoch: 1,
        };
        let notification = Entry::bind_notification(&repo, &request, executor.clone()).unwrap();
        assert_eq!(notification.public_commit, receipt.published);
        assert_eq!(
            notification.projected_session_id,
            receipt.projected_session_id.clone().unwrap()
        );
        assert_eq!(
            Entry::pending(&repo, "s/work", "native", "claude-code")
                .unwrap()
                .len(),
            2
        );
        let wrong_executor = Executor {
            device_id: "other-device".into(),
            ..executor.clone()
        };
        assert!(Entry::bind_notification(&repo, &request, wrong_executor).is_err());
        let accepted = |notification: &Notification| Receipt {
            version: 1,
            receipt_id: format!("receipt-{}", notification.notification_id),
            notification_id: notification.notification_id.clone(),
            binding_digest: notification.digest().unwrap(),
            repository_id: notification.repository_id.clone(),
            public_commit: notification.public_commit.clone(),
        };
        let ack = accepted(&notification);
        let mut wrong = ack.clone();
        wrong.public_commit = "f".repeat(40);
        assert!(Entry::acknowledge(&repo, &request, &wrong).is_err());
        assert!(
            Entry::load(&repo, &request)
                .unwrap()
                .unwrap()
                .acknowledged
                .is_none()
        );
        Entry::acknowledge(&repo, &request, &ack).unwrap();
        Entry::acknowledge(&repo, &request, &ack).unwrap();
        assert_eq!(
            Entry::load(&repo, &request).unwrap().unwrap().acknowledged,
            Some(ack)
        );
        let remaining = Entry::pending(&repo, "s/work", "native", "claude-code").unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].notification_id, second.notification_id);

        let next_receipt = PublicationReceipt {
            source: next.source.clone(),
            published: if mode.is_encrypted() {
                "d".repeat(40)
            } else {
                next.source.clone()
            },
            ..receipt.clone()
        };
        Entry::prepare(&repo, &next, &next_receipt).unwrap();
        let uncertain = Entry::bind_notification(&repo, &next, executor).unwrap();
        assert!(
            Entry::load(&repo, &next)
                .unwrap()
                .unwrap()
                .publication
                .is_none()
        );
        Entry::acknowledge(&repo, &next, &accepted(&uncertain)).unwrap();
        assert_eq!(
            Entry::load(&repo, &next).unwrap().unwrap().publication,
            Some(next_receipt)
        );
        let mut conflicting = receipt.clone();
        conflicting.published = "d".repeat(40);
        assert!(Entry::complete(&repo, &request, &conflicting).is_err());
        assert_eq!(
            Entry::load(&repo, &request).unwrap().unwrap().publication,
            Some(receipt)
        );
        let next_path = path(&repo, &next).unwrap();
        fs::write(&next_path, b"incomplete JSON").unwrap();
        next.notification_id = None;
        assert!(Entry::begin(&repo, &mut next, second.capture).is_err());
        assert_eq!(fs::read(&next_path).unwrap(), b"incomplete JSON");
    }
}
