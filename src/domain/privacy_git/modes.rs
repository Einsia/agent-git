//! Explicit local publication choices are scoped to a logical session and destination.

use super::*;
use crate::domain::session_privacy::{AccountIdentity, SessionScheme};
use crate::hub::identity::RemoteIdentity;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Choice {
    version: u32,
    pub actor: AccountIdentity,
    pub repository: String,
    pub repository_id: Option<String>,
    pub session: String,
    pub scheme: SessionScheme,
}

impl Choice {
    pub fn new(
        actor: AccountIdentity,
        repository: String,
        identity: Option<&RemoteIdentity>,
        session: String,
        scheme: SessionScheme,
    ) -> Self {
        Self {
            version: 1,
            actor,
            repository,
            repository_id: identity.map(|id| id.agent_id.clone()),
            session,
            scheme,
        }
    }

    pub fn verify(&self, identity: &RemoteIdentity, repository: &str) -> Result<()> {
        ensure!(
            self.actor.hub == identity.hub
                && self
                    .repository_id
                    .as_ref()
                    .map_or(self.repository == repository, |id| id == &identity.agent_id),
            "session publication choice belongs to another destination; use its original destination or a fresh checkout"
        );
        Ok(())
    }

    pub fn save(&self, repo: &Repo) -> Result<()> {
        self.actor.validate()?;
        ensure!(
            self.version == 1 && meta::is_bare_id(&self.session),
            "invalid session publication choice"
        );
        crate::commands::parse_slug(&self.repository)?;
        if let Some(id) = &self.repository_id {
            RemoteIdentity::new(&self.actor.hub, id)?;
        }
        let (path, scope) = location(repo, &self.actor.hub, &self.session)?;
        state::save(&path, &scope, &serde_json::to_vec(self)?)
    }
}

fn location(repo: &Repo, hub: &str, session: &str) -> Result<(std::path::PathBuf, String)> {
    let scope = digest_json(&json!({"version":1,"hub":hub,"session":session}))?;
    Ok((
        repo.common_dir()?
            .join("agit/session-publication-modes")
            .join(scope.trim_start_matches("sha256:")),
        scope,
    ))
}

/// Selection and first registration serialize locally so a confirmed choice cannot be overtaken.
pub(crate) fn lock(repo: &Repo, hub: &str, session: &str) -> Result<fs::File> {
    let (directory, _) = location(repo, hub, session)?;
    crate::infra::config::create_state_dir(&directory)?;
    let lock = crate::infra::config::state_file_options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .context("session publication mode is being selected or registered; retry")?;
    Ok(lock)
}

pub(crate) fn load(repo: &Repo, hub: &str, session: &str) -> Result<Option<Choice>> {
    let (path, scope) = location(repo, hub, session)?;
    let Some(bytes) = state::load(&path, &scope)? else {
        return Ok(None);
    };
    let choice: Choice = serde_json::from_slice(&bytes)?;
    choice.actor.validate()?;
    ensure!(
        choice.version == 1
            && choice.actor.hub == hub
            && choice.session == session
            && meta::is_bare_id(session),
        "invalid session publication choice"
    );
    Ok(Some(choice))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pending name cannot authorize another destination; a bound identity survives renames only.
    #[test]
    fn selection_locks_the_session_and_fences_destination_reuse() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(directory.path()).unwrap();
        let actor = AccountIdentity::new("https://hub.invalid", "owner").unwrap();
        let session = format!("agit-{}", "a".repeat(40));
        let identity =
            RemoteIdentity::new(&actor.hub, "00000000-0000-0000-0000-000000000001").unwrap();
        let guard = lock(&repo, &actor.hub, &session).unwrap();
        assert!(lock(&repo, &actor.hub, &session).is_err());
        let mut choice = Choice::new(
            actor.clone(),
            "owner/project".into(),
            None,
            session.clone(),
            SessionScheme::Plain,
        );
        choice.save(&repo).unwrap();
        assert_eq!(
            load(&repo, &actor.hub, &session).unwrap(),
            Some(choice.clone())
        );
        choice.verify(&identity, "owner/project").unwrap();
        assert!(choice.verify(&identity, "owner/other").is_err());
        choice.repository_id = Some(identity.agent_id.clone());
        choice.verify(&identity, "owner/renamed").unwrap();
        let replacement =
            RemoteIdentity::new(&actor.hub, "00000000-0000-0000-0000-000000000002").unwrap();
        assert!(choice.verify(&replacement, "owner/project").is_err());
        drop(guard);
        assert!(lock(&repo, &actor.hub, &session).is_ok());
    }
}
