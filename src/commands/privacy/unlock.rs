//! Recovery requests the exact publication and installs only authenticated original content.

use super::super::CmdResult;
use crate::domain::{
    privacy::PrivacyPolicy,
    privacy_envelope::{PrivacyEnvelope, digest_bytes},
    privacy_layer::PrivateLayer,
    privacy_recovery::RecoveredSnapshot,
    refs,
    repo::Repo,
    session_privacy::{
        AcceptedSnapshot, SESSION_ENVELOPE_VERSION, SessionOwner, SessionPrivacy, SessionScheme,
    },
};
use crate::{
    ExitCode,
    hub::{Client, identity::RemoteIdentity},
    infra::config,
    ui,
};
use anyhow::{Context, ensure};
use std::path::{Path, PathBuf};

pub(super) fn run(target: &str, workspace: Option<&Path>) -> CmdResult {
    let spec =
        crate::commands::target::resolve_local_repo(crate::commands::target::parse_spec(target)?)?;
    let spec = crate::commands::context::substitute_at(spec)?;
    ensure!(
        matches!(spec.tail, refs::Tail::None),
        "privacy unlock requires a complete snapshot, without a path or turn selector"
    );
    let slug = match &spec.repo {
        refs::RepoSel::Slug(owner, name) => format!("{owner}/{name}"),
        _ => crate::commands::context::resolve(&std::env::current_dir()?)?.repo,
    };
    let (repo, slug) = super::resolve_repo(Some(&slug))?;
    let resolved = refs::resolve(&repo, &spec)?;
    let policy = PrivacyPolicy::load(&repo)?;
    let workspace = workspace
        .or(policy.workspace.as_deref())
        .context(
            "bind a local workspace with `agit privacy policy set-workspace` or pass --workspace",
        )?
        .canonicalize()
        .context("recovery workspace is unavailable")?;
    ensure!(workspace.is_dir(), "recovery workspace must be a directory");
    let bytes = envelope_bytes(&repo, &resolved.sha)?
        .context("this snapshot has no encrypted privacy envelope")?;
    let envelope = PrivacyEnvelope::parse(&bytes)?;
    let cache = repo.common_dir()?.join("agit/privacy-recovery");
    let destination = cache.join(&resolved.sha);
    ensure!(
        !destination.exists(),
        "this snapshot already has local recovery data"
    );

    ensure!(
        envelope.format_version == SESSION_ENVELOPE_VERSION,
        "unsupported obsolete encryption format; repository-password recovery is unavailable"
    );
    recover_selected(&repo, &resolved.sha, &workspace, &bytes, &envelope)?;
    report_recovery(&slug, &resolved.sha, &destination, &workspace);
    Ok(ExitCode::Ok)
}

pub(crate) fn recover_session(
    repo: &Repo,
    commit: &str,
    workspace: &Path,
) -> anyhow::Result<Option<RecoveredSnapshot>> {
    let Some(bytes) = envelope_bytes(repo, commit)? else {
        return Ok(None);
    };
    let envelope = PrivacyEnvelope::parse(&bytes)?;
    ensure!(
        envelope.format_version == SESSION_ENVELOPE_VERSION,
        "unsupported obsolete encryption format; repository-password recovery is unavailable"
    );
    recover_selected(repo, commit, workspace, &bytes, &envelope)?;
    Ok(Some(RecoveredSnapshot::load(repo, commit)?.context(
        "recovered original is unavailable after installation",
    )?))
}

pub(crate) fn envelope_bytes(repo: &Repo, commit: &str) -> anyhow::Result<Option<Vec<u8>>> {
    crate::domain::privacy_recovery::committed_envelope(repo, commit)
}

fn recover_selected(
    repo: &Repo,
    commit: &str,
    workspace: &Path,
    bytes: &[u8],
    envelope: &PrivacyEnvelope,
) -> anyhow::Result<()> {
    let (_, layer) = open_session_original(repo, commit, bytes, envelope, false)?;
    let layer = layer.context("original access is unavailable")?;
    install_recovery(repo, commit, workspace, bytes, &layer)?;
    Ok(())
}

