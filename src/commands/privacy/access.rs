//! Confirmed requests retain their scope, revisions, account and recipients across interrupted delivery.

use super::CmdResult;
use crate::domain::{
    privacy_envelope::{PrivacyEnvelope, digest_bytes},
    refs,
    repo::Repo,
    session_grants::{GrantPolicyMutation, GrantScope, GrantTarget, SessionGrantPolicy},
    session_privacy::{
        AcceptedSnapshot, AccountIdentity, OwnerResolution, SESSION_ENVELOPE_VERSION, SessionOwner,
        SessionPrivacy, validate_identifier,
    },
};
use crate::hub::{
    Client,
    identity::RemoteIdentity,
    privacy::grants::{GrantMember, GrantMutation, GrantOperation, GrantState},
};
use crate::{ExitCode, ui};
use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::BTreeSet, io::Write, path::PathBuf};

#[derive(Subcommand)]
pub enum Action {
    /// Inspect the current session authorization policy.
    Policy {
        #[arg(value_name = "OWNER/REPO@REF")]
        target: String,
    },
    /// Preserve grants on accepted commits and disable session-wide authorization.
    DisableSession {
        #[arg(value_name = "OWNER/REPO@REF")]
        target: String,
    },
    /// List readers at the session policy's active scope.
    Show {
        #[arg(value_name = "OWNER/REPO@REF")]
        target: String,
    },
    /// Retired: originals stay on this computer. Existing grants keep reading earlier snapshots.
    Grant {
        #[arg(value_name = "OWNER/REPO@REF")]
        target: String,
    },
    /// Revoke one reader's access to the session or selected commit.
    Revoke {
        #[arg(value_name = "OWNER/REPO@REF")]
        target: String,
        /// Exact account ID or unambiguous username from access show.
        #[arg(long, value_name = "ACCOUNT")]
        member: String,
    },
    /// Stop explicit grants at the active scope; retain eligible default readers.
    Stop {
        #[arg(value_name = "OWNER/REPO@REF")]
        target: String,
    },
    /// Retry an exact confirmed grant or policy request with its frozen revisions.
    Retry {
        #[arg(value_name = "OWNER/REPO")]
        repository: String,
        #[arg(value_name = "REQUEST_ID")]
        request_id: String,
    },
}

