//! Immutable Stop requests outlive workers; a saved tip is separate from publication.

use super::merge_archive as persistence;
use anyhow::{Context as _, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};
use std::fs::File;
use std::path::{Path, PathBuf};

/// Settlement watermarks may advance without granting a different native claim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Authority {
    pub runtime: String,
    pub session: String,
    pub binding: Option<super::link::NativeBinding>,
    pub cwd: Option<String>,
    pub owner: Option<String>,
    pub repository: Option<String>,
    pub branch: Option<String>,
    pub privacy: Option<super::session_privacy::SessionPrivacy>,
    pub claimed_by: Option<String>,
    pub materialized: bool,
    pub superseded_by: Option<String>,
    pub archive: bool,
}

impl Authority {
    pub fn of(link: &super::link::Link) -> Self {
        Self {
            runtime: link.source.clone(),
            session: link.session_id.clone(),
            binding: link.native_binding.clone(),
            cwd: link.cwd.clone(),
            owner: link.owner.clone(),
            repository: link.agent.clone(),
            branch: link.branch.clone(),
            privacy: link.session_privacy.clone(),
            claimed_by: link.claimed_by.clone(),
            materialized: link.baseline_bytes.is_some(),
            superseded_by: link.superseded_by.clone(),
            archive: link.merge_archive.is_some(),
        }
    }

    /// Only settlement may discover an absent cwd without changing the route's epoch.
    pub fn admits_settlement(&self, current: &Self) -> bool {
        let mut current = current.clone();
        if self.cwd.is_none() {
            current.cwd = None;
        }
        self == &current
    }
}

pub fn root(store: &super::store::Store) -> PathBuf {
    store.root().with_extension("stop-requests")
}

pub fn for_session(store: &super::store::Store, runtime: &str, session: &str) -> Journal {
    let mut digest = Sha256::new();
    digest.update(runtime.as_bytes());
    digest.update([0]);
    digest.update(session.as_bytes());
    Journal::at(root(store).join(hex::encode(digest.finalize())))
}

pub(crate) fn policy_epoch(scope: &str) -> crate::Result<Option<String>> {
    let directory = crate::infra::config::store_root()?.with_extension("stop-policies");
    if !persistence::authority_directory_exists(&directory)? {
        return Ok(None);
    }
    let path = directory.join(hex::encode(Sha256::digest(scope.as_bytes())));
    persistence::read_private_bytes(&path, 128)?
        .map(|bytes| {
            let value = std::str::from_utf8(&bytes)?;
            RequestId::parse(value)?;
            Ok(value.to_owned())
        })
        .transpose()
}

pub(crate) fn destination_policy(owner: &str, name: &str) -> crate::Result<String> {
    Ok(format!(
        "destination:{}/{}/{}",
        crate::infra::config::hub_host_key(&crate::infra::config::hub_url())?,
        owner.to_ascii_lowercase(),
        name.to_ascii_lowercase()
    ))
}

/// Revocation lands before a policy mutation, so restoring its old value cannot revive work.
pub(crate) fn revoke_policy(scope: &str) -> crate::Result<()> {
    let store = super::store::Store::at(crate::infra::config::store_root()?);
    if !persistence::authority_directory_exists(&root(&store))? {
        return Ok(());
    }
    let directory = store.root().with_extension("stop-policies");
    persistence::create_namespace(&directory)?;
    let path = directory.join(hex::encode(Sha256::digest(scope.as_bytes())));
    persistence::durable_publish_bytes(&path, uuid::Uuid::new_v4().to_string().as_bytes(), true)
}

pub fn epoch(journal: &Journal) -> crate::Result<Option<String>> {
    if !persistence::authority_directory_exists(journal.directory())? {
        return Ok(None);
    }
    persistence::read_private_bytes(&journal.directory.join("authority.epoch"), 128)?
        .map(|bytes| {
            let value = std::str::from_utf8(&bytes)?;
            RequestId::parse(value)?;
            Ok(value.to_owned())
        })
        .transpose()
}

