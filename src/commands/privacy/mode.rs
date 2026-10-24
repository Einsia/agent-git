//! Publication mode selection precedes immutable remote registration.

use super::*;
use crate::domain::{
    meta,
    privacy_git::modes::{self, Choice},
    refs,
    session_privacy::{SessionOwner, SessionScheme},
    storage,
};
use crate::hub::{
    Client,
    identity::{self, RemoteIdentity},
    privacy::sessions::SessionPrivacyResponse,
};

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Mode {
    Plain,
    Encrypted,
}

impl Mode {
    fn scheme(self) -> SessionScheme {
        match self {
            Self::Plain => SessionScheme::Plain,
            Self::Encrypted => SessionScheme::SessionOwnerV2,
        }
    }
}

pub(super) struct Selection {
    pub repo: Repo,
    pub repository: String,
    pub client: Client,
    pub identity: Option<RemoteIdentity>,
    pub metadata: meta::Meta,
    pub choice: Option<Choice>,
    pub registered: Option<SessionPrivacyResponse>,
    pub visibility: String,
    commit: String,
}

impl Selection {
    pub fn resolve(target: &str) -> Result<Self> {
        let spec = crate::commands::target::resolve_local_repo(
            crate::commands::target::parse_spec(target)?,
        )?;
        let spec = crate::commands::context::substitute_at(spec)?;
        ensure!(
            matches!(spec.tail, refs::Tail::None),
            "publication mode requires a complete session without a turn or path selector"
        );
        let slug = match &spec.repo {
            refs::RepoSel::Slug(owner, name) => format!("{owner}/{name}"),
            _ => crate::commands::context::resolve(&std::env::current_dir()?)?.repo,
        };
        let (repo, _) = super::resolve_repo(Some(&slug))?;
        let resolved = refs::resolve(&repo, &spec)?;
        let mut metadata = storage::metadata_local(repo.root(), &resolved.sha)?;
        ensure!(
            metadata.is_session_line() && meta::is_bare_id(&metadata.session),
            "select a settled session before choosing its publication mode"
        );
        let pin = identity::read(&repo)?;
        let (client, repository, pin) =
            if let Some(bytes) = super::unlock::envelope_bytes(&repo, &resolved.sha)? {
                let envelope = crate::domain::privacy_envelope::PrivacyEnvelope::parse(&bytes)?;
                let binding = envelope
                    .session_binding
                    .context("snapshot has no session publication authority")?;
                let (client, repository, identity) =
                    super::unlock::source_context(&repo, &binding.repository_id)?;
                metadata.session = binding.session_id;
                (client, repository, Some(identity))
            } else if pin.is_some() {
                let (client, repository, _) = super::unlock::reader_context(&repo)?;
                (client, repository, pin)
            } else {
                (Client::from_env(), slug, pin)
            };
        let (owner, name) = crate::commands::parse_slug(&repository)?;
        let remote = match client.get_agent(&owner, &name) {
            Ok(remote) => Some(remote),
            Err(error)
                if pin.is_none()
                    && error
                        .downcast_ref::<crate::hub::client::ApiError>()
                        .is_some_and(|api| api.status == 404) =>
            {
                None
            }
            Err(error) => return Err(error),
        };
        let identity = remote
            .as_ref()
            .map(|remote| -> Result<_> {
                ensure!(
                    remote.owner == owner && remote.name == name,
                    "Hub returned another publication destination"
                );
                let id = RemoteIdentity::new(client.base(), &remote.agent_id)?;
                ensure!(
                    pin.as_ref().is_none_or(|pin| pin == &id),
                    "publication destination differs from the checkout identity"
                );
                Ok(id)
            })
            .transpose()?;
        let registered = identity
            .as_ref()
            .map(|id| client.registered_session_privacy(&repository, id, &metadata.session))
            .transpose()?
            .flatten();
        let choice = modes::load(&repo, client.base(), &metadata.session)?;
        if let (Some(choice), Some(id)) = (&choice, &identity) {
            choice.verify(id, &repository)?;
        }
        let visibility = remote
            .map(|remote| remote.visibility)
            .unwrap_or_else(|| "selected at first push".into());
        Ok(Self {
            repo,
            repository,
            client,
            identity,
            metadata,
            choice,
            registered,
            visibility,
            commit: resolved.sha,
        })
    }

