//! Native identity selects archive settlement; branch context never selects a historical journal.

use anyhow::{Context, ensure};

use crate::domain::link::{self, Link};
use crate::domain::merge_archive::{
    self, ArchiveJournalGuard, ArchivePhase, ExplorationBinding, MergeArchiveRole, RuntimeLinkKey,
};
use crate::domain::secret_filter::VaultStore;
use crate::domain::{mergetx, repo::Repo, store::Store};
use crate::{ExitCode, Result};

use super::super::merge::archive::{self as core, TailDestination, TailOutcome};

/// A launched runtime and a lease-supervised commit carry their exact native key, not a branch hint.
pub(crate) const NATIVE_ENV: &str = "AGIT_SETTLEMENT_NATIVE";
pub(crate) const ROLE_ENV: &str = "AGIT_SETTLEMENT_ARCHIVE_ROLE";
pub(crate) const RC_PREFIX: &str = "AGIT_ARCHIVE_SETTLEMENT ";
/// Withheld exploration cannot acknowledge a pending commit from before the Archive opened.
pub(crate) const WITHHELD_RESULT: &str = "archive-exploration-withheld";

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RcHandoff {
    pub native: RuntimeLinkKey,
    pub role: MergeArchiveRole,
}

/// A retained handoff is an identity expectation, never a substitute for fresh admission.
pub(crate) fn expected_rc_handoff() -> Result<Option<RcHandoff>> {
    let Some(role) = std::env::var_os(ROLE_ENV) else {
        return Ok(None);
    };
    checked((|| {
        let role: MergeArchiveRole = serde_json::from_str(
            role.to_str()
                .context("expected archive role is not Unicode")?,
        )?;
        let native: RuntimeLinkKey = serde_json::from_str(
            &std::env::var(NATIVE_ENV).context("expected archive native identity is missing")?,
        )?;
        role.validate(role.origin_head.len())?;
        native.validate()?;
        Ok(Some(RcHandoff { native, role }))
    })())
}

#[derive(Debug)]
struct Failure;

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("archive settlement was not completed")
    }
}

impl std::error::Error for Failure {}

pub(crate) fn is_failure(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Failure>().is_some()
}

pub(crate) fn checked<T>(result: Result<T>) -> Result<T> {
    result.context(Failure)
}

/// A hook payload cannot fall back to process identity when Archive capture is required.
pub(crate) fn require_hook_identity(present: bool) -> Result<()> {
    checked((|| {
        ensure!(
            present
                || (std::env::var_os(ROLE_ENV).is_none()
                    && std::env::var_os(mergetx::GENERATION_ENV).is_none()),
            "archive hook payload has no native session identity"
        );
        Ok(())
    })())
}

/// Archive process authority requires its existing store; recreating it loses native ownership.
pub(crate) fn process_store() -> Result<Option<Store>> {
    if std::env::var_os(ROLE_ENV).is_none() && std::env::var_os(mergetx::GENERATION_ENV).is_none() {
        return Ok(None);
    }
    checked(
        Store::open()
            .and_then(|store| store.context("the launched Archive store is missing or unreadable"))
            .map(Some),
    )
}

/// Retained Archive authority must not become an ordinary quiet no-op after role loss.
pub(super) fn require_selected_role(selected: Option<&Link>) -> Result<()> {
    checked((|| {
        ensure!(
            selected.is_some_and(|link| link.merge_archive.is_some())
                || (std::env::var_os(ROLE_ENV).is_none()
                    && std::env::var_os(mergetx::GENERATION_ENV).is_none()),
            "the selected native Link lost its archive role"
        );
        Ok(())
    })())
}

/// A selected metadata error is not an absent, unadopted session.
pub(crate) fn native_link(store: &Store, key: &RuntimeLinkKey) -> Result<Option<Link>> {
    let snapshot = checked(link::read_archive_link_snapshot(
        store,
        &key.runtime,
        &key.session_id,
    ))?;
    if snapshot.is_none()
        && (std::env::var_os(ROLE_ENV).is_some()
            || std::env::var_os(mergetx::GENERATION_ENV).is_some())
        && std::env::var(NATIVE_ENV)
            .ok()
            .and_then(|value| serde_json::from_str::<RuntimeLinkKey>(&value).ok())
            .as_ref()
            == Some(key)
    {
        return checked(Err(anyhow::anyhow!(
            "the launched archive native Link is missing"
        )));
    }
    Ok(snapshot.map(|snapshot| snapshot.link))
}