struct Selection {
    repo: Repo,
    checkout: String,
    repository: String,
    identity: RemoteIdentity,
    client: Client,
    actor: AccountIdentity,
    snapshot: Option<AcceptedSnapshot>,
    policy: SessionGrantPolicy,
    owner_account_id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedRequest {
    version: u32,
    repository: String,
    identity: RemoteIdentity,
    actor: AccountIdentity,
    snapshot: Option<AcceptedSnapshot>,
    owner_account_id: String,
    request: SavedMutation,
    members: Vec<GrantMember>,
}

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "mutation",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum SavedMutation {
    Grants(GrantMutation),
    Policy(GrantPolicyMutation),
}

impl SavedMutation {
    fn target(&self) -> &GrantTarget {
        match self {
            Self::Grants(request) => &request.target,
            Self::Policy(request) => &request.target,
        }
    }
    fn id(&self) -> &str {
        match self {
            Self::Grants(request) => &request.idempotency_key,
            Self::Policy(request) => &request.idempotency_key,
        }
    }
}

/// What `privacy access grant` answers now that encrypted publication is retired.
const GRANT_RETIRED: &str = "Granting access to originals is retired: sessions publish only the redacted view, and the originals stay on this computer. People you already gave access keep it for earlier snapshots; `agit privacy access show` lists them and `agit privacy access revoke` ends it. Nothing was changed.";

pub(super) fn run(command: Action) -> CmdResult {
    if matches!(command, Action::Grant { .. }) {
        ui::error(GRANT_RETIRED);
        return Ok(ExitCode::Precondition);
    }
    let target = match &command {
        Action::Show { target }
        | Action::Grant { target }
        | Action::Revoke { target, .. }
        | Action::Stop { target }
        | Action::Policy { target }
        | Action::DisableSession { target } => target,
        Action::Retry {
            repository,
            request_id,
        } => return retry(repository, request_id),
    };
    let selected = Selection::resolve(target, !matches!(command, Action::Policy { .. }))?;
    if matches!(command, Action::Policy { .. }) {
        show_policy(&selected.repository, &selected.policy);
        return Ok(ExitCode::Ok);
    }
    let (request, members) = if matches!(command, Action::DisableSession { .. }) {
        (
            SavedMutation::Policy(GrantPolicyMutation::disable(&selected.policy)?),
            Vec::new(),
        )
    } else {
        let target = selected.grant_target()?;
        let revision = selected.policy.policy_revision;
        let (mutation, members) = match command {
            Action::Show { .. } => {
                let state = selected.client.original_grants(
                    &selected.repository,
                    &selected.identity,
                    &target,
                    revision,
                )?;
                show(&selected.repository, &state)?;
                return Ok(ExitCode::Ok);
            }
            Action::Grant { .. } => {
                let preview = selected.client.preview_original_grants(
                    &selected.repository,
                    &selected.identity,
                    &target,
                    revision,
                )?;
                (preview.mutation(&target, revision)?, preview.members)
            }
            Action::Revoke { member, .. } => {
                let state = selected.client.original_grants(
                    &selected.repository,
                    &selected.identity,
                    &target,
                    revision,
                )?;
                let matches: Vec<_> = state
                    .readers
                    .iter()
                    .filter(|reader| {
                        reader.account_id == member || reader.username.as_deref() == Some(&member)
                    })
                    .collect();
                ensure!(
                    matches.len() == 1,
                    "select an exact account ID from access show; the requested member is absent or ambiguous"
                );
                let reader = matches[0];
                (
                    GrantMutation::new(
                        target,
                        state.policy_revision,
                        state.grant_revision,
                        GrantOperation::Revoke {
                            account_id: reader.account_id.clone(),
                        },
                    ),
                    vec![GrantMember {
                        account_id: reader.account_id.clone(),
                        username: reader
                            .username
                            .clone()
                            .unwrap_or_else(|| reader.account_id.clone()),
                    }],
                )
            }
            Action::Stop { .. } => {
                let state = selected.client.original_grants(
                    &selected.repository,
                    &selected.identity,
                    &target,
                    revision,
                )?;
                (
                    GrantMutation::new(
                        target,
                        state.policy_revision,
                        state.grant_revision,
                        GrantOperation::StopExplicit,
                    ),
                    state
                        .readers
                        .into_iter()
                        .filter(|reader| reader.explicit_grant || reader.session_explicit_grant)
                        .map(|reader| GrantMember {
                            username: reader.username.unwrap_or_else(|| reader.account_id.clone()),
                            account_id: reader.account_id,
                        })
                        .collect(),
                )
            }
            _ => unreachable!("policy and retry are handled separately"),
        };
        (SavedMutation::Grants(mutation), members)
    };
    let saved = SavedRequest {
        version: 2,
        repository: selected.repository,
        identity: selected.identity,
        actor: selected.actor,
        snapshot: selected.snapshot,
        owner_account_id: selected.owner_account_id,
        request,
        members,
    };
    saved.validate()?;
    if !saved.confirm()? {
        return Err(crate::commands::Cancelled.into());
    }
    saved.verify_account(&selected.client)?;
    saved.verify_source(&selected.repo)?;
    saved.save(&selected.repo)?;
    saved.submit(&selected.client, &selected.checkout)?;
    Ok(ExitCode::Ok)
}

impl Selection {
    fn resolve(target: &str, manage: bool) -> Result<Self> {
        let spec = crate::commands::target::resolve_local_repo(
            crate::commands::target::parse_spec(target)?,
        )?;
        let spec = crate::commands::context::substitute_at(spec)?;
        ensure!(
            matches!(spec.tail, refs::Tail::None),
            "original access requires a complete snapshot without a turn or path selector"
        );
        let slug = match &spec.repo {
            refs::RepoSel::Slug(owner, name) => format!("{owner}/{name}"),
            _ => crate::commands::context::resolve(&std::env::current_dir()?)?.repo,
        };
        let (repo, checkout) = super::resolve_repo(Some(&slug))?;
        let resolved = refs::resolve(&repo, &spec)?;
        let (_, _, identity) = super::unlock::reader_context(&repo)?;
        let mut found = crate::domain::privacy_git::publication_envelope(
            &repo,
            resolved.branch.as_deref(),
            &resolved.sha,
            &identity,
        )?;
        // A session published encrypted that continues as plain keeps the access to its earlier
        // snapshots' originals: a plain snapshot names that of the newest encrypted one below it.
        if found.1.is_none() {
            let session = crate::domain::storage::metadata_local(repo.root(), &resolved.sha)
                .ok()
                .map(|metadata| metadata.session)
                .filter(|session| crate::domain::meta::is_bare_id(session));
            let line = repo.git(&[
                "rev-list",
                "--first-parent",
                "--max-count=2000",
                &resolved.sha,
            ])?;
            // Only the selected session's own earlier snapshots. An encrypted snapshot names its
            // session in its binding: as published, its metadata carries the public id instead, so
            // it is judged by the binding. Any other commit of another session, such as the one
            // this session was forked from, ends the walk. A snapshot whose envelope can't be read
            // fails the access: walked past, it would hand out an older original, or none.
            for commit in line.lines().skip(1) {
                let Some(session) = &session else {
                    break;
                };
                let candidate = crate::domain::privacy_git::publication_envelope(
                    &repo,
                    resolved.branch.as_deref(),
                    commit,
                    &identity,
                )?;
                if candidate.1.is_some() {
                    let bound = candidate.1.as_deref().and_then(|bytes| {
                        PrivacyEnvelope::parse(bytes)
                            .ok()?
                            .session_binding
                            .map(|binding| binding.session_id)
                    });
                    if bound.as_deref() == Some(session.as_str()) {
                        found = candidate;
                    }
                    break;
                }
                let same = crate::domain::storage::metadata_local(repo.root(), commit)
                    .is_ok_and(|metadata| metadata.session == *session);
                if !same {
                    break;
                }
            }
        }
        let (published_commit, bytes) = found;
        let snapshot = bytes
            .map(|bytes| -> Result<_> {
                let envelope = PrivacyEnvelope::parse(&bytes)?;
                ensure!(
                    envelope.format_version == SESSION_ENVELOPE_VERSION,
                    "original access requires session-owner encryption"
                );
                let snapshot = AcceptedSnapshot {
                    version: SESSION_ENVELOPE_VERSION,
                    binding: envelope
                        .session_binding
                        .context("snapshot has no session authority")?,
                    published_commit,
                    envelope_digest: digest_bytes(&bytes),
                };
                snapshot.validate()?;
                Ok(snapshot)
            })
            .transpose()?;
        let source_id = snapshot
            .as_ref()
            .map_or(identity.agent_id.as_str(), |snapshot| {
                snapshot.binding.repository_id.as_str()
            });
        let (client, repository, identity) = super::unlock::source_context(&repo, source_id)?;
        let actor = client.session_account()?;
        let (session, owner_account_id) = if let Some(snapshot) = &snapshot {
            (
                snapshot.binding.session_id.clone(),
                snapshot.binding.owner_key.account_id.clone(),
            )
        } else {
            let metadata = crate::domain::storage::metadata_local(repo.root(), &resolved.sha)?;
            let _mode_lock =
                crate::domain::privacy_git::modes::lock(&repo, &identity.hub, &metadata.session)?;
            let choice =
                crate::domain::privacy_git::modes::load(&repo, &identity.hub, &metadata.session)?;
            if let Some(choice) = &choice {
                choice.verify(&identity, &repository)?;
                ensure!(
                    choice.scheme != crate::domain::session_privacy::SessionScheme::Plain,
                    "plain sessions use repository read access; original grants require session encryption"
                );
            }
            ensure!(
                metadata.is_session_line() && crate::domain::meta::is_bare_id(&metadata.session),
                "original access requires one identified logical session"
            );
            let mut local = metadata
                .session_privacy
                .clone()
                .unwrap_or(SessionPrivacy::new(metadata.committer_account.clone())?);
            if let Some(choice) = choice {
                local.scheme = choice.scheme;
            }
            let response = match client.registered_session_privacy(
                &repository,
                &identity,
                &metadata.session,
            )? {
                Some(response) => {
                    ensure!(
                        response.config.privacy.scheme
                            != crate::domain::session_privacy::SessionScheme::Plain,
                        "plain sessions use repository read access; original grants require session encryption"
                    );
                    local.scheme = response.config.privacy.scheme;
                    response
                }
                None => {
                    ensure!(
                        local.scheme != crate::domain::session_privacy::SessionScheme::Plain,
                        "plain sessions use repository read access; access to originals applies only to sessions published encrypted earlier"
                    );
                    let privacy = if local.owner_source == OwnerResolution::Unresolved {
                        let mut privacy = SessionPrivacy::new(Some(actor.clone()))?;
                        privacy.scheme = local.scheme;
                        privacy
                    } else {
                        local.clone()
                    };
                    ensure!(
                        privacy.owner == Some(SessionOwner::User(actor.account_id.clone())),
                        "only the session owner can manage original access"
                    );
                    if privacy.scheme
                        == crate::domain::session_privacy::SessionScheme::SessionOwnerV2
                    {
                        client.ensure_session_key(&actor)?;
                    }
                    client.register_session_privacy(
                        &repository,
                        &identity,
                        &metadata.session,
                        &privacy,
                        false,
                    )?
                }
            };
            response
                .config
                .verify_registration(&identity.agent_id, &metadata.session, &local)?;
            let Some(SessionOwner::User(owner)) = response.config.privacy.owner else {
                anyhow::bail!("original access management requires an account session owner");
            };
            (metadata.session, owner)
        };
        ensure!(
            !manage || actor.account_id == owner_account_id,
            "only the session owner can manage original access"
        );
        let policy = client.original_grant_policy(&repository, &identity, &session)?;
        Ok(Self {
            repo,
            checkout,
            repository,
            identity,
            client,
            actor,
            snapshot,
            policy,
            owner_account_id,
        })
    }

