//! Local source-to-publication acknowledgements never become part of the outgoing Git tree.

use super::{repo::Repo, storage};
use crate::hub::identity::RemoteIdentity;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
};

pub const SUPERVISOR_RESULT_ENV: &str = "AGIT_RC_SUPERVISOR_PUSH_RESULT";
const MAX_RECEIPT_BYTES: usize = 64 * 1024;
/// Where [`save_acknowledged_tags`] keeps a branch's tag listing, beside its receipt.
const ACKNOWLEDGED_TAGS: &str = "agit/privacy-published-tags";

pub mod outbox;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationMode {
    Ordinary,
    SessionPlain,
    SessionOwnerV2,
}

impl PublicationMode {
    pub fn uses_generated_history(&self) -> bool {
        *self != Self::Ordinary
    }
    pub fn is_encrypted(&self) -> bool {
        *self == Self::SessionOwnerV2
    }

    /// Every receipt declares its scheme and carries that scheme's exact bindings.
    pub fn validate_bindings(
        &self,
        version: u32,
        policy: Option<&str>,
        recipient: Option<&str>,
    ) -> Result<()> {
        match self {
            Self::SessionPlain => ensure!(
                version == 3 && policy.is_none() && recipient.is_none(),
                "plain session receipt must not carry encrypted bindings"
            ),
            Self::SessionOwnerV2 => {
                ensure!(version == 3, "unsupported session-owner receipt version");
                super::privacy_envelope::validate_digest(
                    policy.context("session receipt has no policy")?,
                )?;
                super::privacy_envelope::validate_digest(
                    recipient.context("session receipt has no owner key")?,
                )?;
            }
            Self::Ordinary => ensure!(
                version == 2 && policy.is_none() && recipient.is_none(),
                "ordinary publication must not carry encrypted-only bindings"
            ),
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationReceipt {
    pub version: u32,
    pub mode: PublicationMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_snapshot: Option<super::session_privacy::AcceptedSnapshot>,
    pub repository: String,
    pub branch: String,
    pub source: String,
    pub published: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projected_session_id: Option<String>,
    pub destination: RemoteIdentity,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
}

/// A receipt's published history, held open while it answers ancestry questions.
pub(crate) struct PublishedHistory {
    store: Repo,
    published: String,
    _lock: Option<std::fs::File>,
}

impl PublishedHistory {
    /// Whether `commit` is in this history: one bounded question, not a set of every published
    /// ancestor, so it answers however long the history grows.
    pub(crate) fn contains(&self, commit: &str) -> Result<bool> {
        super::repo::publication::is_raw_ancestor(&self.store, commit, &self.published)
    }
}

impl PublicationReceipt {
    pub(crate) fn session_id(&self, repo: &Repo) -> Result<String> {
        self.validate()?;
        if self.mode.uses_generated_history() {
            return super::privacy_git::receipt_session_id(repo, self);
        }
        let metadata = storage::metadata_local(repo.root(), &self.published)?;
        ensure!(
            metadata.is_session_line() && super::meta::is_bare_id(&metadata.session),
            "ordinary publication receipt does not identify a settled session"
        );
        Ok(metadata.session)
    }

    /// Whether the published history this receipt names contains `commit`.
    pub(crate) fn reaches(&self, repo: &Repo, commit: &str) -> Result<bool> {
        self.history(repo)?.contains(commit)
    }

    /// The published history this receipt names, opened once for several questions: generated
    /// history lives in its own store, which is located and locked here.
    pub(crate) fn history(&self, repo: &Repo) -> Result<PublishedHistory> {
        self.validate()?;
        let (store, lock) = if self.mode.uses_generated_history() {
            let (store, lock) = super::privacy_git::receipt_store(repo, self)?;
            (store, Some(lock))
        } else {
            (repo.clone(), None)
        };
        Ok(PublishedHistory {
            store,
            published: self.published.clone(),
            _lock: lock,
        })
    }

    pub fn validate(&self) -> Result<()> {
        self.mode.validate_bindings(
            self.version,
            self.policy_digest.as_deref(),
            self.recipient.as_deref(),
        )?;
        if self.mode == PublicationMode::SessionOwnerV2 {
            let snapshot = self
                .session_snapshot
                .as_ref()
                .context("session receipt has no accepted snapshot binding")?;
            snapshot.validate()?;
            ensure!(
                snapshot.binding.repository_id == self.destination.agent_id
                    && snapshot.published_commit == self.published
                    && (snapshot.binding.source_commit == self.source
                        || self.source == self.published)
                    && self.projected_session_id.as_deref()
                        == Some(&snapshot.binding.published_session_id)
                    && self.recipient.as_deref()
                        == Some(&snapshot.binding.owner_key.fingerprint()?),
                "session receipt differs from its accepted snapshot authority"
            );
        } else {
            ensure!(
                self.session_snapshot.is_none(),
                "publication receipt has an unexpected session authority"
            );
        }
        ensure!(
            self.mode.uses_generated_history() || self.source == self.published,
            "ordinary publication must retain the source commit identity"
        );
        ensure!(
            self.mode != PublicationMode::SessionPlain || self.projected_session_id.is_some(),
            "plain session receipt requires its logical session identity"
        );
        crate::domain::repo::valid_branch_name(&self.branch)?;
        validate_repository(&self.repository)?;
        ensure!(
            valid_oid(&self.source) && valid_oid(&self.published),
            "invalid publication receipt object ID"
        );
        if let Some(session) = &self.projected_session_id {
            ensure!(
                crate::domain::meta::is_bare_id(session),
                "invalid projected session identity"
            );
        }
        ensure!(
            RemoteIdentity::new(&self.destination.hub, &self.destination.agent_id)?
                == self.destination,
            "noncanonical publication receipt identity"
        );
        ensure!(!self.url.is_empty(), "incomplete publication receipt");
        Ok(())
    }

    pub fn save(&self, repo: &Repo) -> Result<()> {
        // The tag listing recorded with the receipt this one replaces vouches for nothing now; a
        // publication that knows its listing records it again once this receipt is saved.
        forget_acknowledged_tags(repo, &self.branch)?;
        self.save_at(&receipt_path(repo, &self.branch)?)
    }

    fn save_at(&self, path: &Path) -> Result<()> {
        self.validate()?;
        persist(path, &serde_json::to_vec(self)?)
    }

    pub fn load(repo: &Repo, branch: &str) -> Result<Option<Self>> {
        Self::load_at(&receipt_path(repo, branch)?, branch)
    }

    fn load_at(path: &Path, branch: &str) -> Result<Option<Self>> {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(metadata) => ensure!(
                metadata.file_type().is_file(),
                "publication receipt is not a regular file"
            ),
        }
        let saved: Self =
            serde_json::from_slice(&storage::read_bytes_capped(path, MAX_RECEIPT_BYTES)?)?;
        saved.validate()?;
        ensure!(
            saved.branch == branch,
            "publication receipt belongs to another branch"
        );
        Ok(Some(saved))
    }

    pub fn matches(
        &self,
        repo: &Repo,
        branch: &str,
        source: &str,
        destination: &RemoteIdentity,
    ) -> Result<bool> {
        self.validate()?;
        Ok(self.branch == branch
            && self.source == source
            && self.destination == *destination
            && repo.remote_url().as_deref() == Some(self.url.as_str()))
    }
}

fn receipt_path(repo: &Repo, branch: &str) -> Result<PathBuf> {
    branch_record_path(repo, "agit/privacy-published", branch)
}

fn branch_record_path(repo: &Repo, directory: &str, branch: &str) -> Result<PathBuf> {
    use sha2::Digest;
    let key = hex::encode(sha2::Sha256::digest(branch.as_bytes()));
    Ok(repo
        .common_dir()?
        .join(directory)
        .join(format!("{key}.json")))
}

fn persist(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("publication receipt has no directory")?;
    crate::infra::config::create_state_dir(parent)?;
    let mut staging = tempfile::NamedTempFile::new_in(parent)?;
    staging.write_all(bytes)?;
    staging.as_file().sync_all()?;
    staging.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// The local tags a publication is frozen from, as one digest of their names and objects.
///
/// A publication carries every tag its heads reach, but its receipt names only the branch tip, so
/// a branch whose tip is unchanged still owes a tag added since its publication. Equal digests
/// mean no tag was added, moved or removed.
pub(crate) fn tag_listing<'a>(tags: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    use sha2::Digest;
    let sorted: std::collections::BTreeMap<&str, &str> = tags.into_iter().collect();
    let mut digest = sha2::Sha256::new();
    for (name, oid) in sorted {
        digest.update(format!("{oid} {name}\n").as_bytes());
    }
    hex::encode(digest.finalize())
}

/// [`tag_listing`] of `repo`'s tags as they are now.
pub(crate) fn current_tag_listing(repo: &Repo) -> Result<String> {
    let mut tags = Vec::new();
    repo.git_stream_split_inspection(
        &[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/tags",
        ],
        b'\n',
        crate::infra::local_git::Deadline::new(),
        |line| {
            if !line.is_empty() {
                let line = std::str::from_utf8(line).context("tag listing is not valid UTF-8")?;
                let (oid, name) = line.split_once(' ').context("tag listing is malformed")?;
                tags.push((name.to_owned(), oid.to_owned()));
            }
            Ok(())
        },
    )?;
    Ok(tag_listing(
        tags.iter().map(|(name, oid)| (name.as_str(), oid.as_str())),
    ))
}

/// The tag listing a branch's acknowledged publication was frozen from. It is kept beside the
/// receipt, not in it: receipts are read with unknown fields refused, so a field there makes every
/// agit that predates it refuse the receipt.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcknowledgedTags {
    branch: String,
    source: String,
    tags: String,
}

/// Record the tag listing `receipt`'s publication was frozen from, once the receipt is saved.
pub(crate) fn save_acknowledged_tags(
    repo: &Repo,
    receipt: &PublicationReceipt,
    tags: &str,
) -> Result<()> {
    let record = AcknowledgedTags {
        branch: receipt.branch.clone(),
        source: receipt.source.clone(),
        tags: tags.to_owned(),
    };
    persist(
        &branch_record_path(repo, ACKNOWLEDGED_TAGS, &receipt.branch)?,
        &serde_json::to_vec(&record)?,
    )
}

/// Drop `branch`'s recorded tag listing, so the next push asks the Hub about the branch: for
/// anything that makes the branch owe a ref its tip and tags do not show.
pub(crate) fn forget_acknowledged_tags(repo: &Repo, branch: &str) -> Result<()> {
    match fs::remove_file(branch_record_path(repo, ACKNOWLEDGED_TAGS, branch)?) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

/// The tag listing recorded for `branch`'s acknowledged publication of `source`. `None` when the
/// record names another source, as when an agit that keeps no listing published the branch since.
pub(crate) fn acknowledged_tags(repo: &Repo, branch: &str, source: &str) -> Result<Option<String>> {
    let path = branch_record_path(repo, ACKNOWLEDGED_TAGS, branch)?;
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(metadata) => ensure!(
            metadata.file_type().is_file(),
            "acknowledged tag listing is not a regular file"
        ),
    }
    let record: AcknowledgedTags =
        serde_json::from_slice(&storage::read_bytes_capped(&path, MAX_RECEIPT_BYTES)?)?;
    Ok((record.branch == branch && record.source == source).then_some(record.tags))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupervisorPushRequest {
    pub version: u32,
    pub request_id: String,
    pub repository: String,
    pub branch: String,
    pub source: String,
    pub destination: RemoteIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_id: Option<String>,
}

impl SupervisorPushRequest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1 && uuid::Uuid::parse_str(&self.request_id).is_ok(),
            "invalid supervisor publication request"
        );
        ensure!(valid_oid(&self.source), "invalid supervisor source commit");
        if let Some(id) = &self.notification_id {
            ensure!(
                uuid::Uuid::parse_str(id).is_ok(),
                "invalid publication notification ID"
            );
        }
        crate::domain::repo::valid_branch_name(&self.branch)?;
        validate_repository(&self.repository)?;
        ensure!(
            RemoteIdentity::new(&self.destination.hub, &self.destination.agent_id)?
                == self.destination,
            "noncanonical supervisor destination"
        );
        Ok(())
    }

    pub fn verify(&self, receipt: &PublicationReceipt) -> Result<()> {
        self.validate()?;
        receipt.validate()?;
        ensure!(
            self.repository == receipt.repository
                && self.branch == receipt.branch
                && self.source == receipt.source
                && self.destination == receipt.destination,
            "publication result differs from the supervisor request"
        );
        Ok(())
    }

    pub fn read_result(&self, path: &Path) -> Result<PublicationReceipt> {
        let result = self.read_reply(path)?;
        ensure!(
            result.candidate.is_none(),
            "publication candidate requires durable outbox verification"
        );
        Ok(result.publication)
    }

    pub fn read_managed_result(
        &self,
        repo: &Repo,
        path: &Path,
    ) -> Result<(Self, PublicationReceipt)> {
        let result = self.read_reply(path)?;
        ensure!(
            self.notification_id.is_some(),
            "publication result has no managed intent"
        );
        let original = outbox::Entry::load(repo, self)?.context("publication intent is missing")?;
        let selected = result.candidate.unwrap_or_else(|| self.clone());
        if selected != *self {
            let saved = outbox::Entry::load(repo, &selected)?
                .context("publication candidate is missing")?;
            ensure!(
                saved.capture == original.capture
                    && saved.publication.as_ref() == Some(&result.publication),
                "publication result differs from its durable candidate"
            );
        }
        outbox::Entry::verify(repo, &selected, &result.publication)?;
        Ok((selected, result.publication))
    }

    fn read_reply(&self, path: &Path) -> Result<SupervisorPushResult> {
        let result: SupervisorPushResult =
            serde_json::from_slice(&storage::read_bytes_capped(path, MAX_RECEIPT_BYTES)?)?;
        ensure!(
            result.request == *self,
            "publication result belongs to another supervisor attempt"
        );
        self.verify(&result.publication)?;
        if let Some(candidate) = &result.candidate {
            candidate.validate()?;
            let mut original = candidate.clone();
            original.notification_id = self.notification_id.clone();
            ensure!(
                self.notification_id.is_some()
                    && candidate.notification_id.is_some()
                    && original == *self,
                "publication candidate belongs to another supervisor attempt"
            );
        }
        Ok(result)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SupervisorPushResult {
    request: SupervisorPushRequest,
    publication: PublicationReceipt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    candidate: Option<SupervisorPushRequest>,
}

/// The caller owns the private file. Keeping its descriptor pins the reply to the captured
/// request; a cancelled, declined or interrupted publication leaves no success-shaped result.
pub(crate) struct SupervisorReply {
    file: fs::File,
    pub request: SupervisorPushRequest,
    candidate: Option<SupervisorPushRequest>,
}

impl SupervisorReply {
    pub fn from_env() -> Result<Option<Self>> {
        let Some(path) = std::env::var_os(SUPERVISOR_RESULT_ENV) else {
            return Ok(None);
        };
        ensure!(
            fs::symlink_metadata(&path)?.file_type().is_file(),
            "supervisor result is not a regular file"
        );
        let mut file = fs::OpenOptions::new().read(true).write(true).open(path)?;
        ensure!(
            file.metadata()?.is_file(),
            "supervisor result is not a regular file"
        );
        let mut bytes = Vec::new();
        (&mut file)
            .take((MAX_RECEIPT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_RECEIPT_BYTES,
            "supervisor request exceeds its limit"
        );
        let request: SupervisorPushRequest = serde_json::from_slice(&bytes)?;
        request.validate()?;
        Ok(Some(Self {
            file,
            request,
            candidate: None,
        }))
    }

    pub fn prepare(&mut self, repo: &Repo, publication: &PublicationReceipt) -> Result<()> {
        self.request.verify(publication)?;
        if self.request.notification_id.is_some() {
            let selected = outbox::Entry::prepare_candidate(repo, &self.request, publication)?;
            self.candidate = (selected != self.request).then_some(selected);
        }
        Ok(())
    }

    pub fn complete(mut self, repo: &Repo, publication: PublicationReceipt) -> Result<()> {
        self.request.verify(&publication)?;
        if self.request.notification_id.is_some() {
            let selected = self.candidate.as_ref().unwrap_or(&self.request);
            outbox::Entry::complete(repo, selected, &publication)?;
        }
        let bytes = serde_json::to_vec(&SupervisorPushResult {
            request: self.request,
            publication,
            candidate: self.candidate,
        })?;
        self.file.rewind()?;
        self.file.set_len(0)?;
        self.file.write_all(&bytes)?;
        self.file.sync_all()?;
        Ok(())
    }
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_repository(repository: &str) -> Result<()> {
    let (owner, name) = repository
        .split_once('/')
        .context("publication receipt needs owner/repository")?;
    ensure!(
        !owner.is_empty()
            && !name.is_empty()
            && !name.contains('/')
            && repository.trim() == repository
            && !repository.chars().any(char::is_control),
        "invalid publication repository"
    );
    Ok(())
}

#[cfg(test)]
pub(crate) fn bind_session_fixture(mut receipt: PublicationReceipt) -> PublicationReceipt {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/privacy-session-owner-envelope.json"
    ))
    .unwrap();
    let mut snapshot: super::session_privacy::AcceptedSnapshot =
        serde_json::from_value(fixture["content_key_response"]["snapshot"].clone()).unwrap();
    snapshot.binding.repository_id = receipt.destination.agent_id.clone();
    snapshot.binding.source_commit = receipt.source.clone();
    snapshot.published_commit = receipt.published.clone();
    snapshot.binding.published_session_id = receipt.projected_session_id.clone().unwrap();
    snapshot.envelope_digest = super::privacy_envelope::digest_bytes(receipt.published.as_bytes());
    receipt.version = 3;
    receipt.mode = PublicationMode::SessionOwnerV2;
    receipt.policy_digest = Some(super::privacy_envelope::digest_bytes(b"policy"));
    receipt.recipient = Some(snapshot.binding.owner_key.fingerprint().unwrap());
    receipt.session_snapshot = Some(snapshot);
    receipt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_receipts_preserve_source_identity_and_require_explicit_mode() {
        let ordinary = PublicationReceipt {
            session_snapshot: None,
            version: 2,
            mode: PublicationMode::Ordinary,
            repository: "alice/demo".into(),
            branch: "work".into(),
            source: "a".repeat(40),
            published: "a".repeat(40),
            projected_session_id: None,
            destination: RemoteIdentity::new(
                "https://hub.example",
                "00000000-0000-0000-0000-000000000001",
            )
            .unwrap(),
            url: "https://hub.example/alice/demo.git".into(),
            policy_digest: None,
            recipient: None,
        };
        ordinary.validate().unwrap();
        let wire = serde_json::to_value(&ordinary).unwrap();
        assert_eq!(wire["mode"], "ordinary");
        assert!(wire.get("recipient").is_none() && wire.get("policy_digest").is_none());
        let mut untyped = wire;
        untyped.as_object_mut().unwrap().remove("mode");
        assert!(serde_json::from_value::<PublicationReceipt>(untyped).is_err());
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        ordinary.save(&repo).unwrap();
        assert_eq!(
            PublicationReceipt::load(&repo, "work").unwrap(),
            Some(ordinary.clone())
        );
        let mut mismatched = ordinary.clone();
        mismatched.published = "b".repeat(40);
        assert!(mismatched.validate().is_err());
        mismatched.mode = PublicationMode::SessionPlain;
        mismatched.version = 3;
        assert!(mismatched.validate().is_err());
        mismatched.projected_session_id = Some(format!("agit-{}", "c".repeat(40)));
        mismatched.validate().unwrap();
        mismatched.policy_digest =
            Some(super::super::privacy_envelope::digest_bytes(b"unexpected"));
        assert!(mismatched.validate().is_err());
        mismatched = ordinary.clone();
        mismatched.recipient = Some("invented-recipient".into());
        assert!(mismatched.validate().is_err());
        mismatched = ordinary;
        mismatched.version = 1;
        assert!(mismatched.validate().is_err());
    }
}