pub(super) fn exact_session(
    store: &Store,
    session_id: &str,
    runtime: Option<&str>,
) -> Result<Option<Link>> {
    let mut matches = Vec::new();
    for &candidate in crate::adapter::RUNTIMES {
        if runtime.is_some_and(|runtime| runtime != candidate) {
            continue;
        }
        if let Some(link) = native_link(
            store,
            &RuntimeLinkKey {
                runtime: candidate.to_owned(),
                session_id: session_id.to_owned(),
            },
        )? {
            matches.push(link);
        }
    }
    checked((|| {
        ensure!(
            matches.len() <= 1,
            "native session id belongs to multiple runtimes; supply its runtime explicitly"
        );
        Ok(matches.pop())
    })())
}

pub(super) fn process_link(store: &Store) -> Result<Option<Link>> {
    if let Some(value) = std::env::var_os(NATIVE_ENV) {
        return checked((|| {
            let key: RuntimeLinkKey = serde_json::from_str(
                value
                    .to_str()
                    .context("settlement native key is not Unicode")?,
            )?;
            key.validate()?;
            native_link(store, &key)?
                .context("selected settlement native Link is missing")
                .map(Some)
        })());
    }
    let mut keys = std::collections::BTreeSet::new();
    for &(name, runtime) in crate::infra::runtime_session::ENV_SESSIONS {
        if let Ok(session_id) = std::env::var(name)
            && !session_id.is_empty()
        {
            keys.insert(RuntimeLinkKey {
                runtime: runtime.to_owned(),
                session_id,
            });
        }
    }
    let mut selected = Vec::new();
    for key in keys {
        if let Some(link) = native_link(store, &key)? {
            selected.push(link);
        }
    }
    if selected.iter().any(|link| link.merge_archive.is_some()) {
        return checked((|| {
            ensure!(
                selected.len() == 1,
                "archive settlement requires one exact native identity"
            );
            Ok(selected.pop())
        })());
    }
    Ok(None)
}

pub(super) fn require_options(args: &super::Args, link: &Link) -> Result<()> {
    checked((|| {
        let role = link
            .merge_archive
            .as_ref()
            .context("archive role is missing")?;
        let route = format!("{}@{}", role.slug, role.branch);
        ensure!(
            args.target.as_deref().is_none_or(|target| {
                target == "@"
                    || target == route
                    || target == role.branch
                    || target == link.session_id
            }),
            "explicit settlement target differs from the selected archive runtime"
        );
        ensure!(
            args.name.is_none()
                && args.branch.is_none()
                && args.milestone.is_none()
                && args.tag.is_none()
                && !args.code
                && args.message.is_none()
                && args.paths.is_empty(),
            "archive settlement records native evidence only; commit modifiers cannot reroute or annotate it"
        );
        if args.from_supervisor {
            let expected: MergeArchiveRole = serde_json::from_str(
                &std::env::var(ROLE_ENV)
                    .context("strict archive settlement has no admitted generation handoff")?,
            )?;
            ensure!(
                &expected == role,
                "strict archive settlement generation changed after RC landing"
            );
            ensure!(
                std::env::var("AGIT_SESSION").ok().as_deref() == Some(route.as_str()),
                "strict archive settlement route differs from the supervisor identity"
            );
            ensure!(
                std::env::var_os(NATIVE_ENV).is_some()
                    && std::env::var_os(crate::hub::identity::EXPECTED_AGENT_ID_ENV).is_some()
                    && std::env::var_os(super::SUPERVISOR_RESULT_ENV).is_some(),
                "strict archive settlement requires the supervisor native identity, immutable repository fence and result handoff"
            );
        }
        Ok(())
    })())
}