    fn grant_target(&self) -> Result<GrantTarget> {
        let target = match self.policy.grant_scope {
            GrantScope::Session => self.policy.target()?,
            GrantScope::Commit => GrantTarget::from_snapshot(self.snapshot.as_ref()
                .context("commit-scoped original access requires an accepted snapshot; publish or fetch it first")?)?,
        };
        self.policy.verify_target(&target)?;
        Ok(target)
    }
}

impl SavedRequest {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 2,
            "unsupported saved original access request"
        );
        if let Some(snapshot) = &self.snapshot {
            snapshot.validate()?;
            ensure!(
                snapshot.binding.repository_id == self.identity.agent_id
                    && snapshot.binding.session_id == self.request.target().session_id()
                    && snapshot.binding.owner_key.account_id == self.owner_account_id,
                "saved original access snapshot authority is inconsistent"
            );
        }
        match &self.request {
            SavedMutation::Grants(request) => {
                request.validate()?;
                if request.target.scope() == GrantScope::Commit {
                    request.target.verify(&GrantTarget::from_snapshot(
                        self.snapshot
                            .as_ref()
                            .context("saved commit grant has no snapshot")?,
                    )?)?;
                }
            }
            SavedMutation::Policy(request) => {
                request.validate()?;
                ensure!(
                    request.grant_scope == GrantScope::Commit && self.members.is_empty(),
                    "saved policy operation is unsupported"
                );
            }
        }
        validate_identifier(&self.owner_account_id)?;
        self.actor.validate()?;
        crate::commands::parse_slug(&self.repository)?;
        ensure!(
            RemoteIdentity::new(&self.identity.hub, &self.identity.agent_id)? == self.identity
                && self.actor.hub == self.identity.hub
                && self.request.target().agent_id() == self.identity.agent_id
                && self.actor.account_id == self.owner_account_id,
            "saved original access authority is inconsistent"
        );
        let mut accounts = BTreeSet::new();
        for member in &self.members {
            validate_identifier(&member.account_id)?;
            validate_identifier(&member.username)?;
            ensure!(
                accounts.insert(&member.account_id),
                "saved original access repeats a member"
            );
        }
        if let SavedMutation::Grants(GrantMutation {
            action: GrantOperation::Revoke { account_id },
            ..
        }) = &self.request
        {
            ensure!(
                self.members.len() == 1 && self.members[0].account_id == *account_id,
                "saved revocation differs from its confirmed member"
            );
        }
        Ok(())
    }

    fn verify_account(&self, client: &Client) -> Result<()> {
        ensure!(
            client.session_account()? == self.actor,
            "original access account changed"
        );
        let credential = crate::infra::credentials::load_checked(&self.identity.hub)?
            .context("original access account is no longer signed in")?;
        ensure!(
            client.credential_username().as_deref() == Some(credential.username.as_str())
                && credential
                    .account_id
                    .as_ref()
                    .is_none_or(|id| id == &self.actor.account_id),
            "the selected original access account changed during confirmation"
        );
        Ok(())
    }

    fn verify_source(&self, repo: &Repo) -> Result<()> {
        let (_, _, identity) = super::unlock::source_context(repo, &self.identity.agent_id)?;
        ensure!(
            identity == self.identity,
            "original access destination changed during confirmation"
        );
        Ok(())
    }

    fn confirm(&self) -> Result<bool> {
        let scope = self.request.target().scope().as_str();
        let operation = match &self.request {
            SavedMutation::Policy(_) => {
                eprintln!(
                    "Current effective recipients keep originals of all already accepted commits across branches. Later commits require their own explicit grants. Re-enabling session authorization is unsupported."
                );
                "Disable session-level original authorization".to_string()
            }
            SavedMutation::Grants(request) => match request.action {
                GrantOperation::Grant { .. } => {
                    eprintln!(
                        "{}",
                        if request.target.scope() == GrantScope::Session {
                            "Listed recipients gain complete originals of all existing and future accepted commits in this session. Later repository members are not added automatically."
                        } else {
                            "Listed recipients gain complete originals of only the selected saved commit."
                        }
                    );
                    format!("Grant {scope} original access to the listed members")
                }
                GrantOperation::Revoke { .. } => format!(
                    "Revoke default and explicit original access for the listed member at {scope} scope"
                ),
                GrantOperation::StopExplicit => format!(
                    "Stop explicit grants at {scope} scope and retain only eligible defaults"
                ),
            },
        };
        eprintln!("{}", target_label(&self.repository, self.request.target()));
        for member in &self.members {
            eprintln!("  {} ({})", member.username, member.account_id);
        }
        if std::env::var_os("AGIT_YES").is_some() {
            return Ok(true);
        }
        ui::prompt::answer(&format!("{operation}?"), false)?.ok_or_else(|| {
            crate::commands::InteractionRequired("original access changes require confirmation; use --yes or an interactive terminal".into()).into()
        })
    }

    fn save(&self, repo: &Repo) -> Result<()> {
        let path = request_path(repo, self.request.id())?;
        let parent = path
            .parent()
            .context("original access request has no directory")?;
        crate::infra::config::create_state_dir(parent)?;
        let mut staged = tempfile::NamedTempFile::new_in(parent)?;
        staged.write_all(&serde_json::to_vec(self)?)?;
        staged.as_file().sync_all()?;
        staged
            .persist_noclobber(path)
            .map_err(|error| error.error)?;
        Ok(())
    }

    fn submit(&self, client: &Client, checkout: &str) -> Result<()> {
        let result: Result<serde_json::Value> = match &self.request {
            SavedMutation::Grants(request) => client.mutate_original_grants(&self.repository, &self.identity, request)
                .map(|result| json!({"operation":"privacy_access_update", "repository":self.repository,
                    "request_id":self.request.id(), "action":request.action, "members":self.members,
                    "target":result.target,"policy_revision":result.policy_revision,"grant_revision":result.grant_revision})),
            SavedMutation::Policy(request) => client.change_original_grant_policy(&self.repository, &self.identity, request)
                .map(|result| json!({"operation":"privacy_access_policy_update", "repository":self.repository,
                    "request_id":self.request.id(),"policy":result.policy,
                    "converted_recipients":result.converted_recipients,"converted_snapshots":result.converted_snapshots})),
        };
        let result = result.map_err(|error| {
            if let Some(api) = error.downcast_ref::<crate::hub::client::ApiError>() {
                if api.kind == "unsupported" {
                    return error;
                }
                if api.status == 409 {
                    return error.context(format!(
                        "request {} needs a new confirmation; inspect original access first",
                        self.request.id()
                    ));
                }
                if matches!(api.status, 401 | 403) {
                    return error;
                }
            }
            error.context(format!(
                "request {} is retained; retry it with `agit privacy access retry {} {}`",
                self.request.id(),
                checkout,
                self.request.id()
            ))
        })?;
        if crate::commands::json::requested() {
            println!("{result}");
        } else {
            match &self.request {
                SavedMutation::Grants(request) => ui::success(&format!(
                    "Original access updated at {} scope.",
                    request.target.scope().as_str()
                )),
                SavedMutation::Policy(_) => {
                    ui::success("Session-level original authorization is disabled.");
                    println!(
                        "Converted recipients: {}; accepted snapshots: {}.",
                        result["converted_recipients"], result["converted_snapshots"]
                    );
                }
            }
            println!("Request: {}", self.request.id());
        }
        Ok(())
    }
}