/// Absent credentials or denied source access permits sanitized context; transient failures remain errors.
pub(crate) fn open_session_original(
    repo: &Repo,
    commit: &str,
    bytes: &[u8],
    envelope: &PrivacyEnvelope,
    allow_sanitized: bool,
) -> anyhow::Result<(AcceptedSnapshot, Option<PrivateLayer>)> {
    ensure!(
        envelope.format_version == SESSION_ENVELOPE_VERSION,
        "this encryption format cannot supply independent fork context"
    );
    let expected = AcceptedSnapshot {
        version: SESSION_ENVELOPE_VERSION,
        binding: envelope
            .session_binding
            .clone()
            .context("missing session authority")?,
        published_commit: commit.to_owned(),
        envelope_digest: digest_bytes(bytes),
    };
    expected.validate()?;
    let (client, repository, identity) = reader_context(repo)?;
    if allow_sanitized && client.checked_access_token()?.is_none() {
        return Ok((expected, None));
    }
    let source_identity = RemoteIdentity::new(&identity.hub, &expected.binding.repository_id)?;
    let response = publication_source(
        repo,
        &client,
        repository,
        identity,
        &expected.binding.repository_id,
    )
    .and_then(|(repository, identity)| {
        client.session_original_key(&repository, &identity, &expected)
    });
    let response = match response {
        Ok(response) => response,
        Err(error)
            if allow_sanitized
                && (error.is::<crate::hub::privacy::location::SourceUnavailable>()
                    || error
                        .downcast_ref::<crate::hub::client::ApiError>()
                        .is_some_and(|api| {
                            api.status == 403 && api.kind == "original_access_denied"
                        })) =>
        {
            return Ok((expected, None));
        }
        Err(error) => return Err(error),
    };
    let layer = response.open_layer(&expected, bytes)?;
    drop(response);
    let ownership: SessionPrivacy = serde_json::from_value(
        layer
            .metadata
            .get("session_privacy")
            .cloned()
            .context("session original has no ownership record")?,
    )?;
    ownership.validate()?;
    ensure!(
        ownership.scheme == SessionScheme::SessionOwnerV2
            && ownership.hub.as_deref() == Some(source_identity.hub.as_str())
            && ownership.owner
                == Some(SessionOwner::User(
                    expected.binding.owner_key.account_id.clone()
                )),
        "session original ownership differs from its publication authority"
    );
    Ok((expected, Some(layer)))
}

fn install_recovery(
    repo: &Repo,
    commit: &str,
    workspace: &Path,
    bytes: &[u8],
    layer: &PrivateLayer,
) -> anyhow::Result<PathBuf> {
    let cache = repo.common_dir()?.join("agit/privacy-recovery");
    let destination = cache.join(commit);
    config::create_state_dir(&cache)?;
    let staging = layer.restore(&cache, workspace)?;
    layer.import_protection(repo)?;
    std::fs::write(
        staging.path().join("publication-digest"),
        digest_bytes(bytes),
    )?;
    crate::domain::privacy_recovery::write_manifest(staging.path(), bytes)?;
    ensure!(
        !destination.exists(),
        "recovery data appeared while authorization was pending"
    );
    std::fs::rename(staging.path(), &destination)
        .context("cannot install private recovery data")?;
    Ok(destination)
}

fn report_recovery(slug: &str, commit: &str, destination: &Path, workspace: &Path) {
    if crate::commands::json::requested() {
        println!(
            "{}",
            serde_json::json!({"operation": "privacy_unlock", "repository": slug, "snapshot": commit, "recovery_directory": destination, "workspace": workspace, "key_source": "account"})
        );
    } else {
        ui::success("private session recovered locally");
        println!("recovery directory: {}", destination.display());
    }
}

pub(super) fn reader_context(
    repo: &crate::domain::repo::Repo,
) -> anyhow::Result<(Client, String, RemoteIdentity)> {
    let identity = crate::hub::identity::read(repo)?.context("this checkout has no immutable source identity; clone the repository before recovering originals")?;
    let origin = repo.remote_url().context("this checkout has no source remote; restore its verified repository URL before recovering originals")?;
    let (hub, repository) = super::sources::remote_scope(&origin)?;
    ensure!(
        hub == identity.hub,
        "the source remote belongs to another Hub; refusing original access lookup"
    );
    let client = Client::for_stored_hub(&identity.hub);
    Ok((client, repository, identity))
}

pub(crate) fn source_context(
    repo: &Repo,
    repository_id: &str,
) -> anyhow::Result<(Client, String, RemoteIdentity)> {
    let (client, repository, identity) = reader_context(repo)?;
    let (repository, identity) =
        publication_source(repo, &client, repository, identity, repository_id)?;
    Ok((client, repository, identity))
}

fn publication_source(
    repo: &Repo,
    client: &Client,
    repository: String,
    identity: RemoteIdentity,
    repository_id: &str,
) -> anyhow::Result<(String, RemoteIdentity)> {
    if identity.agent_id == repository_id {
        return Ok((repository, identity));
    }
    let source = RemoteIdentity::new(&identity.hub, repository_id)?;
    let repository = match super::sources::retained(repo, &source)? {
        Some(repository) => repository,
        None => client.repository_location(&source)?,
    };
    Ok((repository, source))
}