/// The exact role is retained across the outer selection and the locked admission. Landed tail
/// publication rechecks it under its own locks; no ordinary settlement follows any archive phase.
pub(super) fn settle(store: &Store, selected: &Link) -> Result<Option<ExitCode>> {
    require_selected_role(Some(selected))?;
    let Some(role) = selected.merge_archive.as_ref() else {
        return Ok(None);
    };
    checked((|| {
        let native = RuntimeLinkKey {
            runtime: selected.source.clone(),
            session_id: selected.session_id.clone(),
        };
        ensure!(
            selected.is_archive_for(role, &native.runtime, &native.session_id),
            "selected Link does not own its archive role"
        );
        let (owner, name) = super::super::parse_slug(&role.slug)?;
        let primary = Repo::open(crate::infra::config::repo_dir(&owner, &name)?)
            .context("archive repository checkout is missing")?;
        let repo = core::settlement_destination(&primary, &role.branch)?;
        if std::env::var_os(crate::hub::identity::EXPECTED_AGENT_ID_ENV).is_some() {
            crate::hub::identity::require_current_expected(
                &repo,
                &crate::infra::config::hub_url(),
            )?;
        }
        let landed = admit(&repo, store, selected, role, &native, None)?;
        if landed {
            let global = VaultStore::open_default()?.matcher()?;
            match core::settle_tail(
                TailDestination {
                    repo: &repo,
                    store,
                    role,
                    native: &native,
                },
                &global,
            )? {
                TailOutcome::Noop { .. } => {}
                TailOutcome::Published { commit, .. } => super::record_supervisor_result(&commit)?,
            }
        } else if let Some(path) = std::env::var_os(super::SUPERVISOR_RESULT_ENV) {
            std::fs::write(path, format!("{WITHHELD_RESULT}\n"))?;
        }
        Ok(Some(ExitCode::Ok))
    })())
}

/// Child completion uses retained launch authority and cannot fall back to ordinary recording.
pub(crate) fn finish_child(repo: &Repo, store: &Store, binding: &ExplorationBinding) -> Result<()> {
    ensure!(
        super::delegated_settlement(false)?.is_none(),
        "archive final capture is delegated to the supervisor"
    );
    ensure!(
        super::super::config::get("commit.auto").as_deref() != Some("false"),
        "automatic settlement is disabled by `commit.auto = false`"
    );
    checked((|| {
        let selected = native_link(store, &binding.native)?
            .context("archive final capture Link is missing")?;
        ensure!(
            selected.is_archive_for(
                &binding.role,
                &binding.native.runtime,
                &binding.native.session_id
            ),
            "archive final capture Link lost its launched role"
        );
        ensure!(
            super::owner_for_recording(true)?.is_some(),
            "archive settlement requires sign-in"
        );
        if std::env::var_os(crate::hub::identity::EXPECTED_AGENT_ID_ENV).is_some() {
            crate::hub::identity::require_current_expected(repo, &crate::infra::config::hub_url())?;
        }
        ensure!(
            admit(
                repo,
                store,
                &selected,
                &binding.role,
                &binding.native,
                Some(binding)
            )?,
            "archive merge child exited before the merge landed; the transaction remains open"
        );
        let global = VaultStore::open_default()?.matcher()?;
        core::settle_final_tail(
            TailDestination {
                repo,
                store,
                role: &binding.role,
                native: &binding.native,
            },
            binding,
            &global,
        )?;
        Ok(())
    })())
}