fn retry(repository: &str, request_id: &str) -> CmdResult {
    let (repo, _) = super::resolve_repo(Some(repository))?;
    let saved: SavedRequest = serde_json::from_slice(&crate::domain::storage::read_bytes_capped(
        &request_path(&repo, request_id)?,
        4 * 1024 * 1024,
    )?)?;
    saved.validate()?;
    ensure!(
        saved.request.id() == request_id,
        "saved original access request ID changed"
    );
    let (client, _, identity) = super::unlock::source_context(&repo, &saved.identity.agent_id)?;
    ensure!(
        saved.identity == identity,
        "saved original access destination changed"
    );
    saved.verify_account(&client)?;
    if grants_access(&saved.request) {
        ui::error(GRANT_RETIRED);
        return Ok(ExitCode::Precondition);
    }
    if !saved.confirm()? {
        return Err(crate::commands::Cancelled.into());
    }
    saved.verify_account(&client)?;
    saved.verify_source(&repo)?;
    saved.submit(&client, repository)?;
    Ok(ExitCode::Ok)
}

/// Whether a saved request gives someone access to originals, which is retired; ending access
/// is not.
fn grants_access(request: &SavedMutation) -> bool {
    matches!(
        request,
        SavedMutation::Grants(GrantMutation {
            action: GrantOperation::Grant { .. },
            ..
        })
    )
}

