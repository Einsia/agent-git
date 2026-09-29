//! Local source-to-publication acknowledgements never become part of the outgoing Git tree.

use super::{privacy::PrivacyPolicy, repo::Repo, storage};
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

pub mod outbox;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationMode {
    Ordinary,
    #[default]
    Encrypted,
}

impl PublicationMode {
    pub fn is_encrypted(&self) -> bool {
        *self == Self::Encrypted
    }

    /// Legacy records are encrypted; ordinary records must explicitly identify their format.
    pub fn validate_bindings(
        &self,
        version: u32,
        policy: Option<&str>,
        recipient: Option<&str>,
    ) -> Result<()> {
        match self {
            Self::Encrypted => ensure!(
                version == 1
                    && policy.is_some_and(|value| !value.is_empty())
                    && recipient.is_some_and(|value| !value.is_empty()),
                "incomplete or unsupported encrypted publication binding"
            ),
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
    #[serde(default, skip_serializing_if = "PublicationMode::is_encrypted")]
    pub mode: PublicationMode,
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

impl PublicationReceipt {
    pub(crate) fn session_id(&self, repo: &Repo) -> Result<String> {
        self.validate()?;
        if self.mode.is_encrypted() {
            return super::privacy_git::receipt_session_id(repo, self);
        }
        let metadata = storage::metadata_local(repo.root(), &self.published)?;
        ensure!(
            metadata.is_session_line() && super::meta::is_bare_id(&metadata.session),
            "ordinary publication receipt does not identify a settled session"
        );
        Ok(metadata.session)
    }

    pub(crate) fn ancestors(&self, repo: &Repo) -> Result<std::collections::BTreeSet<String>> {
        self.validate()?;
        if self.mode.is_encrypted() {
            super::privacy_git::receipt_ancestors(repo, self)
        } else {
            super::repo::publication::raw_ancestors(repo, &self.published)
        }
    }

    pub fn validate(&self) -> Result<()> {
        self.mode.validate_bindings(
            self.version,
            self.policy_digest.as_deref(),
            self.recipient.as_deref(),
        )?;
        ensure!(
            self.mode.is_encrypted() || self.source == self.published,
            "ordinary publication must retain the source commit identity"
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
        self.save_at(&receipt_path(repo, &self.branch)?)
    }

    pub(crate) fn save_for_destination(&self, repo: &Repo) -> Result<()> {
        self.save_at(&destination_receipt_path(
            repo,
            &self.branch,
            &self.destination,
        )?)
    }

    fn save_at(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let parent = path
            .parent()
            .context("publication receipt has no directory")?;
        crate::infra::config::create_state_dir(parent)?;
        let mut staging = tempfile::NamedTempFile::new_in(parent)?;
        staging.write_all(&serde_json::to_vec(self)?)?;
        staging.as_file().sync_all()?;
        staging.persist(path).map_err(|error| error.error)?;
        Ok(())
    }

    pub fn load(repo: &Repo, branch: &str) -> Result<Option<Self>> {
        Self::load_at(&receipt_path(repo, branch)?, branch)
    }

    pub(crate) fn load_for_destination(
        repo: &Repo,
        branch: &str,
        destination: &RemoteIdentity,
    ) -> Result<Option<Self>> {
        let saved = Self::load_at(
            &destination_receipt_path(repo, branch, destination)?,
            branch,
        )?;
        ensure!(
            saved
                .as_ref()
                .is_none_or(|receipt| receipt.destination == *destination),
            "publication receipt belongs to another destination"
        );
        Ok(saved)
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
            && repo.remote_url().as_deref() == Some(self.url.as_str())
            && (!self.mode.is_encrypted()
                || Some(PrivacyPolicy::load(repo)?.digest()?) == self.policy_digest))
    }
}

fn receipt_path(repo: &Repo, branch: &str) -> Result<PathBuf> {
    use sha2::Digest;
    let key = hex::encode(sha2::Sha256::digest(branch.as_bytes()));
    Ok(repo
        .common_dir()?
        .join("agit/privacy-published")
        .join(format!("{key}.json")))
}

fn destination_receipt_path(
    repo: &Repo,
    branch: &str,
    destination: &RemoteIdentity,
) -> Result<PathBuf> {
    use sha2::Digest;
    let key = hex::encode(sha2::Sha256::digest(serde_json::to_vec(destination)?));
    let branch = hex::encode(sha2::Sha256::digest(branch.as_bytes()));
    Ok(repo
        .common_dir()?
        .join("agit/publication-destinations")
        .join(key)
        .join(format!("{branch}.json")))
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
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ordinary_receipts_preserve_source_ids_and_legacy_encrypted_records_roundtrip() {
        let legacy = json!({
            "version":1, "repository":"alice/demo", "branch":"work",
            "source":"a".repeat(40), "published":"b".repeat(40),
            "destination":{"hub":"https://hub.example","agent_id":"00000000-0000-0000-0000-000000000001"},
            "url":"https://hub.example/alice/demo.git", "policy_digest":"policy", "recipient":"recipient"
        });
        let encrypted: PublicationReceipt = serde_json::from_value(legacy.clone()).unwrap();
        encrypted.validate().unwrap();
        assert_eq!(encrypted.mode, PublicationMode::Encrypted);
        assert_eq!(serde_json::to_value(&encrypted).unwrap(), legacy);

        let ordinary = PublicationReceipt {
            version: 2,
            mode: PublicationMode::Ordinary,
            published: encrypted.source.clone(),
            policy_digest: None,
            recipient: None,
            ..encrypted.clone()
        };
        ordinary.validate().unwrap();
        let wire = serde_json::to_value(&ordinary).unwrap();
        assert_eq!(wire["mode"], "ordinary");
        assert!(wire.get("recipient").is_none() && wire.get("policy_digest").is_none());
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        ordinary.save(&repo).unwrap();
        assert_eq!(
            PublicationReceipt::load(&repo, "work").unwrap(),
            Some(ordinary.clone())
        );
        let mut mismatched = ordinary.clone();
        mismatched.published = encrypted.published;
        assert!(mismatched.validate().is_err());
        mismatched = ordinary.clone();
        mismatched.recipient = Some("invented-recipient".into());
        assert!(mismatched.validate().is_err());
        mismatched = ordinary;
        mismatched.version = 1;
        assert!(mismatched.validate().is_err());
    }
}