// Admission shares the publication lock order and reads no native records. Its boolean is a
// disposition, not authority to write: the tail core reacquires locks and proves publication.
fn admit(
    repo: &Repo,
    store: &Store,
    selected: &Link,
    role: &MergeArchiveRole,
    native: &RuntimeLinkKey,
    expected: Option<&ExplorationBinding>,
) -> Result<bool> {
    let observed = merge_archive::read_preparation_intent(repo.root(), &role.generation)?
        .context("archive settlement journal is missing")?;
    ensure!(
        observed.binding.role == *role && observed.binding.native == *native,
        "archive settlement journal differs from its selected role"
    );
    let _branches = core::lock_binding_branches(store, &observed.binding)?;
    let _link = link::lock(store, &native.runtime, &native.session_id)?;
    let journal = ArchiveJournalGuard::acquire(repo.root(), &role.generation)?;
    let control = mergetx::ControlGuard::acquire(repo.root())?;
    journal.recover_pending()?;
    let journal = journal
        .read()?
        .context("archive settlement journal is missing")?;
    ensure!(
        journal.binding == observed.binding,
        "archive settlement journal differs from its selected role"
    );
    ensure!(
        expected.is_none_or(|binding| journal.binding == *binding),
        "archive settlement differs from its launched binding"
    );
    let marker = std::env::var_os(mergetx::ENV);
    let generation = std::env::var_os(mergetx::GENERATION_ENV);
    if marker.is_some() || generation.is_some() {
        ensure!(
            marker.as_ref().and_then(|value| value.to_str())
                == Some(format!("{}@{}", role.slug, journal.binding.target_branch()).as_str())
                && generation.as_ref().and_then(|value| value.to_str())
                    == Some(role.generation.as_str()),
            "archive settlement generation differs from the launched runtime"
        );
    }
    let current = native_link(store, native)?.context("archive settlement Link disappeared")?;
    ensure!(
        current.to_json()? == selected.to_json()?,
        "archive Link changed after native selection"
    );
    ensure!(
        current.is_archive_for(role, &native.runtime, &native.session_id)
            && current.baseline_bytes == Some(journal.binding.installed.bytes)
            && current.baseline_hash.as_ref() == Some(&journal.binding.installed.sha256)
            && current.materialized_from.as_ref() == Some(&role.origin_head),
        "archive settlement Link identity or installation changed"
    );
    match journal.phase {
        ArchivePhase::Preparing | ArchivePhase::Open => {
            let image = control
                .read_activation_snapshot()?
                .context("archive exploration transaction is missing")?;
            merge_archive::checked_abort_transaction(
                &image.json,
                &journal.binding,
                journal.phase == ArchivePhase::Preparing,
            )?;
            ensure!(
                repo.git(&[
                    "rev-parse",
                    "--verify",
                    &format!("refs/heads/{}", role.branch)
                ])?
                .trim()
                    == role.origin_head,
                "archive exploration target moved"
            );
            if let Some(target) = &journal.binding.file_target {
                core::file_agent::require_seed_binding(repo, &journal.binding)?;
                ensure!(
                    repo.git(&[
                        "rev-parse",
                        "--verify",
                        &format!("refs/heads/{}", target.branch)
                    ])? == target.head,
                    "file merge target moved during exploration"
                );
            }
            Ok(false)
        }
        ArchivePhase::Landed { .. } => {
            ensure!(
                !control
                    .read()?
                    .is_some_and(|tx| tx.target == role.branch
                        || tx.target == journal.binding.target_branch()),
                "archive target has an active merge transaction"
            );
            Ok(true)
        }
        ArchivePhase::Aborting | ArchivePhase::Aborted | ArchivePhase::Detached => {
            anyhow::bail!("archive runtime is stopped; it cannot settle ordinary turns")
        }
    }
}

pub(crate) fn rc_handoff(
    store: &Store,
    primary: &Repo,
    selected: &Link,
    slug: &str,
    branch: &str,
    cwd: &std::path::Path,
) -> Result<RcHandoff> {
    checked((|| {
        let role = selected
            .merge_archive
            .as_ref()
            .context("RC archive role is missing")?;
        let native = RuntimeLinkKey {
            runtime: selected.source.clone(),
            session_id: selected.session_id.clone(),
        };
        ensure!(
            selected.is_archive_for(role, &native.runtime, &native.session_id)
                && role.slug == slug
                && role.branch == branch
                && selected
                    .cwd
                    .as_deref()
                    .map(std::path::Path::new)
                    .map(std::path::Path::canonicalize)
                    .transpose()?
                    == Some(cwd.canonicalize()?),
            "RC archive landing differs from the exact runtime identity"
        );
        let repo = core::settlement_destination(primary, branch)?;
        admit(&repo, store, selected, role, &native, None)?;
        Ok(RcHandoff {
            native,
            role: role.clone(),
        })
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_archive_failure_survives_context_but_matching_prose_does_not() {
        let error = checked::<()>(Err(anyhow::anyhow!("native capture failed")))
            .context("installed hook")
            .unwrap_err();
        assert!(is_failure(&error));
        assert!(!is_failure(&anyhow::anyhow!(
            "archive settlement was not completed"
        )));
    }
}