fn request_path(repo: &Repo, id: &str) -> Result<PathBuf> {
    ensure!(
        uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id),
        "original access retry requires a saved request ID"
    );
    Ok(repo
        .common_dir()?
        .join("agit/privacy-access")
        .join(format!("{id}.json")))
}

fn show(repository: &str, state: &GrantState) -> Result<()> {
    if crate::commands::json::requested() {
        println!(
            "{}",
            json!({"operation":"privacy_access_show", "repository":repository, "access":state})
        );
        return Ok(());
    }
    println!("{}", target_label(repository, &state.target));
    println!(
        "Policy revision: {}; grant revision: {}.",
        state.policy_revision, state.grant_revision
    );
    for reader in &state.readers {
        let mut labels = Vec::new();
        if reader.owner_default {
            labels.push("owner default");
        }
        if reader.committer_default {
            labels.push("committer default");
        }
        if reader.explicit_grant {
            labels.push("explicit grant");
        }
        if reader.session_explicit_grant {
            labels.push("session explicit grant");
        }
        if reader.session_revoked {
            labels.push("session revoked");
        }
        if reader.default_suppressed {
            labels.push("default access suppressed");
        }
        if reader.session_membership_invalidated {
            labels.push("session membership invalidated");
        }
        if reader.revoked {
            labels.push("revoked");
        }
        if reader.membership_invalidated {
            labels.push("membership invalidated");
        }
        if !reader.current_member {
            labels.push("not a current member");
        }
        println!(
            "{} ({}): {} [{}]",
            reader.username.as_deref().unwrap_or(&reader.account_id),
            reader.account_id,
            match (state.target.scope(), reader.can_read) {
                (GrantScope::Session, true) => "session explicit access",
                (GrantScope::Session, false) =>
                    "no session explicit access; snapshot defaults may apply",
                (GrantScope::Commit, true) => "can read original",
                (GrantScope::Commit, false) => "no original access",
            },
            labels.join(", ")
        );
    }
    Ok(())
}