/// Cancellation precedes a route mutation and survives restoring an earlier Link image.
pub(crate) fn changing_authority(
    store: &super::store::Store,
    runtime: &str,
    session: &str,
    before: Option<&super::link::Link>,
    after: Option<&super::link::Link>,
) -> crate::Result<()> {
    if before.map(Authority::of) == after.map(Authority::of) {
        return Ok(());
    }
    invalidate_authority(store, runtime, session)
}

/// Restoring raw Link evidence also revokes requests admitted under the replaced claim.
pub(crate) fn invalidate_authority(
    store: &super::store::Store,
    runtime: &str,
    session: &str,
) -> crate::Result<()> {
    let journal = for_session(store, runtime, session);
    if !persistence::authority_directory_exists(journal.directory())? {
        return Ok(());
    }
    persistence::durable_publish_bytes(
        &journal.directory.join("authority.epoch"),
        uuid::Uuid::new_v4().to_string().as_bytes(),
        true,
    )
}

const VERSION: u32 = 1;
const RECORD_LIMIT: u64 = 64 * 1024;
const PENDING_LIMIT: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(String);

impl RequestId {
    pub fn parse(value: &str) -> crate::Result<Self> {
        let id = uuid::Uuid::parse_str(value).context("invalid Stop request identity")?;
        ensure!(
            id.to_string() == value,
            "noncanonical Stop request identity"
        );
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockedBy {
    AuthorityChanged,
    InputChanged,
    PrivacyPending,
    SaveFailed,
    PublicationFailed,
    InterruptedSave,
    InterruptedAdoption,
    AutomaticSavingOff,
    Delegated,
    MissingLink,
    MergePaused,
    Sealed,
    NativeCompletionPending,
    PublicationOff,
    RepositoryIdentityUnavailable,
    MemoryIncomplete,
}

/// Transcript coverage alone cannot establish the outcome of memory collection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum MemoryOutcome {
    NotApplicable,
    TrackingOff,
    Collected {
        changed: bool,
        withheld: bool,
        warnings: bool,
    },
    Failed,
    Superseded,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Phase {
    Accepted,
    Saving {
        parent: Option<String>,
    },
    Saved {
        tip: String,
        memory: MemoryOutcome,
    },
    Publishing {
        tip: String,
        memory: MemoryOutcome,
    },
    Published {
        tip: String,
        memory: MemoryOutcome,
    },
    Blocked {
        reason: BlockedBy,
        tip: Option<String>,
        memory: Option<MemoryOutcome>,
    },
}

impl Phase {
    /// A complete local save needs no automatic retry when publication is disabled or unbound.
    pub fn completed_tip(&self) -> Option<&str> {
        match self {
            Self::Published { tip, .. }
            | Self::Blocked {
                reason: BlockedBy::PublicationOff | BlockedBy::RepositoryIdentityUnavailable,
                tip: Some(tip),
                memory:
                    Some(
                        MemoryOutcome::NotApplicable
                        | MemoryOutcome::TrackingOff
                        | MemoryOutcome::Collected { .. },
                    ),
            } => Some(tip),
            _ => None,
        }
    }

    fn validate(&self) -> crate::Result<()> {
        let tip = match self {
            Self::Accepted => None,
            Self::Saving { parent } => parent.as_deref(),
            Self::Saved { tip, .. }
            | Self::Publishing { tip, .. }
            | Self::Published { tip, .. } => Some(tip.as_str()),
            Self::Blocked { tip, .. } => tip.as_deref(),
        };
        if let Some(tip) = tip {
            ensure!(
                matches!(tip.len(), 40 | 64) && tip.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "Stop request has an invalid commit identity"
            );
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope<T> {
    version: u32,
    id: RequestId,
    payload: T,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Progress {
    version: u32,
    request_sha256: String,
    phase: Phase,
}

pub struct Request<T> {
    pub id: RequestId,
    pub payload: T,
    pub phase: Phase,
    digest: String,
}

/// A journal names one native session. Producers never take its worker lock.
pub struct Journal {
    directory: PathBuf,
}

impl Journal {
    pub fn at(directory: PathBuf) -> Self {
        Self { directory }
    }

    /// The parent must already be an owner-controlled state directory.
    pub fn create(&self) -> crate::Result<()> {
        persistence::create_namespace(&self.directory)
    }

    fn path(&self, id: &RequestId, extension: &str) -> crate::Result<PathBuf> {
        RequestId::parse(&id.0)?;
        Ok(self.directory.join(format!("{}.{extension}", id.0)))
    }

    pub fn enqueue<T: Serialize>(&self, payload: T) -> crate::Result<RequestId> {
        self.admit()?.enqueue(payload)
    }

    /// A producer can recheck its authority under the same lock that installs its request.
    pub fn admit(&self) -> crate::Result<Admission<'_>> {
        self.create()?;
        let admission = persistence::open_file(&self.directory.join("admission.lock"), true, true)?;
        fs2::FileExt::try_lock_exclusive(&admission)
            .context("Stop request admission is busy; this request was not accepted")?;
        Ok(Admission {
            journal: self,
            _lock: admission,
        })
    }

    fn enqueue_admitted<T: Serialize>(&self, payload: T) -> crate::Result<RequestId> {
        ensure!(
            self.pending()?.len() < PENDING_LIMIT,
            "automatic saving has too many pending Stop requests; recover them before retrying"
        );
        let id = RequestId(uuid::Uuid::now_v7().to_string());
        let bytes = serde_json::to_vec(&Envelope {
            version: VERSION,
            id: id.clone(),
            payload,
        })?;
        ensure!(
            bytes.len() as u64 <= RECORD_LIMIT,
            "Stop request exceeds its metadata budget"
        );
        persistence::durable_publish_bytes(&self.path(&id, "json")?, &bytes, false)?;
        Ok(id)
    }

    /// A bad request remains independently inspectable instead of hiding other requests.
    pub fn pending(&self) -> crate::Result<Vec<RequestId>> {
        if !persistence::authority_directory_exists(&self.directory)? {
            return Ok(Vec::new());
        }
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(&self.directory)? {
            let path = entry?.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                anyhow::bail!("Stop request filename is not readable");
            };
            ids.push(RequestId::parse(stem)?);
            ensure!(
                ids.len() <= PENDING_LIMIT,
                "Stop request journal exceeds its inspection budget"
            );
        }
        ids.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(ids)
    }

    pub fn read<T: DeserializeOwned>(&self, id: &RequestId) -> crate::Result<Request<T>> {
        self.read_record(id, "json")
    }

    /// Retired evidence is readable without returning it to the pending queue.
    pub fn read_completed<T: DeserializeOwned>(&self, id: &RequestId) -> crate::Result<Request<T>> {
        let request = self.read_record(id, "done")?;
        ensure!(
            request.phase.completed_tip().is_some(),
            "retired Stop request has no complete outcome"
        );
        Ok(request)
    }

    fn read_record<T: DeserializeOwned>(
        &self,
        id: &RequestId,
        extension: &str,
    ) -> crate::Result<Request<T>> {
        let bytes = persistence::read_private_bytes(&self.path(id, extension)?, RECORD_LIMIT)?
            .context("Stop request is missing; its outcome is unknown")?;
        let envelope: Envelope<T> = serde_json::from_slice(&bytes)?;
        ensure!(
            envelope.version == VERSION && envelope.id == *id,
            "Stop request format or identity is unsupported; evidence is retained"
        );
        let digest = hex::encode(Sha256::digest(&bytes));
        let phase = match persistence::read_private_bytes(&self.path(id, "state")?, RECORD_LIMIT)? {
            None => Phase::Accepted,
            Some(bytes) => {
                let progress: Progress = serde_json::from_slice(&bytes)?;
                ensure!(
                    progress.version == VERSION && progress.request_sha256 == digest,
                    "Stop request progress does not match its immutable request"
                );
                progress.phase.validate()?;
                progress.phase
            }
        };
        Ok(Request {
            id: id.clone(),
            payload: envelope.payload,
            phase,
            digest,
        })
    }

    pub fn try_own(&self) -> crate::Result<Option<Owned<'_>>> {
        self.create()?;
        let file = persistence::open_file(&self.directory.join("worker.lock"), true, true)?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Some(Owned {
                journal: self,
                _lock: file,
            })),
            Err(error) if crate::infra::local_state::is_contended(&error) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn owned(&self) -> crate::Result<bool> {
        let path = self.directory.join("worker.lock");
        if !path.try_exists()? {
            return Ok(false);
        }
        let file = persistence::open_file(&path, false, false)?;
        match fs2::FileExt::try_lock_shared(&file) {
            Ok(()) => Ok(false),
            Err(error) if crate::infra::local_state::is_contended(&error) => Ok(true),
            Err(error) => Err(error.into()),
        }
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }
}

/// Admission does not acquire worker ownership or wait for a worker to finish saving.
pub struct Admission<'a> {
    journal: &'a Journal,
    _lock: File,
}

impl Admission<'_> {
    pub fn enqueue<T: Serialize>(self, payload: T) -> crate::Result<RequestId> {
        self.journal.enqueue_admitted(payload)
    }
}

/// Progress changes retain the worker lock and compare the request and previous outcome.
pub struct Owned<'a> {
    journal: &'a Journal,
    _lock: File,
}

