//! Shared identities and immutable snapshot bindings for session-owner encryption.
//!
//! Missing metadata is unregistered history, not permission to publish plaintext. Repository
//! slugs, branches and the account transporting a commit do not establish session ownership.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

pub const SESSION_ENVELOPE_VERSION: u32 = 2;
pub const MAX_REVISION: u64 = (1 << 53) - 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PublishedSnapshot {
    FileLine {
        commit: String,
    },
    Plain {
        commit: String,
        session_id: String,
        config_revision: u64,
    },
    SessionOwnerV2 {
        snapshot: AcceptedSnapshot,
        config_revision: u64,
    },
}

impl PublishedSnapshot {
    pub fn commit(&self) -> &str {
        match self {
            Self::FileLine { commit } | Self::Plain { commit, .. } => commit,
            Self::SessionOwnerV2 { snapshot, .. } => &snapshot.published_commit,
        }
    }
}

/// Preserved objects must already be accepted; they do not acquire a new mode or entitlement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPublicationManifest {
    pub version: u32,
    pub repository_id: String,
    pub snapshots: Vec<PublishedSnapshot>,
    pub preserved_commits: Vec<String>,
}

impl SessionPublicationManifest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == SESSION_ENVELOPE_VERSION,
            "unsupported session publication manifest"
        );
        validate_repository_id(&self.repository_id)?;
        let mut commits = std::collections::BTreeSet::<&str>::new();
        let mut sessions = std::collections::BTreeMap::new();
        let mut sources = std::collections::BTreeSet::new();
        let mut owners = std::collections::BTreeMap::new();
        for commit in &self.preserved_commits {
            validate_oid(commit)?;
            ensure!(
                commits.insert(commit),
                "duplicate preserved publication commit"
            );
        }
        for entry in &self.snapshots {
            validate_oid(entry.commit())?;
            ensure!(
                commits.insert(entry.commit()),
                "ambiguous publication snapshot"
            );
            let (session, scheme, revision) = match entry {
                PublishedSnapshot::FileLine { .. } => continue,
                PublishedSnapshot::Plain {
                    session_id,
                    config_revision,
                    ..
                } => (session_id, SessionScheme::Plain, *config_revision),
                PublishedSnapshot::SessionOwnerV2 {
                    snapshot,
                    config_revision,
                } => {
                    snapshot.validate()?;
                    ensure!(
                        snapshot.binding.repository_id == self.repository_id,
                        "publication snapshot belongs to another repository"
                    );
                    ensure!(
                        sources.insert(&snapshot.binding.source_commit),
                        "publication assigns conflicting identities to one source snapshot"
                    );
                    let authority = (
                        &snapshot.binding.owner_key.account_id,
                        &snapshot.binding.published_session_id,
                    );
                    if let Some(previous) = owners.insert(&snapshot.binding.session_id, authority) {
                        ensure!(
                            previous == authority,
                            "publication changes a session's owner or public identity"
                        );
                    }
                    (
                        &snapshot.binding.session_id,
                        SessionScheme::SessionOwnerV2,
                        *config_revision,
                    )
                }
            };
            ensure!(
                super::meta::is_bare_id(session),
                "publication has no logical session identity"
            );
            validate_revision(revision)?;
            if let Some(previous) = sessions.insert(session, (scheme, revision)) {
                ensure!(
                    previous == (scheme, revision),
                    "publication mixes configurations for one session"
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionScheme {
    Plain,
    SessionOwnerV2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkContext {
    Original,
    Sanitized,
}

/// Provenance identifies copied context; it does not grant access to source originals.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkOrigin {
    pub commit: String,
    pub context: ForkContext,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<AcceptedSnapshot>,
}

impl ForkOrigin {
    pub fn validate(&self) -> Result<()> {
        validate_oid(&self.commit)?;
        if let Some(snapshot) = &self.snapshot {
            snapshot.validate()?;
            ensure!(
                snapshot.published_commit == self.commit,
                "fork provenance differs from its source snapshot"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountIdentity {
    pub hub: String,
    pub account_id: String,
}

impl AccountIdentity {
    pub fn new(hub: &str, account_id: &str) -> Result<Self> {
        let identity = Self {
            hub: canonical_hub(hub)?,
            account_id: account_id.into(),
        };
        identity.validate()?;
        Ok(identity)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            canonical_hub(&self.hub)? == self.hub,
            "noncanonical account Hub"
        );
        validate_identifier(&self.account_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "id",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SessionOwner {
    User(String),
    Organization(String),
}

impl SessionOwner {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::User(id) | Self::Organization(id) => validate_identifier(id),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerResolution {
    Creator,
    FirstCommitter,
    RepositoryOwner,
    Unresolved,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPrivacy {
    pub scheme: SessionScheme,
    pub hub: Option<String>,
    pub creator: Option<String>,
    pub owner: Option<SessionOwner>,
    pub owner_source: OwnerResolution,
}

impl SessionPrivacy {
    /// Offline birth retains the encrypted default without guessing an account from a namespace.
    pub fn new(creator: Option<AccountIdentity>) -> Result<Self> {
        if let Some(identity) = &creator {
            identity.validate()?;
        }
        Ok(Self {
            scheme: SessionScheme::SessionOwnerV2,
            hub: creator.as_ref().map(|identity| identity.hub.clone()),
            creator: creator.as_ref().map(|identity| identity.account_id.clone()),
            owner: creator
                .as_ref()
                .map(|identity| SessionOwner::User(identity.account_id.clone())),
            owner_source: if creator.is_some() {
                OwnerResolution::Creator
            } else {
                OwnerResolution::Unresolved
            },
        })
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(hub) = &self.hub {
            ensure!(canonical_hub(hub)? == *hub, "noncanonical session Hub");
        }
        if let Some(creator) = &self.creator {
            validate_identifier(creator)?;
            ensure!(
                self.hub.is_some(),
                "session creator requires a Hub identity"
            );
        }
        if let Some(owner) = &self.owner {
            owner.validate()?;
            ensure!(self.hub.is_some(), "session owner requires a Hub identity");
        }
        match self.owner_source {
            OwnerResolution::Creator => ensure!(
                self.creator
                    .as_ref()
                    .is_some_and(|creator| self.owner == Some(SessionOwner::User(creator.clone()))),
                "session creator must be its original owner"
            ),
            OwnerResolution::FirstCommitter => ensure!(
                matches!(self.owner, Some(SessionOwner::User(_))),
                "first committer must resolve to an account"
            ),
            OwnerResolution::RepositoryOwner => ensure!(
                self.owner.is_some(),
                "repository fallback requires an owner principal"
            ),
            OwnerResolution::Unresolved => ensure!(
                self.owner.is_none() && self.creator.is_none(),
                "unresolved session cannot carry a guessed owner or creator"
            ),
        }
        ensure!(
            self.scheme == SessionScheme::Plain
                || !matches!(self.owner, Some(SessionOwner::Organization(_))),
            "organization fallback ownership does not support session encryption"
        );
        Ok(())
    }
}

/// Registration freezes ownership once, independently of renamable presentation coordinates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPrivacyConfig {
    pub repository_id: String,
    pub session_id: String,
    pub privacy: SessionPrivacy,
    pub revision: u64,
    pub created_at: String,
}

impl SessionPrivacyConfig {
    pub fn validate(&self) -> Result<()> {
        validate_repository_id(&self.repository_id)?;
        ensure!(
            super::meta::is_bare_id(&self.session_id),
            "invalid logical session identity"
        );
        self.privacy.validate()?;
        validate_revision(self.revision)?;
        chrono::DateTime::parse_from_rfc3339(&self.created_at)
            .context("invalid session creation time")?;
        if self.privacy.scheme == SessionScheme::SessionOwnerV2 {
            ensure!(
                matches!(self.privacy.owner, Some(SessionOwner::User(_))),
                "encrypted registration requires an account owner"
            );
        }
        Ok(())
    }

    /// A retry or another signed-in account cannot replace an already recorded creator.
    pub fn verify_registration(
        &self,
        repository: &str,
        session: &str,
        local: &SessionPrivacy,
    ) -> Result<()> {
        self.validate()?;
        local.validate()?;
        ensure!(
            self.repository_id == repository && self.session_id == session,
            "session registration belongs to another repository or session"
        );
        ensure!(
            self.privacy.scheme == local.scheme,
            "session encryption mode differs from registered history"
        );
        if local.hub.is_some() {
            ensure!(
                self.privacy.hub == local.hub,
                "session registration belongs to another Hub"
            );
        }
        if local.owner_source != OwnerResolution::Unresolved {
            ensure!(
                self.privacy == *local,
                "session registration cannot replace recorded ownership"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerKeyBinding {
    pub account_id: String,
    pub key_version: u64,
}

impl OwnerKeyBinding {
    pub fn validate(&self) -> Result<()> {
        validate_identifier(&self.account_id)?;
        validate_revision(self.key_version)
    }

    pub fn recipient_id(&self) -> Result<String> {
        use sha2::{Digest, Sha256};
        self.validate()?;
        let digest = Sha256::digest(serde_json::to_vec(self)?);
        Ok(format!("session-owner-{}", hex::encode(digest)))
    }

    pub fn fingerprint(&self) -> Result<String> {
        use sha2::{Digest, Sha256};
        self.validate()?;
        Ok(format!(
            "sha256:{}",
            hex::encode(Sha256::digest(serde_json::to_vec(self)?))
        ))
    }
}

/// Unmapped attribution binds private source evidence without disclosing names or emails.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommitterAttribution {
    Account { account_id: String },
    Unmapped { source_identity_digest: String },
}

impl CommitterAttribution {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Account { account_id } => validate_identifier(account_id),
            Self::Unmapped {
                source_identity_digest,
            } => validate_digest(source_identity_digest),
        }
    }
}

/// Original Git committer evidence authenticates the account or digest in the public binding.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceCommitter {
    pub git_identity: Vec<u8>,
    pub account: Option<AccountIdentity>,
}

impl std::fmt::Debug for SourceCommitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SourceCommitter(<redacted>)")
    }
}

impl SourceCommitter {
    pub fn from_commit(bytes: &[u8], account: Option<AccountIdentity>) -> Result<Self> {
        ensure!(
            bytes.len() <= 1024 * 1024,
            "source commit exceeds the attribution budget"
        );
        let boundary = bytes
            .windows(2)
            .position(|bytes| bytes == b"\n\n")
            .context("source commit has no header boundary")?;
        let mut identities = bytes[..boundary]
            .split(|byte| *byte == b'\n')
            .filter_map(|line| line.strip_prefix(b"committer "));
        let git_identity = identities
            .next()
            .context("source commit has no committer")?
            .to_vec();
        ensure!(
            identities.next().is_none(),
            "source commit has ambiguous committers"
        );
        let source = Self {
            git_identity,
            account,
        };
        source.validate()?;
        Ok(source)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.git_identity.is_empty()
                && self.git_identity.len() <= 16 * 1024
                && !self
                    .git_identity
                    .iter()
                    .any(|byte| matches!(byte, b'\n' | b'\r' | 0)),
            "invalid original Git committer identity"
        );
        if let Some(account) = &self.account {
            account.validate()?;
        }
        Ok(())
    }

    pub fn attribution(&self, hub: &str) -> Result<CommitterAttribution> {
        self.validate()?;
        let hub = canonical_hub(hub)?;
        if let Some(account) = &self.account
            && account.hub == hub
        {
            return Ok(CommitterAttribution::Account {
                account_id: account.account_id.clone(),
            });
        }
        use sha2::{Digest, Sha256};
        Ok(CommitterAttribution::Unmapped {
            source_identity_digest: format!(
                "sha256:{}",
                hex::encode(Sha256::digest(serde_json::to_vec(self)?))
            ),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSnapshotBinding {
    pub repository_id: String,
    pub session_id: String,
    pub published_session_id: String,
    pub owner_key: OwnerKeyBinding,
    pub committer: CommitterAttribution,
    pub source_commit: String,
}

impl SessionSnapshotBinding {
    pub fn validate(&self) -> Result<()> {
        validate_repository_id(&self.repository_id)?;
        ensure!(
            super::meta::is_bare_id(&self.session_id)
                && super::meta::is_bare_id(&self.published_session_id),
            "invalid source-to-publication session mapping"
        );
        self.owner_key.validate()?;
        self.committer.validate()?;
        validate_oid(&self.source_commit)
    }

    pub fn verify_config(&self, config: &SessionPrivacyConfig) -> Result<()> {
        self.validate()?;
        config.validate()?;
        ensure!(
            config.privacy.scheme == SessionScheme::SessionOwnerV2
                && config.repository_id == self.repository_id
                && config.session_id == self.session_id
                && config.privacy.owner
                    == Some(SessionOwner::User(self.owner_key.account_id.clone())),
            "snapshot differs from registered session ownership"
        );
        Ok(())
    }
}

/// The published commit and exact envelope bytes are bound after creating the Git object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedSnapshot {
    pub version: u32,
    pub binding: SessionSnapshotBinding,
    pub published_commit: String,
    pub envelope_digest: String,
}

impl AcceptedSnapshot {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == SESSION_ENVELOPE_VERSION,
            "unsupported accepted snapshot version"
        );
        self.binding.validate()?;
        validate_oid(&self.published_commit)?;
        validate_digest(&self.envelope_digest)
    }
}

pub fn canonical_hub(hub: &str) -> Result<String> {
    crate::infra::hub_authority::HubAuthority::parse(hub)?;
    let (scheme, rest) = hub
        .trim()
        .trim_end_matches('/')
        .split_once("://")
        .context("invalid session Hub")?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    ensure!(
        !path.split('/').any(|part| matches!(part, "." | "..")),
        "invalid session Hub path"
    );
    let prefix = format!(
        "{}://{}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase()
    );
    Ok(if path.is_empty() {
        prefix
    } else {
        format!("{prefix}/{path}")
    })
}

pub fn validate_identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value != "."
            && value != ".."
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
        "invalid privacy identity"
    );
    Ok(())
}

pub fn validate_repository_id(value: &str) -> Result<()> {
    ensure!(
        uuid::Uuid::parse_str(value).is_ok_and(|id| id.to_string() == value),
        "invalid immutable repository identity"
    );
    Ok(())
}

pub fn validate_revision(value: u64) -> Result<()> {
    ensure!(
        value > 0 && value <= MAX_REVISION,
        "invalid privacy revision"
    );
    Ok(())
}

pub fn validate_oid(value: &str) -> Result<()> {
    ensure!(
        matches!(value.len(), 40 | 64)
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "invalid immutable snapshot commit"
    );
    Ok(())
}

fn validate_digest(value: &str) -> Result<()> {
    let hex = value
        .strip_prefix("sha256:")
        .context("privacy digest must be sha256-prefixed")?;
    ensure!(hex.len() == 64, "invalid privacy digest length");
    validate_oid(hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Account IDs are Hub-scoped; unmapped and non-Unicode Git attribution remains exact.
    #[test]
    fn committer_evidence_never_substitutes_a_transport_account() {
        let account = AccountIdentity::new("https://hub.test", "writer").unwrap();
        let raw = b"tree 0000000000000000000000000000000000000000\ncommitter Original \xff <original@example.invalid> 1 +0000\n\ncommitter Message text\n";
        let mut source = SourceCommitter::from_commit(raw, Some(account)).unwrap();
        assert_eq!(
            source.git_identity,
            b"Original \xff <original@example.invalid> 1 +0000"
        );
        assert_eq!(
            source.attribution("https://hub.test").unwrap(),
            CommitterAttribution::Account {
                account_id: "writer".into()
            }
        );
        assert!(matches!(
            source.attribution("https://other.test").unwrap(),
            CommitterAttribution::Unmapped { .. }
        ));
        source.account = None;
        let unmapped = source.attribution("https://hub.test").unwrap();
        source.git_identity[0] = b'X';
        assert_ne!(source.attribution("https://hub.test").unwrap(), unmapped);
        assert!(!format!("{source:?}").contains("original@example.invalid"));
        for invalid in [
            b"tree a\n\ncommitter Not a header".as_slice(),
            b"committer First\ncommitter Second\n\nmessage",
        ] {
            assert!(SourceCommitter::from_commit(invalid, None).is_err());
        }
    }

    /// Ownership survives account switches; unresolved legacy principals remain representable.
    #[test]
    fn registration_preserves_birth_identity_and_legacy_fallback() {
        let privacy = SessionPrivacy::new(Some(
            AccountIdentity::new("https://hub.test", "alice").unwrap(),
        ))
        .unwrap();
        let mut config = SessionPrivacyConfig {
            repository_id: "00000000-0000-0000-0000-000000000001".into(),
            session_id: format!("agit-{}", "a".repeat(40)),
            privacy: privacy.clone(),
            revision: 1,
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        config
            .verify_registration(&config.repository_id, &config.session_id, &privacy)
            .unwrap();
        config.privacy = SessionPrivacy::new(Some(
            AccountIdentity::new("https://hub.test", "bob").unwrap(),
        ))
        .unwrap();
        assert!(
            config
                .verify_registration(&config.repository_id, &config.session_id, &privacy)
                .is_err()
        );
        config.privacy = SessionPrivacy {
            scheme: SessionScheme::Plain,
            hub: Some("https://hub.test".into()),
            creator: None,
            owner: Some(SessionOwner::Organization("org".into())),
            owner_source: OwnerResolution::RepositoryOwner,
        };
        config.validate().unwrap();
        config.privacy.scheme = SessionScheme::SessionOwnerV2;
        assert!(config.validate().is_err());
        config.privacy = SessionPrivacy::new(None).unwrap();
        assert!(config.validate().is_err());
        config.privacy.scheme = SessionScheme::Plain;
        config.validate().unwrap();
    }

    /// Legacy parsing cannot turn absent mode data into permission for plaintext publication.
    #[test]
    fn metadata_distinguishes_unregistered_history_from_explicit_plain() {
        let mut value = serde_json::json!({"session": format!("agit-{}", "a".repeat(40)), "runtime": "codex", "cwd": "/workspace"});
        let legacy: super::super::meta::Meta = serde_json::from_value(value.clone()).unwrap();
        assert!(legacy.session_privacy.is_none());
        assert!(
            serde_json::to_value(legacy)
                .unwrap()
                .get("session_privacy")
                .is_none()
        );
        let mut privacy = SessionPrivacy::new(None).unwrap();
        assert_eq!(privacy.scheme, SessionScheme::SessionOwnerV2);
        privacy.scheme = SessionScheme::Plain;
        value["session_privacy"] = serde_json::to_value(privacy).unwrap();
        let plain: super::super::meta::Meta = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(plain.session_privacy.unwrap().scheme, SessionScheme::Plain);
        value["session_privacy"]["scheme"] = "repository_password".into();
        assert!(serde_json::from_value::<super::super::meta::Meta>(value).is_err());
    }
}
