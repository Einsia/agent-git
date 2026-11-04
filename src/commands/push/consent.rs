//! Project enrollment binds an account and destination; snapshots retain their own privacy modes.

use crate::domain::repo::Repo;
use crate::hub::identity::RemoteIdentity;
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
        Ok(Self::saved(repo)?.is_some_and(|saved| saved == *self))
    }

    fn saved(repo: &Repo) -> Result<Option<Self>> {
        let path = repo.common_dir()?.join("agit/privacy-auto-consent.json");
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let saved: Self =
            serde_json::from_slice(&bytes).context("invalid automatic publication consent")?;
        saved.validate()?;
        Ok(Some(saved))
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

/// Automatic upload into a public repository needs an enrollment the person made for that exact
/// repository while it was public. A private enrollment, or one for another destination, does not
/// carry over: otherwise a repository made public later would keep uploading unasked.
pub(crate) fn confirms_public(repo: &Repo, destination: &RemoteIdentity) -> Result<bool> {
    Ok(AutoConsent::saved(repo)?.is_some_and(|saved| {
        saved.visibility == "public"
            && saved.hub == destination.hub
            && saved.agent_id == destination.agent_id
    }))
}

/// Only a person switches on automatic upload into a public repository: at a terminal, or with a
/// click in agit's own menu bar, which passes `--public-consent` naming the repository its dialog
/// showed. No `--yes`, `--json` or `AGIT_YES` answers it, and neither does the flag inside an agent
/// session: an agent that relays a yes would otherwise enable every later turn's upload on one
/// sentence.
pub(crate) fn require_person_for_public(
    repository: &str,
    command: &str,
    public_consent: Option<&str>,
) -> crate::Result<()> {
    let in_agent_session = crate::tui::Signals::from_process().agent_session.is_some();
    if person_consents(
        repository,
        public_consent,
        crate::ui::prompt::can_ask(),
        in_agent_session,
    ) {
        return Ok(());
    }
    Err(crate::commands::InteractionRequired(format!(
        "Automatic upload into the public repository {repository} is turned on from the AgentGit menu bar, or by running `{command}` in a terminal."
    ))
    .into())
}

/// Whether a person stands behind switching on automatic upload into the public `repository`:
/// a terminal outside an agent session, or the menu bar's consent for exactly that repository,
/// also outside one.
fn person_consents(
    repository: &str,
    public_consent: Option<&str>,
    at_terminal: bool,
    in_agent_session: bool,
) -> bool {
    at_terminal || (!in_agent_session && public_consent == Some(repository))
}

/// Records the person's enrollment when switching automatic upload on for a checkout pinned to a
/// public repository, and refuses the switch when no person stands behind it.
///
/// A checkout whose destination or audience cannot be read records nothing; automatic
/// publication then refuses a public destination by itself.
pub(crate) fn enroll_public_destination(
    repo: &Repo,
    repository: &str,
    command: &str,
    public_consent: Option<&str>,
) -> crate::Result<()> {
    let Some(identity) = crate::hub::identity::read(repo)? else {
        return Ok(());
    };
    let Ok(consent) = super::publication_consent(repo, repository, &identity) else {
        return Ok(());
    };
    if consent.visibility != "public" {
        return Ok(());
    }
    require_person_for_public(repository, command, public_consent)?;
    consent.save(repo)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The menu bar's consent counts only for the repository it names and only outside an agent
    /// session; `--yes` alone is no consent. A gate that took any repository named, or the flag
    /// from an agent's own session, would let an agent switch a public upload on by passing it.
    #[test]
    fn a_public_switch_on_needs_a_terminal_or_the_menu_bars_consent_for_that_repository() {
        assert!(person_consents("alice/app", None, true, false));
        assert!(person_consents(
            "alice/app",
            Some("alice/app"),
            false,
            false
        ));
        assert!(
            !person_consents("alice/app", None, false, false),
            "--yes alone"
        );
        assert!(!person_consents(
            "alice/app",
            Some("alice/other"),
            false,
            false
        ));
        assert!(!person_consents(
            "alice/app",
            Some("alice/app"),
            false,
            true
        ));
    }

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
