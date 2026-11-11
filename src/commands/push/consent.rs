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

/// Set by agit's menu bar for the commands it runs after its own question.
pub(crate) const ASKED_ENV: &str = "AGIT_MENUBAR_ASKED";

/// Who answered a switch-on of automatic upload into a public repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublicAnswer {
    /// A person: at a terminal, or with a click in agit's own menu bar, which passes
    /// `--public-consent` naming the repository its dialog showed.
    Person,
    /// `--yes`, `--json` or `AGIT_YES`, or the menu bar's flag from inside an agent session: an
    /// answer, but not necessarily from the person who will read the result.
    Unattended,
}

/// A public automatic upload is switched on like a private one: by a person, or by `--yes`,
/// `--json` or `AGIT_YES` from any session, an agent's included. Only a switch-on that nobody
/// answered is refused. One answered without a person is recorded by `record_unattended`, so the
/// menu bar tells the person what was switched on and where to turn it off.
///
/// The menu bar sets `ASKED_ENV` for its commands: its own question already answered whether to
/// upload, and only its public question answers whether to upload publicly. There a `--yes` without
/// the consent is no answer for a public destination, so a repository that turned public after
/// its question was asked is refused and asked again in the public words.
pub(crate) fn answer_public_switch_on(
    repository: &str,
    command: &str,
    public_consent: Option<&str>,
) -> crate::Result<PublicAnswer> {
    let in_agent_session = crate::tui::Signals::from_process().agent_session.is_some();
    let yes = (std::env::var_os("AGIT_YES").is_some() || crate::commands::json::requested())
        && std::env::var_os(ASKED_ENV).is_none();
    match answer(
        crate::ui::prompt::can_ask(),
        public_consent == Some(repository),
        in_agent_session,
        yes,
    ) {
        Some(answer) => Ok(answer),
        None => {
            let text = format!(
                "Turning on automatic upload into the public repository {repository} needs a yes: run `{command}` with --yes, or turn it on from the AgentGit menu bar."
            );
            crate::commands::fix::refuse(
                "public_consent_required",
                &text,
                serde_json::json!({"repository": repository}),
            );
            Err(crate::commands::InteractionRequired(text).into())
        }
    }
}

/// Who answered: a terminal outside an agent session, or the menu bar's consent for exactly
/// that repository outside one, is a person; any other explicit answer is unattended; none is
/// no answer.
fn answer(
    at_terminal: bool,
    menu_bar: bool,
    in_agent_session: bool,
    yes: bool,
) -> Option<PublicAnswer> {
    if at_terminal || (menu_bar && !in_agent_session) {
        Some(PublicAnswer::Person)
    } else if yes || menu_bar {
        Some(PublicAnswer::Unattended)
    } else {
        None
    }
}

/// A public automatic upload that was switched on with nobody at a terminal or in the menu bar.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UnattendedSwitchOn {
    pub repository: String,
    /// Seconds since the Unix epoch.
    pub at: u64,
}

/// How many unattended switch-ons are kept for the menu bar to tell; it tells each once.
const UNATTENDED_KEPT: usize = 32;

fn unattended_path() -> Result<std::path::PathBuf> {
    Ok(crate::infra::config::agit_home()?.join("notices/public-switch-on.json"))
}

/// Records that automatic upload into the public `repository` was switched on unattended. The
/// record holds the repository name and a time, both under the agit home and never sent.
pub(crate) fn record_unattended(repository: &str) -> Result<()> {
    record_unattended_at(&unattended_path()?, repository)
}

/// Adds `repository` to the list at `path`. The list is read and replaced under one exclusive
/// lock on a lock file that stays beside it: two switch-ons recorded at once both stay, where a
/// read outside the lock would let the later replacement drop the earlier entry.
fn record_unattended_at(path: &std::path::Path, repository: &str) -> Result<()> {
    let directory = path.parent().context("notice file has no directory")?;
    crate::infra::config::create_state_dir(directory)?;
    let lock = crate::infra::config::state_file_options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut kept = read_unattended(path);
    kept.push(UnattendedSwitchOn {
        repository: repository.into(),
        at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs()),
    });
    let excess = kept.len().saturating_sub(UNATTENDED_KEPT);
    kept.drain(..excess);
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(&serde_json::to_vec(&kept)?)?;
    file.persist(path).map_err(|error| error.error)?;
    let _ = fs2::FileExt::unlock(&lock);
    Ok(())
}

