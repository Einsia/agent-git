//! Unlock retains keys in memory unless the caller explicitly requests scoped OS storage.

use super::super::CmdResult;
use crate::domain::{
    privacy::PrivacyPolicy,
    privacy_credentials::{self, CredentialStore, OsUnlockStore, Scope},
    privacy_envelope::{MAX_ENVELOPE_BYTES, PrivacyEnvelope, digest_bytes},
    refs,
};
use crate::{ExitCode, hub::Client, infra::config, ui};
use anyhow::{Context, ensure};
use chrono::Utc;
use std::path::Path;
use zeroize::Zeroizing;

pub(super) fn run(
    target: &str,
    workspace: Option<&Path>,
    remember_for: Option<u16>,
    use_saved_key: bool,
) -> CmdResult {
    run_with_store(
        target,
        workspace,
        remember_for,
        use_saved_key,
        &OsUnlockStore,
        || ui::prompt::repository_password("Repository viewing password", false),
    )
}

fn run_with_store(
    target: &str,
    workspace: Option<&Path>,
    remember_for: Option<u16>,
    use_saved_key: bool,
    store: &impl CredentialStore,
    password: impl FnOnce() -> anyhow::Result<Zeroizing<String>>,
) -> CmdResult {
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
    let object = format!("{}:privacy/envelope.json", resolved.sha);
    let size: usize = repo
        .git(&["cat-file", "-s", &object])
        .context("this snapshot has no encrypted privacy envelope")?
        .trim()
        .parse()?;
    ensure!(
        size <= MAX_ENVELOPE_BYTES,
        "privacy envelope exceeds the size limit"
    );
    let bytes = repo.git_bytes_result(&["cat-file", "blob", &object])?;
    let envelope = PrivacyEnvelope::parse(&bytes)?;
    let cache = repo.common_dir()?.join("agit/privacy-recovery");
    let destination = cache.join(&resolved.sha);
    ensure!(
        !destination.exists(),
        "this snapshot already has local recovery data"
    );

    ensure!(
        envelope.private_payload.wrapped_keys.len() == 1,
        "repository unlock requires one viewing recipient"
    );
    let recipient = &envelope.private_payload.wrapped_keys[0].recipient;
    let (client, repository, identity) = reader_context(&repo)?;
    let record = client.publication_unlock_key(&repository, &identity, &resolved.sha, recipient)?;
    let public_session = envelope
        .public_projection
        .pointer("/metadata/session")
        .and_then(serde_json::Value::as_str)
        .context("publication envelope has no public session identity")?;
    ensure!(
        record.session_id == public_session,
        "repository key response does not match the selected session"
    );
    let scope = Scope {
        hub: identity.hub.clone(),
        agent_id: identity.agent_id.clone(),
        recipient: recipient.clone(),
        viewing_public_key: record.key.key.public_key.clone(),
    };
    scope.validate()?;
    let key = if use_saved_key {
        privacy_credentials::recall(store, &scope, Utc::now())?
    } else {
        record.key.unlock(&password()?)?
    };
    let secret = crypto_box::SecretKey::from_slice(key.as_ref())
        .map_err(|_| anyhow::anyhow!("invalid repository viewing private key"))?;
    let layer = envelope.open_layer(&secret)?;
    drop(secret);
    config::create_state_dir(&cache)?;
    let staging = layer.restore(&cache, &workspace)?;
    layer.import_protection(&repo)?;
    std::fs::write(
        staging.path().join("publication-digest"),
        digest_bytes(&bytes),
    )?;
    crate::domain::privacy_recovery::write_manifest(staging.path(), &bytes)?;
    ensure!(
        !destination.exists(),
        "recovery data appeared while authorization was pending"
    );
    if let Some(hours) = remember_for {
        privacy_credentials::remember(store, &scope, &key, hours, Utc::now())?;
    }
    drop(key);
    std::fs::rename(staging.path(), &destination)
        .context("cannot install private recovery data")?;
    if crate::commands::json::requested() {
        println!(
            "{}",
            serde_json::json!({"operation": "privacy_unlock", "repository": slug, "snapshot": resolved.sha, "recovery_directory": destination, "workspace": workspace, "key_source": if use_saved_key { "os" } else { "password" }, "remembered_hours": remember_for})
        );
    } else {
        ui::success("private session recovered locally");
        println!("recovery directory: {}", destination.display());
        if let Some(hours) = remember_for {
            println!("viewing key saved in the OS credential store for {hours} hours");
        }
    }
    Ok(ExitCode::Ok)
}

#[cfg(test)]
mod tests;

fn reader_context(
    repo: &crate::domain::repo::Repo,
) -> anyhow::Result<(Client, String, crate::hub::identity::RemoteIdentity)> {
    let identity = crate::hub::identity::read(repo)?.context("this checkout has no immutable source identity; clone the repository before password unlock")?;
    let origin = repo.remote_url().context("this checkout has no source remote; restore its verified repository URL before password unlock")?;
    let (hub, repository) = super::sources::remote_scope(&origin)?;
    ensure!(
        hub == identity.hub,
        "the source remote belongs to another Hub; refusing repository key lookup"
    );
    let client = Client::for_stored_hub(&identity.hub);
    Ok((client, repository, identity))
}

pub(super) fn forget(repository: &str) -> CmdResult {
    let (repo, repository) = super::resolve_repo(Some(repository))?;
    let identity = crate::hub::identity::read(&repo)?
        .context("this checkout has no immutable repository identity for its saved key")?;
    privacy_credentials::forget(&OsUnlockStore, &identity.hub, &identity.agent_id)?;
    if crate::commands::json::requested() {
        println!(
            "{}",
            serde_json::json!({"operation":"privacy_forget_unlock", "hub":identity.hub, "agent_id":identity.agent_id, "repository":repository})
        );
    } else {
        ui::success("saved unlock key removed; existing local recovery data is retained");
    }
    Ok(ExitCode::Ok)
}
