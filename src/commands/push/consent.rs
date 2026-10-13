//! Project enrollment binds an account and destination; snapshots retain their own privacy modes.

use crate::domain::repo::Repo;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AutoConsent {
    pub version: u32,
    pub hub: String,
    pub account: String,
    pub account_id: String,
    pub agent_id: String,
    pub url: String,
    pub visibility: String,
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
        ensure!(self.version == 3, "unsupported publication enrollment");
        let identity = crate::hub::identity::RemoteIdentity::new(&self.hub, &self.agent_id)?;
        ensure!(
            identity.hub == self.hub && identity.agent_id == self.agent_id,
            "noncanonical publication enrollment"
        );
        crate::domain::session_privacy::AccountIdentity::new(&self.hub, &self.account_id)?;
        ensure!(
            !self.account.is_empty() && matches!(self.visibility.as_str(), "public" | "private"),
            "invalid publication enrollment"
        );
        let (hub, _) = super::super::privacy::sources::remote_scope(&self.url)?;
        ensure!(
            hub == self.hub,
            "publication enrollment belongs to another Hub"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A saved enrollment cannot authorize another account, repository or audience.
    #[test]
    fn enrollment_retains_the_exact_account_and_destination() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(dir.path()).unwrap();
        let mut consent = AutoConsent {
            version: 3,
            hub: "https://hub.example".into(),
            account: "alice".into(),
            account_id: "account-1".into(),
            agent_id: "00000000-0000-0000-0000-000000000001".into(),
            url: "https://hub.example/alice/demo.git".into(),
            visibility: "private".into(),
        };
        assert!(!consent.matches(&repo).unwrap());
        consent.save(&repo).unwrap();
        assert!(consent.matches(&repo).unwrap());
        consent.account_id = "account-2".into();
        assert!(!consent.matches(&repo).unwrap());
        consent.save(&repo).unwrap();
        consent.agent_id = "00000000-0000-0000-0000-000000000002".into();
        assert!(!consent.matches(&repo).unwrap());
        consent.save(&repo).unwrap();
        consent.visibility = "public".into();
        assert!(!consent.matches(&repo).unwrap());
        consent.hub = "https://other.example".into();
        assert!(consent.save(&repo).is_err());
    }
}
