//! Standard Git LFS owns filtering and its shared local object cache.

use crate::domain::repo::Repo;
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};

pub fn require_client(repo: &Repo) -> Result<()> {
    let output = repo
        .git(&["lfs", "version"])
        .context("install Git LFS from https://git-lfs.com/ to manage large files")?;
    validate_client_version(&output)
}

pub(crate) fn validate_client_version(output: &str) -> Result<()> {
    let version = output
        .strip_prefix("git-lfs/")
        .and_then(|rest| rest.split_whitespace().next())
        .context("unrecognized Git LFS version")?;
    let parts: Vec<u64> = version
        .split('.')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .context("unrecognized Git LFS version")?;
    ensure!(
        parts.as_slice() >= [3, 7, 1].as_slice(),
        "Git LFS must be updated to at least 3.7.1; see https://git-lfs.com/"
    );
    Ok(())
}

pub fn prepare_tracking(repo: &Repo, paths: &[String]) -> Result<()> {
    require_client(repo)?;
    ensure!(
        paths.iter().all(|path| !path
            .split('/')
            .any(|part| part.eq_ignore_ascii_case(".gitattributes")
                || part.eq_ignore_ascii_case(".lfsconfig"))),
        "Git attributes and LFS configuration cannot themselves use LFS"
    );
    let attribute_path = repo.root().join(".gitattributes");
    let working = match std::fs::symlink_metadata(&attribute_path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "Git attributes must be a regular file"
            );
            Some(std::fs::read(&attribute_path)?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let tracked = !repo
        .git_bytes_result(&["ls-files", "-z", "--", ".gitattributes"])?
        .is_empty();
    let staged = if tracked {
        Some(repo.git_bytes_result(&["show", ":.gitattributes"])?)
    } else {
        None
    };
    ensure!(
        working == staged,
        "stage or restore your .gitattributes edits before adding LFS files"
    );
    repo.git(&["lfs", "install", "--local", "--skip-repo"])?;
    let result = (|| {
        for path in paths {
            repo.git(&["lfs", "track", "--filename", "--", path])?;
            ensure!(
                is_tracked(repo, path)?,
                "nested attributes override LFS tracking for {path}; update them before adding this file"
            );
        }
        Ok(())
    })();
    if result.is_err() {
        match working {
            Some(bytes) => std::fs::write(&attribute_path, bytes)?,
            None => {
                if attribute_path.exists() {
                    std::fs::remove_file(&attribute_path)?;
                }
            }
        }
    }
    result
}

pub fn is_tracked(repo: &Repo, path: &str) -> Result<bool> {
    let output = repo.git_bytes_result(&["check-attr", "-z", "filter", "--", path])?;
    Ok(output.split(|byte| *byte == 0).nth(2) == Some(b"lfs".as_slice()))
}

pub fn object_path(repo: &Repo, pointer: &super::Pointer) -> Result<PathBuf> {
    super::cached_object_path(repo, pointer)
}

pub fn extract_cached(repo: &Repo, pointer: &super::Pointer, output: &Path) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    let path = object_path(repo, pointer)?;
    let mut input = std::fs::File::open(path)
        .context("the LFS object is not cached; fetch it from the repository remote")?;
    pointer.verify(&mut input)?;
    input.seek(SeekFrom::Start(0))?;
    let parent = output.parent().context("the output path has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    std::io::copy(&mut input, temporary.as_file_mut())?;
    temporary.as_file().sync_all()?;
    temporary.persist(output).map_err(|error| error.error)?;
    Ok(())
}

pub use super::history::reachable;

pub fn upload_selected(
    repo: &Repo,
    references: &[String],
    identity: &crate::hub::identity::RemoteIdentity,
    allow_secrets: bool,
) -> Result<()> {
    crate::telemetry::measure(crate::telemetry::Operation::ArtifactUpload, || {
        upload_selected_inner(repo, references, identity, allow_secrets)
    })
}

fn upload_selected_inner(
    repo: &Repo,
    references: &[String],
    identity: &crate::hub::identity::RemoteIdentity,
    allow_secrets: bool,
) -> Result<()> {
    let pointers = reachable(repo, references)?;
    let pointers = crate::hub::git::missing_lfs_uploads(repo, &pointers, identity)?;
    if pointers.is_empty() {
        return Ok(());
    }
    let limits = crate::domain::secrets::ScanLimits::default();
    let mut remaining = limits.budget_bytes;
    let allowlist = crate::domain::secrets::load_allowlist(&crate::infra::config::agit_home()?);
    let registered = crate::domain::secrets::registered_matcher_for_repo(repo)?;
    // Historical payloads absent from the destination leave the machine even during an incremental push.
    for pointer in &pointers {
        let payload =
            super::inspection::cached(repo, pointer, limits.max_object_bytes, &mut remaining)?;
        let (blocked, complete) = match payload {
            super::inspection::Payload::Binary => (false, true),
            super::inspection::Payload::Text(text) => (
                !crate::domain::secrets::scan_text_registered_with(&text, &allowlist, &registered)
                    .is_empty(),
                true,
            ),
            super::inspection::Payload::TooLarge => {
                pointer.verify(std::fs::File::open(object_path(repo, pointer)?)?)?;
                (true, false)
            }
        };
        if blocked {
            ensure!(
                (allow_secrets && complete) || crate::infra::config::allow_secrets(),
                "LFS upload blocked: payload {} contains suspected secrets or cannot be completely scanned",
                pointer.oid
            );
            if allow_secrets && complete {
                crate::ui::warning(
                    "--allow-secrets explicitly accepts credential findings in this verified LFS payload.",
                );
            } else {
                crate::ui::warning(
                    "AGIT_ALLOW_SECRETS is set — uploading an LFS payload that did not pass the secret scan.",
                );
            }
        }
    }
    // Explicit objects keep native Git traversal from changing the inspected upload scope.
    for batch in pointers.chunks(100) {
        let mut args = vec!["push", "--object-id", "origin"];
        args.extend(batch.iter().map(|pointer| pointer.oid.as_str()));
        let outcome = crate::hub::git::run_lfs_for_remote(repo, &args, identity)?;
        ensure!(
            outcome.ok(),
            "LFS upload failed before publishing Git references: {}",
            outcome.stderr.trim()
        );
    }
    Ok(())
}