/// The unattended public switch-ons recorded here, oldest first; none when unreadable.
pub(crate) fn unattended() -> Vec<UnattendedSwitchOn> {
    unattended_path()
        .map(|path| read_unattended(&path))
        .unwrap_or_default()
}

fn read_unattended(path: &std::path::Path) -> Vec<UnattendedSwitchOn> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Records the enrollment when switching automatic upload on for a checkout pinned to a public
/// repository, and refuses the switch when nobody answered it.
///
/// A checkout whose destination or audience cannot be read records nothing, and an unread
/// audience says so: automatic publication then refuses a public destination by itself, and the
/// person learns why its uploads may wait.
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
        crate::ui::warning(&format!(
            "agit couldn't read who can see {repository}; if it is public, automatic uploads into it wait until you switch it on again once agit can read that"
        ));
        return Ok(());
    };
    if consent.visibility != "public" {
        return Ok(());
    }
    let answer = answer_public_switch_on(repository, command, public_consent)?;
    consent.save(repo)?;
    crate::ui::warning(&format!(
        "automatic upload into {repository} is on (public: anyone can read it)"
    ));
    if answer == PublicAnswer::Unattended {
        record_unattended(repository)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A person at a terminal, or the menu bar's consent for the repository outside an agent
    /// session, is a person's answer; `--yes` or the flag from an agent's session is an answer
    /// too, recorded as unattended; nothing given is no answer. A gate that refused the agent's
    /// yes would block what the person allowed, and one that took it as the person's would leave
    /// them unaware a public upload was switched on.
    #[test]
    fn a_public_switch_on_is_answered_by_anyone_and_told_when_no_person_answered() {
        use PublicAnswer::{Person, Unattended};
        assert_eq!(answer(true, false, false, false), Some(Person));
        assert_eq!(answer(false, true, false, false), Some(Person));
        assert_eq!(answer(false, false, false, true), Some(Unattended));
        assert_eq!(answer(false, false, true, true), Some(Unattended));
        assert_eq!(answer(false, true, true, false), Some(Unattended));
        assert_eq!(answer(false, false, true, false), None);
        assert_eq!(answer(false, false, false, false), None);
    }

    /// Two switch-ons recorded at once both stay: a writer waits for the list's lock before it
    /// reads. A writer that read before taking it would replace the list with its own copy, and the
    /// person would never be told of the other switch-on.
    #[test]
    fn switch_ons_recorded_at_once_both_stay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notices/public-switch-on.json");
        record_unattended_at(&path, "alice/one").unwrap();
        let held = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.with_extension("lock"))
            .unwrap();
        fs2::FileExt::lock_exclusive(&held).unwrap();
        let (done, recorded) = std::sync::mpsc::channel();
        let writer = {
            let path = path.clone();
            std::thread::spawn(move || {
                record_unattended_at(&path, "alice/two").unwrap();
                done.send(()).unwrap();
            })
        };
        assert!(
            recorded
                .recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "a switch-on was recorded while another writer held the list"
        );
        // The writer holding the lock replaces the list meanwhile.
        let mut kept = read_unattended(&path);
        kept.push(UnattendedSwitchOn {
            repository: "alice/three".into(),
            at: 3,
        });
        fs::write(&path, serde_json::to_vec(&kept).unwrap()).unwrap();
        fs2::FileExt::unlock(&held).unwrap();
        writer.join().unwrap();
        let names: Vec<_> = read_unattended(&path)
            .into_iter()
            .map(|entry| entry.repository)
            .collect();
        assert_eq!(names, ["alice/one", "alice/three", "alice/two"]);
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
