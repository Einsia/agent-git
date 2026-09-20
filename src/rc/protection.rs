//! Resolve the repository that owns a native session's local secret mappings.

use std::path::{Path, PathBuf};

pub(crate) fn native_repository(
    runtime: &str,
    native: &str,
    cwd: &Path,
) -> crate::Result<Option<PathBuf>> {
    use crate::domain::{link, store::Store};
    let Some(store) = Store::open()? else {
        return Ok(None);
    };
    let Some(snapshot) = link::read_archive_link_snapshot(&store, runtime, native)? else {
        return Ok(None);
    };
    let link = snapshot.link;
    let (Some(owner), Some(agent)) = (&link.owner, &link.agent) else {
        return Ok(None);
    };
    let (owner, agent) = crate::commands::parse_slug(&format!("{owner}/{agent}"))?;
    if let Some(recorded) = &link.cwd {
        anyhow::ensure!(
            Path::new(recorded).canonicalize()? == cwd.canonicalize()?,
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
    let redactor = crate::domain::redact::Redactor::try_this_machine()?;
    match native_repository(runtime, native, cwd)? {
        Some(root) => Ok(redactor
            .with_repository(&root)?
            .with_native_context(runtime, native, cwd, &root)),
        None => Ok(redactor),
    }
}
