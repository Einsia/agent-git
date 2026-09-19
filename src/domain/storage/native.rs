//! Immutable local objects share the storage materializer's size, hash and sequence checks.

use super::{
    MAX_EVENT_BYTES, MAX_MATERIALIZED_BYTES, ReadLimitExceeded, materialize_ids_with_limits,
};
use crate::domain::{meta, repo::Repo};
use anyhow::{Context, Result};
use std::{cell::RefCell, path::Path};

pub(super) struct Snapshot {
    objects: gix::Repository,
    tree: gix::ObjectId,
}

impl Snapshot {
    /// Only immutable commits with ordinary repository routing use native reads. Missing objects
    /// and unsupported routing leave transport and diagnostics to the caller's Git read path.
    pub(super) fn open(root: &Path, commit: &str) -> Option<Self> {
        let id = gix::ObjectId::from_hex(commit.as_bytes()).ok()?;
        let mut objects = Repo::at(root).native_commit_repository()?;
        objects.object_cache_size(8 * 1024 * 1024);
        let tree = objects.find_commit(id).ok()?.tree_id().ok()?.detach();
        Some(Self { objects, tree })
    }

    fn blob_header(&self, path: &str) -> Result<(gix::ObjectId, usize)> {
        let entry = self
            .objects
            .find_tree(self.tree)?
            .lookup_entry(path.split('/'))?
            .with_context(|| format!("snapshot has no {path}"))?;
        let id = entry.object_id();
        let header = self.objects.find_header(id)?;
        anyhow::ensure!(
            header.kind() == gix::objs::Kind::Blob,
            "{path} is not a blob"
        );
        let size = usize::try_from(header.size()).context("snapshot object size overflow")?;
        Ok((id, size))
    }

    pub(super) fn blob(&self, path: &str, limit: usize) -> Result<Vec<u8>> {
        let (id, size) = self.blob_header(path)?;
        if size > limit {
            return Err(ReadLimitExceeded(format!("{path} exceeds the {limit}-byte limit")).into());
        }
        let blob = self.objects.find_blob(id)?;
        anyhow::ensure!(blob.data.len() == size, "snapshot object size mismatch");
        Ok(blob.data.clone())
    }

    pub(super) fn materialize(&self, ids: &[String]) -> Result<String> {
        self.materialize_bounded(ids, MAX_MATERIALIZED_BYTES)
    }

    pub(super) fn materialize_bounded(&self, ids: &[String], maximum: usize) -> Result<String> {
        let objects = RefCell::new(Vec::new());
        materialize_ids_with_limits(
            ids,
            MAX_EVENT_BYTES.min(maximum),
            maximum,
            |unique| {
                let mut sizes = Vec::with_capacity(unique.len());
                let mut objects = objects.borrow_mut();
                for id in unique {
                    let (object, size) = self.blob_header(&meta::event_path(id)?)?;
                    objects.push(object);
                    sizes.push(size);
                }
                Ok(sizes)
            },
            |_, sizes, offsets, output| {
                for ((object, size), offset) in objects.borrow().iter().zip(sizes).zip(offsets) {
                    let blob = self.objects.find_blob(*object)?;
                    anyhow::ensure!(blob.data.len() == *size, "snapshot event size mismatch");
                    let end = offset
                        .checked_add(*size)
                        .context("event output range overflow")?;
                    output
                        .get_mut(*offset..end)
                        .context("event output range is out of bounds")?
                        .copy_from_slice(&blob.data);
                }
                Ok(())
            },
        )
    }
}
