//! Unattended publication needs device-local consent for the current policy and destination.

use crate::domain::privacy_receipt::PublicationMode;
use crate::domain::repo::Repo;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AutoConsent {
    pub version: u32,
    #[serde(default, skip_serializing_if = "PublicationMode::is_encrypted")]
    pub mode: PublicationMode,
    pub hub: String,
    pub account: String,
    pub account_id: Option<String>,
    pub agent_id: String,
    pub url: String,
    pub visibility: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
}

impl AutoConsent {
    pub fn matches(&self, repo: &Repo) -> Result<bool> {
        self.validate()?;
        let path = repo.common_dir()?.join("agit/privacy-auto-consent.json");
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let saved: Self =
            serde_json::from_slice(&bytes).context("invalid automatic publication consent")?;
        saved.validate()?;
        Ok(saved == *self)
    }

    pub fn save(&self, repo: &Repo) -> Result<()> {
        self.validate()?;
        let parent = repo.common_dir()?.join("agit");
        crate::infra::config::create_state_dir(&parent)?;
        let mut staging = tempfile::NamedTempFile::new_in(&parent)?;
        staging.write_all(&serde_json::to_vec(self)?)?;
        staging.as_file().sync_all()?;
        staging
            .persist(parent.join("privacy-auto-consent.json"))
            .map_err(|error| error.error)?;
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        self.mode.validate_bindings(
            self.version,
            self.policy_digest.as_deref(),
            self.recipient.as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_keeps_legacy_encrypted_bindings_and_separates_ordinary_destinations() {
        let legacy = serde_json::json!({"version":1,"hub":"https://hub.example","account":"alice",
            "account_id":"account-1","agent_id":"repository-1","url":"https://hub.example/alice/demo.git",
            "visibility":"private","policy_digest":"policy","recipient":"recipient"});
        let encrypted: AutoConsent = serde_json::from_value(legacy.clone()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        encrypted.save(&repo).unwrap();
        assert!(encrypted.matches(&repo).unwrap());
        assert_eq!(serde_json::to_value(&encrypted).unwrap(), legacy);
        let mut ordinary = AutoConsent {
            version: 2,
            mode: PublicationMode::Ordinary,
            policy_digest: None,
            recipient: None,
            ..encrypted
        };
        assert!(!ordinary.matches(&repo).unwrap());
        ordinary.save(&repo).unwrap();
        assert!(ordinary.matches(&repo).unwrap());
        ordinary.agent_id = "another-repository".into();
        assert!(!ordinary.matches(&repo).unwrap());
        ordinary.recipient = Some("invented-recipient".into());
        assert!(ordinary.save(&repo).is_err());
    }
}
