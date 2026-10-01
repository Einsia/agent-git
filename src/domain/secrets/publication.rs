//! Compatibility error categories for captured publication integrity.

use super::*;
use crate::domain::repo::{ObjectBody, Repo};
use std::cell::Cell;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InspectionFailure {
    #[error("publication inspection configuration is invalid")]
    Configuration,
    #[error("publication inspection local policy is unavailable")]
    LocalState,
    #[error("publication inspection cannot read its captured content")]
    Content,
    #[error("publication inspection is incomplete")]
    Incomplete,
}
pub(super) fn checked_objects(
    repo: &Repo,
    objects: &[String],
    kind: Option<&str>,
) -> crate::Result<Vec<(String, String, u64)>> {
    let mut checked = Vec::with_capacity(objects.len());
    repo.git_cat_file_batch_check(objects.to_vec(), |oid, actual, size| {
        anyhow::ensure!(
            objects
                .get(checked.len())
                .is_some_and(|expected| expected == oid)
                && kind.map_or_else(
                    || matches!(actual, "blob" | "tree"),
                    |expected| expected == actual
                ),
            "prepared object header is invalid"
        );
        checked.push((oid.to_owned(), actual.to_owned(), size));
        Ok(())
    })?;
    anyhow::ensure!(
        checked.len() == objects.len(),
        "prepared object header response is incomplete"
    );
    Ok(checked)
}

pub(super) fn reserve_objects(
    objects: &[(String, String, u64)],
    limit: u64,
    remaining: &Cell<u64>,
) -> crate::Result<()> {
    let mut needed = 0u64;
    for (_, _, size) in objects {
        if *size <= limit {
            needed = needed.checked_add(*size).ok_or(BudgetSpent)?;
        }
    }
    if needed > remaining.get() {
        return Err(anyhow::Error::new(BudgetSpent));
    }
    remaining.set(remaining.get() - needed);
    Ok(())
}

pub(super) fn check_body(
    expected: &[(String, String, u64)],
    at: usize,
    oid: &str,
    kind: &str,
    body: &ObjectBody<'_>,
) -> crate::Result<()> {
    let size = match body {
        ObjectBody::Read(bytes) => bytes.len() as u64,
        ObjectBody::TooLarge(size) => *size as u64,
    };
    anyhow::ensure!(
        expected
            .get(at)
            .is_some_and(|(want_oid, want_kind, want_size)| want_oid == oid
                && want_kind == kind
                && *want_size == size),
        "prepared object body differs from its header"
    );
    Ok(())
}

pub(super) fn scan_blob_payload(
    context: &BlobScanContext<'_>,
    oid: &str,
    bytes: &[u8],
    labels: &HashMap<String, String>,
    binary: &Cell<u64>,
    out: &mut HitCollector,
) -> crate::Result<()> {
    if matches!(crate::domain::lfs::Pointer::parse(bytes), Ok(Some(_))) {
        return Ok(());
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => {
            let report = scan_repository_payload_capped(
                text,
                context.allowlist,
                context.trusted_identities,
                Policy::CLIENT,
                out.remaining(),
                context.registered,
            );
            if report.truncated {
                out.mark_truncated();
            }
            let file = labels
                .get(oid)
                .map(|path| format!("blob object {}/{path}", &oid[..8]));
            out.extend(report.hits.into_iter().map(|mut hit| {
                hit.file.clone_from(&file);
                hit.source = Source::BlobObject;
                hit
            }));
        }
        Err(_) => binary.set(binary.get().saturating_add(1)),
    }
    Ok(())
}