    pub fn show(&self) {
        let local = self
            .choice
            .as_ref()
            .map(|choice| choice.scheme)
            .unwrap_or_else(|| {
                self.metadata
                    .session_privacy
                    .as_ref()
                    .map_or(SessionScheme::Plain, |privacy| privacy.scheme)
            });
        let registered = self
            .registered
            .as_ref()
            .map(|response| response.config.privacy.scheme);
        if crate::commands::json::requested() {
            println!(
                "{}",
                serde_json::json!({"operation":"privacy_mode","repository":self.repository,"session_id":self.metadata.session,"local_mode":local,"registered_mode":registered,"registration_locked":registered.is_some()})
            );
        } else {
            println!("{}: session {}", self.repository, self.metadata.session);
            println!(
                "Local publication mode: {}{}.",
                label(local),
                if self.choice.is_some() {
                    " (explicit choice)"
                } else {
                    " (snapshot default)"
                }
            );
            match registered {
                Some(mode) => println!("Registered publication mode: {} (locked).", label(mode)),
                None => println!("Publication mode is not registered yet."),
            }
        }
    }
}

fn label(scheme: SessionScheme) -> &'static str {
    match scheme {
        SessionScheme::Plain => "plain",
        SessionScheme::SessionOwnerV2 => "encrypted",
    }
}

pub(super) fn run(target: &str, mode: Option<Mode>) -> CmdResult {
    let mut selected = Selection::resolve(target)?;
    let Some(mode) = mode else {
        selected.show();
        return Ok(ExitCode::Ok);
    };
    let scheme = mode.scheme();
    if let Some(registered) = &selected.registered {
        ensure!(
            registered.config.privacy.scheme == scheme,
            "session publication mode is already registered and cannot be changed"
        );
        if let Some(choice) = &mut selected.choice
            && choice.scheme != scheme
        {
            let _lock = modes::lock(
                &selected.repo,
                &selected.client.session_account()?.hub,
                &selected.metadata.session,
            )?;
            choice.scheme = scheme;
            choice.save(&selected.repo)?;
        }
        selected.show();
        return Ok(ExitCode::Ok);
    }
    ensure!(
        super::unlock::envelope_bytes(&selected.repo, &selected.commit)?.is_none(),
        "accepted encrypted snapshots cannot change publication mode"
    );
    let actor = selected.client.session_account()?;
    if let Some(privacy) = &selected.metadata.session_privacy {
        ensure!(
            privacy.hub.as_ref().is_none_or(|hub| hub == &actor.hub)
                && privacy
                    .owner
                    .as_ref()
                    .is_none_or(|owner| owner == &SessionOwner::User(actor.account_id.clone()))
                && privacy
                    .creator
                    .as_ref()
                    .is_none_or(|creator| creator == &actor.account_id),
            "only the session creator can choose its initial publication mode"
        );
    } else if let Some(creator) = &selected.metadata.committer_account {
        ensure!(
            creator == &actor,
            "only the session creator can choose its initial publication mode"
        );
    }
    eprintln!(
        "Session {} in {}: {}. Repository audience: {}.",
        selected.metadata.session,
        selected.repository,
        label(scheme),
        selected.visibility
    );
    if scheme == SessionScheme::Plain {
        eprintln!(
            "All unpublished snapshots and future commits in this session will publish without session encryption. Repository readers can read the published content. Local secret protection still applies."
        );
    }
    if std::env::var_os("AGIT_YES").is_none() && !ui::prompt::confirm("Save this session publication mode?", false)?.ok_or_else(|| crate::commands::InteractionRequired("publication mode changes require confirmation; use --yes or an interactive terminal".into()))? {
        return Ok(ExitCode::Ok);
    }
    let _lock = modes::lock(&selected.repo, &actor.hub, &selected.metadata.session)?;
    let current = Selection::resolve(target)?;
    ensure!(
        current.commit == selected.commit
            && current.identity == selected.identity
            && current.choice == selected.choice
            && current.client.session_account()? == actor,
        "session or destination changed during confirmation; retry"
    );
    ensure!(
        current.registered.is_none(),
        "session was registered during confirmation; inspect its mode before retrying"
    );
    let choice = Choice::new(
        actor,
        selected.repository.clone(),
        selected.identity.as_ref(),
        selected.metadata.session.clone(),
        scheme,
    );
    choice.save(&selected.repo)?;
    Selection {
        choice: Some(choice),
        ..selected
    }
    .show();
    Ok(ExitCode::Ok)
}
