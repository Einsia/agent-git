//! Status reads accepted publication state without downloading an original content key.

use super::CmdResult;
use crate::ExitCode;
use crate::domain::{
    privacy_envelope::{PrivacyEnvelope, digest_bytes},
    refs,
    session_privacy::{AcceptedSnapshot, SESSION_ENVELOPE_VERSION, SessionScheme},
};
use crate::hub::privacy::status::DisplayStatus;
use anyhow::{Context, ensure};

pub(super) fn run(target: &str) -> CmdResult {
    let spec =
        crate::commands::target::resolve_local_repo(crate::commands::target::parse_spec(target)?)?;
    let spec = crate::commands::context::substitute_at(spec)?;
    ensure!(
        matches!(spec.tail, refs::Tail::None),
        "privacy status requires a complete snapshot without a turn or path selector"
    );
    let slug = match &spec.repo {
        refs::RepoSel::Slug(owner, name) => format!("{owner}/{name}"),
        _ => crate::commands::context::resolve(&std::env::current_dir()?)?.repo,
    };
    let (repo, _) = super::resolve_repo(Some(&slug))?;
    let resolved = refs::resolve(&repo, &spec)?;
    let (client, repository, identity) = super::unlock::reader_context(&repo)?;
    let (commit, bytes) = crate::domain::privacy_git::publication_envelope(
        &repo,
        resolved.branch.as_deref(),
        &resolved.sha,
        &identity,
    )?;
    let (session, expected) = if let Some(bytes) = bytes {
        let envelope = PrivacyEnvelope::parse(&bytes)?;
        let snapshot = AcceptedSnapshot {
            version: SESSION_ENVELOPE_VERSION,
            binding: envelope
                .session_binding
                .context("snapshot has no session authority")?,
            published_commit: commit.clone(),
            envelope_digest: digest_bytes(&bytes),
        };
        snapshot.validate()?;
        (snapshot.binding.session_id.clone(), Some(snapshot))
    } else {
        let metadata = crate::domain::storage::metadata_local(repo.root(), &resolved.sha)?;
        ensure!(
            metadata
                .session_privacy
                .as_ref()
                .is_none_or(|privacy| privacy.scheme == SessionScheme::Plain),
            "this encrypted session has no accepted publication; publish its saved snapshot first"
        );
        (metadata.session, None)
    };
    let (client, repository, identity) = if let Some(snapshot) = &expected {
        super::unlock::source_context(&repo, &snapshot.binding.repository_id)?
    } else {
        (client, repository, identity)
    };
    let status = client.session_publication_status(
        &repository,
        &identity,
        &session,
        &commit,
        expected.as_ref(),
    )?;
    if crate::commands::json::requested() {
        println!(
            "{}",
            serde_json::json!({"operation":"privacy_status", "repository":repository, "publication":status})
        );
    } else {
        println!("{repository}@{commit}");
        if let Some(policy) = &status.grant_policy {
            println!(
                "Original grant scope: {}; policy revision: {}.",
                policy.grant_scope.as_str(),
                policy.policy_revision
            );
        }
        match status.scheme {
            SessionScheme::Plain => println!("Original saved unencrypted."),
            SessionScheme::SessionOwnerV2 => {
                println!(
                    "Original saved encrypted; sanitized view {}.",
                    if status.display_status == Some(DisplayStatus::Pending) {
                        "pending"
                    } else {
                        "ready"
                    }
                );
                let access = status.capabilities.context("missing viewer capabilities")?;
                println!(
                    "Original access: {}. Grant management: {}.",
                    if access.can_read_original {
                        "available"
                    } else {
                        "unavailable"
                    },
                    if access.can_manage_original_grants {
                        "available"
                    } else {
                        "unavailable"
                    }
                );
                if let Some(reader) = access.reader {
                    println!(
                        "Current member: {}; owner default: {}; committer default: {}; explicit grant: {}; session explicit grant: {}; revoked: {}; session revoked: {}; defaults suppressed: {}; membership invalidated: {}; session membership invalidated: {}.",
                        reader.current_member,
                        reader.owner_default,
                        reader.committer_default,
                        reader.explicit_grant,
                        reader.session_explicit_grant,
                        reader.revoked,
                        reader.session_revoked,
                        reader.default_suppressed,
                        reader.membership_invalidated,
                        reader.session_membership_invalidated
                    );
                }
            }
        }
    }
    Ok(ExitCode::Ok)
}
