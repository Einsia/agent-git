//! Repository key roles have distinct responses and retain the caller's immutable target.

use crate::domain::privacy_envelope::{ViewingRecipient, valid_token};
use crate::domain::privacy_key::{KeyInput, KeyRecord, recipient_id, validate_public_key};
use crate::hub::{Client, client::ApiError, identity::RemoteIdentity};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const MAX_CONFIG_VERSION: u64 = (1 << 53) - 1;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PublishingKey {
    pub recipient: String,
    pub public_key_algorithm: String,
    pub public_key: String,
}

impl PublishingKey {
    pub fn viewing_recipient(&self) -> Result<ViewingRecipient> {
        validate_public_key(
            &self.public_key_algorithm,
            &self.public_key,
            &self.recipient,
        )?;
        ViewingRecipient::from_base64(self.recipient.clone(), &self.public_key)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishingKeyResponse {
    pub agent_id: String,
    pub config_version: u64,
    pub current: Option<PublishingKey>,
}

impl PublishingKeyResponse {
    pub fn require_current(self, repository: &str) -> Result<PublishingKey> {
        self.current.with_context(|| format!(
            "repository viewing key is not configured; an administrator must run `agit privacy init {repository}` before publication"
        ))
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryKeyConfig {
    pub agent_id: String,
    pub config_version: u64,
    pub current_recipient: Option<String>,
    pub keys: Vec<KeyRecord>,
}

impl RepositoryKeyConfig {
    pub fn current(&self) -> Option<&KeyRecord> {
        self.current_recipient
            .as_ref()
            .and_then(|recipient| self.keys.iter().find(|key| &key.recipient == recipient))
    }

    fn validate(&self) -> Result<()> {
        validate_version(self.config_version)?;
        let mut recipients = BTreeSet::new();
        for record in &self.keys {
            record.validate()?;
            ensure!(
                recipients.insert(&record.recipient),
                "repository configuration contains duplicate recipients"
            );
            ensure!(
                record.current == (self.current_recipient.as_ref() == Some(&record.recipient)),
                "repository configuration current key is inconsistent"
            );
        }
        ensure!(
            self.current_recipient.is_none() || self.current().is_some(),
            "repository configuration is missing its current key"
        );
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationUnlockKey {
    pub agent_id: String,
    pub commit: String,
    pub session_id: String,
    pub key: KeyRecord,
}

#[derive(Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum KeyOperation<'a> {
    Initialize {
        key: &'a KeyInput,
    },
    Rewrap {
        recipient: &'a str,
        key: &'a KeyInput,
    },
    Rotate {
        key: &'a KeyInput,
    },
    Revoke {
        recipient: &'a str,
    },
}

#[derive(Serialize)]
struct Mutation<'a> {
    expected_agent_id: &'a str,
    expected_version: u64,
    #[serde(flatten)]
    operation: &'a KeyOperation<'a>,
}

impl Client {
    pub fn repository_publishing_key(
        &self,
        repository: &str,
        identity: &RemoteIdentity,
    ) -> Result<PublishingKeyResponse> {
        let path = self.key_path(repository, identity, "publishing-key")?;
        let response: PublishingKeyResponse = self.get_expected(&path, Some(&identity.agent_id))?;
        self.check_key_identity(identity, &response.agent_id)?;
        validate_version(response.config_version)?;
        if let Some(key) = &response.current {
            key.viewing_recipient()?;
        }
        Ok(response)
    }

    pub fn repository_key_config(
        &self,
        repository: &str,
        identity: &RemoteIdentity,
    ) -> Result<RepositoryKeyConfig> {
        let path = self.key_path(repository, identity, "keys")?;
        let response: RepositoryKeyConfig = self.get_expected(&path, Some(&identity.agent_id))?;
        self.check_key_identity(identity, &response.agent_id)?;
        response.validate()?;
        Ok(response)
    }

    pub fn publication_unlock_key(
        &self,
        repository: &str,
        identity: &RemoteIdentity,
        commit: &str,
        recipient: &str,
    ) -> Result<PublicationUnlockKey> {
        ensure!(
            (commit.len() == 40 || commit.len() == 64)
                && commit
                    .bytes()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "repository key lookup requires a full immutable commit"
        );
        ensure!(
            valid_token(recipient) && recipient != "." && recipient != "..",
            "invalid repository viewing recipient"
        );
        let path = self.key_path(
            repository,
            identity,
            &format!("keys/{recipient}?ref={commit}"),
        )?;
        let response: PublicationUnlockKey = self.get_expected(&path, Some(&identity.agent_id)).context("cannot read the publication viewing key; verify repository read access and that the accepted publication's key has not been revoked")?;
        self.check_key_identity(identity, &response.agent_id)?;
        ensure!(
            response.commit == commit && response.key.recipient == recipient,
            "repository viewing-key response does not match the selected publication"
        );
        ensure!(
            valid_token(&response.session_id),
            "repository viewing-key response has an invalid session identity"
        );
        response.key.validate()?;
        Ok(response)
    }

    pub fn mutate_repository_key(
        &self,
        repository: &str,
        identity: &RemoteIdentity,
        previous: &RepositoryKeyConfig,
        operation: KeyOperation<'_>,
    ) -> Result<RepositoryKeyConfig> {
        let path = self.key_path(repository, identity, "keys")?;
        self.check_key_identity(identity, &previous.agent_id)?;
        previous.validate()?;
        ensure!(
            previous.config_version < MAX_CONFIG_VERSION,
            "repository key configuration version is exhausted"
        );
        validate_operation(previous, &operation)?;
        let request = Mutation {
            expected_agent_id: &identity.agent_id,
            expected_version: previous.config_version,
            operation: &operation,
        };
        let response: RepositoryKeyConfig = self.post(&path, &request).map_err(|error| {
            if error.downcast_ref::<ApiError>().is_some_and(|e| e.status == 409) {
                error.context("repository key configuration changed or a publication is pending; refresh and reconfirm the operation")
            } else { error }
        })?;
        self.check_key_identity(identity, &response.agent_id)?;
        response.validate()?;
        ensure!(
            response.config_version == previous.config_version + 1,
            "repository key mutation returned an unexpected configuration version"
        );
        verify_mutation(previous, &operation, &response)?;
        Ok(response)
    }

    fn key_path(
        &self,
        repository: &str,
        identity: &RemoteIdentity,
        suffix: &str,
    ) -> Result<String> {
        self.check_key_identity(identity, &identity.agent_id)?;
        Ok(format!("{}/{suffix}", super::repository_path(repository)?))
    }

    fn check_key_identity(&self, expected: &RemoteIdentity, actual_id: &str) -> Result<()> {
        let observed = RemoteIdentity::new(self.base(), actual_id)?;
        ensure!(
            &observed == expected,
            "repository viewing-key response has a different Hub or immutable repository identity"
        );
        Ok(())
    }
}

fn validate_version(version: u64) -> Result<()> {
    ensure!(
        version <= MAX_CONFIG_VERSION,
        "invalid repository key configuration version"
    );
    Ok(())
}

fn validate_operation(previous: &RepositoryKeyConfig, operation: &KeyOperation<'_>) -> Result<()> {
    match operation {
        KeyOperation::Initialize { key } | KeyOperation::Rotate { key } => {
            key.validate()?;
            let initialize = matches!(operation, KeyOperation::Initialize { .. });
            ensure!(
                previous.current_recipient.is_none() == initialize,
                "repository key operation does not match the current configuration; refresh first"
            );
            let recipient = recipient_id(&key.public_key);
            ensure!(
                !previous.keys.iter().any(|key| key.recipient == recipient),
                "initialization and rotation require a fresh viewing key"
            );
        }
        KeyOperation::Rewrap { recipient, key } => {
            key.validate()?;
            let current = previous
                .current()
                .context("repository viewing key is not configured")?;
            ensure!(
                current.recipient == *recipient && current.key.public_key == key.public_key,
                "password change must retain the current viewing key"
            );
        }
        KeyOperation::Revoke { recipient } => ensure!(
            previous.keys.iter().any(|key| key.recipient == *recipient),
            "repository viewing key is unavailable"
        ),
    }
    Ok(())
}

fn verify_mutation(
    previous: &RepositoryKeyConfig,
    operation: &KeyOperation<'_>,
    response: &RepositoryKeyConfig,
) -> Result<()> {
    let (expected_current, removed, uploaded) = match operation {
        KeyOperation::Initialize { key }
        | KeyOperation::Rotate { key }
        | KeyOperation::Rewrap { key, .. } => {
            (Some(recipient_id(&key.public_key)), None, Some(*key))
        }
        KeyOperation::Revoke { recipient } => (
            previous
                .current_recipient
                .clone()
                .filter(|current| current != recipient),
            Some(*recipient),
            None,
        ),
    };
    ensure!(
        response.current_recipient == expected_current,
        "repository key mutation returned an unexpected recipient"
    );
    let mut expected: BTreeSet<_> = previous
        .keys
        .iter()
        .map(|key| key.recipient.as_str())
        .filter(|recipient| Some(*recipient) != removed)
        .collect();
    if let Some(key) = uploaded {
        let current = response
            .current()
            .context("repository key mutation omitted the new key")?;
        ensure!(
            current.key == *key,
            "repository key mutation did not retain the submitted wrapping"
        );
        expected.insert(&current.recipient);
    }
    ensure!(
        expected
            == response
                .keys
                .iter()
                .map(|key| key.recipient.as_str())
                .collect(),
        "repository key mutation changed unrelated key history"
    );
    for old in &previous.keys {
        if Some(old.recipient.as_str()) == removed
            || uploaded.is_some_and(|key| key.public_key == old.key.public_key)
        {
            continue;
        }
        ensure!(
            response
                .keys
                .iter()
                .any(|key| key.recipient == old.recipient && key.key == old.key),
            "repository key mutation changed unrelated key material"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