impl Owned<'_> {
    pub fn transition<T: DeserializeOwned>(
        &self,
        expected: &Request<T>,
        next: Phase,
    ) -> crate::Result<Request<T>> {
        next.validate()?;
        let current = self.journal.read::<T>(&expected.id)?;
        ensure!(
            current.digest == expected.digest && current.phase == expected.phase,
            "Stop request changed before its outcome could be recorded"
        );
        let bytes = serde_json::to_vec(&Progress {
            version: VERSION,
            request_sha256: current.digest.clone(),
            phase: next.clone(),
        })?;
        persistence::durable_publish_bytes(
            &self.journal.path(&expected.id, "state")?,
            &bytes,
            true,
        )?;
        Ok(Request {
            phase: next,
            ..current
        })
    }

    /// Completion retains the immutable request beside its outcome before removing pending work.
    pub fn retire<T: DeserializeOwned>(&self, expected: &Request<T>) -> crate::Result<()> {
        let current = self.journal.read::<T>(&expected.id)?;
        ensure!(
            current.digest == expected.digest
                && current.phase == expected.phase
                && current.phase.completed_tip().is_some(),
            "only a verified complete request can leave the pending queue"
        );
        let path = self.journal.path(&expected.id, "json")?;
        let bytes = persistence::read_private_bytes(&path, RECORD_LIMIT)?
            .context("Stop request disappeared before acknowledgement")?;
        ensure!(
            hex::encode(Sha256::digest(&bytes)) == current.digest,
            "Stop request changed before acknowledgement"
        );
        persistence::durable_retire_transition_bytes(
            &path,
            &self.journal.path(&expected.id, "done")?,
            &bytes,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Route restoration cannot revive a request, but settlement watermarks do not revoke it.
    #[test]
    fn authority_revocation_survives_route_restoration() {
        let directory = tempfile::tempdir().unwrap();
        let store = super::super::store::Store::at(directory.path().join("store"));
        persistence::create_namespace(&root(&store)).unwrap();
        let mut link = super::super::link::Link::new("codex", "session", None);
        let queue = for_session(&store, &link.source, &link.session_id);
        queue.create().unwrap();
        let original = link.clone();
        let admitted = epoch(&queue).unwrap();
        link.native_checkpoint = Some(super::super::link::NativeCheckpoint {
            tip: "a".repeat(40),
            bytes: 17,
            sha256: "b".repeat(64),
        });
        changing_authority(&store, "codex", "session", Some(&original), Some(&link)).unwrap();
        assert_eq!(epoch(&queue).unwrap(), admitted);
        let settled = link.clone();
        link.branch = Some("another-session".into());
        changing_authority(&store, "codex", "session", Some(&settled), Some(&link)).unwrap();
        let revoked = epoch(&queue).unwrap();
        assert_ne!(revoked, admitted);
        changing_authority(&store, "codex", "session", Some(&link), Some(&original)).unwrap();
        assert_ne!(epoch(&queue).unwrap(), admitted);
        assert_ne!(epoch(&queue).unwrap(), revoked);
    }

    /// Completion cannot consume another Stop; local-only outcomes release capacity without
    /// losing evidence, while incomplete memory or failed publication remains recoverable.
    #[test]
    fn worker_completion_retires_only_its_immutable_request() {
        let root = tempfile::tempdir().unwrap();
        let journal = Journal::at(root.path().join("requests"));
        let first = journal.enqueue("first completion").unwrap();
        let owner = journal.try_own().unwrap().unwrap();
        let admission = journal.admit().unwrap();
        assert!(journal.admit().is_err());
        assert!(journal.enqueue("contending completion").is_err());
        assert_eq!(journal.pending().unwrap(), vec![first.clone()]);
        let second = admission.enqueue("second completion").unwrap();
        assert!(journal.admit().is_ok());
        assert!(journal.try_own().unwrap().is_none());
        let request = journal.read::<String>(&first).unwrap();
        let tip = "a".repeat(40);
        let saved = owner
            .transition(
                &request,
                Phase::Saved {
                    tip: tip.clone(),
                    memory: MemoryOutcome::NotApplicable,
                },
            )
            .unwrap();
        assert!(owner.retire(&saved).is_err());
        assert!(owner.transition(&request, Phase::Accepted).is_err());
        let published = owner
            .transition(
                &saved,
                Phase::Published {
                    tip,
                    memory: MemoryOutcome::NotApplicable,
                },
            )
            .unwrap();
        owner.retire(&published).unwrap();
        assert_eq!(journal.pending().unwrap(), vec![second.clone()]);
        assert_eq!(
            journal.read::<String>(&second).unwrap().phase,
            Phase::Accepted
        );
        assert!(journal.path(&first, "done").unwrap().is_file());
        let mut local = journal.read::<String>(&second).unwrap();
        for phase in [
            Phase::Blocked {
                reason: BlockedBy::PublicationFailed,
                tip: Some("b".repeat(40)),
                memory: Some(MemoryOutcome::NotApplicable),
            },
            Phase::Blocked {
                reason: BlockedBy::PublicationOff,
                tip: Some("b".repeat(40)),
                memory: Some(MemoryOutcome::Failed),
            },
        ] {
            local = owner.transition(&local, phase).unwrap();
            assert!(owner.retire(&local).is_err());
        }
        let complete = owner
            .transition(
                &local,
                Phase::Blocked {
                    reason: BlockedBy::PublicationOff,
                    tip: Some("b".repeat(40)),
                    memory: Some(MemoryOutcome::NotApplicable),
                },
            )
            .unwrap();
        owner.retire(&complete).unwrap();
        assert!(journal.pending().unwrap().is_empty());
        let evidence = journal.read_completed::<String>(&second).unwrap();
        assert_eq!(evidence.payload, "second completion");
        assert_eq!(evidence.phase, complete.phase);
        assert!(journal.enqueue("next completion").is_ok());
    }

    /// A stopped worker or unreadable request is retained without blocking another completion.
    #[test]
    fn interrupted_and_unknown_requests_remain_distinct_from_saved_work() {
        let root = tempfile::tempdir().unwrap();
        let journal = Journal::at(root.path().join("requests"));
        let interrupted = journal.enqueue("interrupted").unwrap();
        let unknown = journal.enqueue("unknown format").unwrap();
        let ready = journal.enqueue("ready").unwrap();
        {
            let owner = journal.try_own().unwrap().unwrap();
            let request = journal.read::<String>(&interrupted).unwrap();
            owner
                .transition(&request, Phase::Saving { parent: None })
                .unwrap();
        }
        let path = journal.path(&unknown, "json").unwrap();
        let mut envelope: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        envelope["version"] = serde_json::json!(VERSION + 1);
        persistence::durable_publish_bytes(&path, &serde_json::to_vec(&envelope).unwrap(), true)
            .unwrap();
        assert!(journal.try_own().unwrap().is_some());
        assert!(matches!(
            journal.read::<String>(&interrupted).unwrap().phase,
            Phase::Saving { .. }
        ));
        assert!(journal.read::<String>(&unknown).is_err());
        assert!(path.exists());
        assert_eq!(
            journal.read::<String>(&ready).unwrap().phase,
            Phase::Accepted
        );
        assert_eq!(journal.pending().unwrap().len(), 3);
    }
}
