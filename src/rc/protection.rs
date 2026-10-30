//! Resolve the repository that owns a native session's local secret mappings.

use anyhow::Context;
use std::path::{Path, PathBuf};

pub(crate) fn native_repository(
    runtime: &str,
    native: &str,
    cwd: &Path,
) -> crate::Result<Option<PathBuf>> {
    linked_repository(runtime, native, cwd, |_| Ok(()))
}

/// The repository whose rules protect a runtime catalog row's labels. A conversation read through
/// a registered source is linked under its source-qualified key, the row's `session_ref`, and that
/// link must bind the row's own source and thread. Only when there is no such link does a link
/// kept under the bare thread id count, as a conversation adopted before it was read through a
/// source has: that can only add its repository's rules, never take the source's away.
pub(crate) fn catalog_repository(
    row: &crate::rc::runtime_context::CatalogRow,
) -> crate::Result<Option<PathBuf>> {
    let qualified = linked_repository(&row.runtime, &row.session_ref, &row.cwd, |link| {
        let binding = link
            .native_binding
            .as_ref()
            .context("source-qualified native link has no source binding")?;
        anyhow::ensure!(
            binding.source.source_id == row.source_id && binding.thread_id == row.native_session_id,
            "source-qualified native link names another source or thread"
        );
        Ok(())
    })?;
    match qualified {
        Some(root) => Ok(Some(root)),
        None => native_repository(&row.runtime, &row.native_session_id, &row.cwd),
    }
}

/// The repository of the link stored under `key`, once `check` accepts that link.
fn linked_repository(
    runtime: &str,
    key: &str,
    cwd: &Path,
    check: impl FnOnce(&crate::domain::link::Link) -> crate::Result<()>,
) -> crate::Result<Option<PathBuf>> {
    use crate::domain::{link, store::Store};
    let Some(store) = Store::open()? else {
        return Ok(None);
    };
    let Some(snapshot) = link::read_archive_link_snapshot(&store, runtime, key)? else {
        return Ok(None);
    };
    let link = snapshot.link;
    check(&link)?;
    let (Some(owner), Some(agent)) = (&link.owner, &link.agent) else {
        return Ok(None);
    };
    let (owner, agent) = crate::commands::parse_slug(&format!("{owner}/{agent}"))?;
    if let Some(recorded) = &link.cwd {
        // A removed project directory still selects its repository's mappings for reading.
        let recorded = super::policy::resolved(Path::new(recorded));
        anyhow::ensure!(
            recorded.is_some() && recorded == super::policy::resolved(cwd),
            "native session protection context has a different working directory"
        );
    }
    let root = crate::infra::config::repo_dir(&owner, &agent)?;
    anyhow::ensure!(
        crate::domain::repo::Repo::open(&root).is_some(),
        "the native session's Agent repository is unavailable"
    );
    Ok(Some(root))
}

pub(crate) fn for_native(
    runtime: &str,
    native: &str,
    cwd: &Path,
) -> crate::Result<crate::domain::redact::Redactor> {
    // Unadopted previews redact matches in place without requiring a publication repository.
    let redactor = crate::domain::redact::Redactor::try_this_machine()?.for_device_control();
    match native_repository(runtime, native, cwd).ok().flatten() {
        Some(root) => Ok(redactor
            .with_repository(&root)?
            .with_native_context(runtime, native, cwd, &root)),
        None => Ok(redactor.with_unbound_native_context(runtime, native)),
    }
}