fn target_label(repository: &str, target: &GrantTarget) -> String {
    match target {
        GrantTarget::Session { session_id, .. } => {
            format!("Session: {repository}@{session_id} (all existing and future accepted commits)")
        }
        GrantTarget::Commit {
            published_commit_id,
            ..
        } => format!("Commit: {repository}@{published_commit_id}"),
    }
}

fn show_policy(repository: &str, policy: &SessionGrantPolicy) {
    if crate::commands::json::requested() {
        println!(
            "{}",
            json!({"operation":"privacy_access_policy", "repository":repository, "policy":policy})
        );
    } else {
        println!("Session: {repository}@{}", policy.session_id);
        println!(
            "Original grant scope: {}. Policy revision: {}; session grant revision: {}.",
            policy.grant_scope.as_str(),
            policy.policy_revision,
            policy.grant_revision
        );
        if policy.grant_scope == GrantScope::Commit {
            println!("Re-enabling session-level authorization is unsupported.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grant saved before encrypted publication was retired is not sent on retry; a saved
    /// revoke or stop still is, so the owner can end access people already have.
    #[test]
    fn a_saved_grant_is_retired_and_ending_access_is_not() {
        let saved = |action| {
            SavedMutation::Grants(GrantMutation::new(
                GrantTarget::Session {
                    agent_id: "agent".into(),
                    session_id: "session".into(),
                },
                1,
                1,
                action,
            ))
        };
        assert!(grants_access(&saved(GrantOperation::Grant {
            preview_id: "preview".into()
        })));
        assert!(!grants_access(&saved(GrantOperation::Revoke {
            account_id: "bob".into()
        })));
        assert!(!grants_access(&saved(GrantOperation::StopExplicit)));
    }
}
