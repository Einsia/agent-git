//! Selects one retained archive generation before entering its lifecycle lock order.

use anyhow::{Context, ensure};

use crate::Result;
use crate::domain::merge_archive::{ArchivePhase, ExplorationBinding};
use crate::domain::secret_filter::VaultStore;
use crate::domain::{
    archive_history, merge_archive, mergetx,
    repo::{self, Repo},
    store::Store,
};

use super::{abort, landing};

pub(in crate::commands::merge) fn select(
    repo: &Repo,
    slug: &str,
    branch: Option<&str>,
    transaction: Option<&mergetx::Tx>,
) -> Result<Option<ExplorationBinding>> {
    let generation = if let Some(tx) = transaction {
        tx.require_agent_context(slug)?;
        ensure!(
            branch.is_none_or(|branch| tx.target == branch),
            "the selected branch differs from the open merge transaction"
        );
        if tx.exploration.is_none() {
            let Some(generation) = tx.generation.as_deref() else {
                return Ok(None);
            };
            if !merge_archive::has_retained_intent(&repo::common_git_dir(repo.root()), generation)?
            {
                return Ok(None);
            }
        }
        tx.generation
            .clone()
            .context("archive transaction has no generation")?
    } else {
        let read = |key| match std::env::var(key) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => anyhow::bail!("{key} is not valid Unicode"),
        };
        let marker = read(mergetx::ENV)?;
        let generation = read(mergetx::GENERATION_ENV)?;
        if marker.is_none() && generation.is_none() {
            return Ok(None);
        }
        let branch = branch.context("archive completion requires an explicit branch")?;
        ensure!(
            marker.as_deref() == Some(format!("{slug}@{branch}").as_str()),
            "archive completion requires the exact AGIT_MERGE_TX route"
        );
        generation.context("archive completion requires AGIT_MERGE_GENERATION; historical journals are never selected implicitly")?
    };
    let journal = merge_archive::read_preparation_intent(repo.root(), &generation)?
        .context("the selected archive generation has no retained journal")?;
    let binding = journal.binding;
    ensure!(
        binding.role.slug == slug && branch.is_some_and(|branch| branch == binding.role.branch),
        "archive generation belongs to a different repository or branch"
    );
    if let Some(tx) = transaction {
        let checked = merge_archive::checked_abort_transaction(
            &serde_json::to_string(tx)?,
            &binding,
            journal.activation.is_some()
                || journal
                    .abort
                    .as_ref()
                    .is_some_and(|abort| abort.activation.is_some()),
        )?;
        checked.require_agent_context(slug)?;
    } else {
        ensure!(
            matches!(
                journal.phase,
                ArchivePhase::Landed { .. } | ArchivePhase::Aborted
            ),
            "a missing transaction is not proof of archive completion"
        );
        let retained = match journal.phase {
            ArchivePhase::Landed { .. } => journal
                .landing
                .as_ref()
                .map(|landing| &landing.transaction_json),
            ArchivePhase::Aborted => journal.abort.as_ref().map(|abort| &abort.transaction_json),
            _ => unreachable!(),
        }
        .context("archive completion has no retained transaction")?;
        mergetx::checked_activation_image(retained)?.require_agent_context(slug)?;
    }
    Ok(Some(binding))
}

/// Only the selected branch's registered holder supplies shared-file edits and checkout recovery.
/// Discovery does not prune, move or create worktrees before lifecycle admission.
pub(in crate::commands::merge) fn destination(primary: &Repo, branch: &str) -> Result<Repo> {
    for key in archive_history::GIT_ROUTING_ENV {
        ensure!(
            std::env::var_os(key).is_none(),
            "archive selection refuses inherited Git routing override {key}"
        );
    }
    let listing = primary.git_bytes_result(&["worktree", "list", "--porcelain", "-z"])?;
    let holders = super::branch_holders(&listing, branch)?;
    ensure!(
        holders.len() <= 1,
        "archive target has multiple checkout holders"
    );
    let selected = match holders.into_iter().next() {
        Some(path) => Repo::at(path.canonicalize()?),
        None => primary.clone(),
    };
    ensure!(
        repo::common_git_dir(selected.root()).canonicalize()?
            == repo::common_git_dir(primary.root()).canonicalize()?,
        "archive checkout belongs to a different repository"
    );
    super::require_destination_routing(&selected, branch)?;
    Ok(selected)
}

pub(in crate::commands::merge) fn continue_selected(
    repo: &Repo,
    store: &Store,
    binding: &ExplorationBinding,
) -> Result<landing::LandingOutcome> {
    if let Some(outcome) = landing::replay(repo, store, binding)? {
        return Ok(outcome);
    }
    let (owner, name) = super::super::super::parse_slug(&binding.source.slug)?;
    let source = Repo::open(crate::infra::config::repo_dir(&owner, &name)?)
        .context("the frozen merge source repository is unavailable")?;
    let global = VaultStore::open_default()?.matcher()?;
    landing::land(
        landing::LandingRequest {
            repo,
            source_repo: &source,
            store,
            binding,
        },
        &global,
    )
}

pub(in crate::commands::merge) fn abort_selected(
    repo: &Repo,
    store: &Store,
    binding: &ExplorationBinding,
) -> Result<abort::AbortOutcome> {
    abort::abort(abort::AbortRequest {
        repo,
        store,
        binding,
    })
}
