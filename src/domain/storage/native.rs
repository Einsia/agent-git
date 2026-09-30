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
        self.materialize_bounded_profiled(ids, maximum, &mut |_, _| {})
    }

    pub(super) fn materialize_bounded_profiled(
        &self,
        ids: &[String],
        maximum: usize,
        record_timing: &mut dyn FnMut(&'static str, f64),
    ) -> Result<String> {
        let objects = RefCell::new(Vec::new());
        let timings = RefCell::new(record_timing);
        let started = std::time::Instant::now();
        let result = materialize_ids_with_limits(
            ids,
            MAX_EVENT_BYTES.min(maximum),
            maximum,
            |unique| {
                let started = std::time::Instant::now();
                let mut sizes = Vec::with_capacity(unique.len());
                let mut objects = objects.borrow_mut();
                for id in unique {
                    let (object, size) = self.blob_header(&meta::event_path(id)?)?;
                    objects.push(object);
                    sizes.push(size);
                }
                timings.borrow_mut()(
                    "identity_seed_native_headers_ms",
                    started.elapsed().as_secs_f64() * 1000.0,
                );
                Ok(sizes)
            },
            |_, sizes, offsets, output| {
                let started = std::time::Instant::now();
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
                timings.borrow_mut()(
                    "identity_seed_native_read_ms",
                    started.elapsed().as_secs_f64() * 1000.0,
                );
                Ok(())
            },
        );
        timings.borrow_mut()(
            "identity_seed_native_materialize_ms",
            started.elapsed().as_secs_f64() * 1000.0,
        );
        result
    }

    /// The visitor's effects are provisional until every event validates successfully.
    pub(super) fn visit_envelopes_bounded(
        &self,
        ids: &[String],
        maximum: usize,
        mut visit: impl FnMut(&super::Envelope) -> Result<()>,
        record_timing: &mut dyn FnMut(&'static str, f64),
    ) -> Result<usize> {
        let started = std::time::Instant::now();
        let (unique, indexes) = super::index_unique_ids(ids)?;
        let mut objects = Vec::with_capacity(unique.len());
        let mut sizes = Vec::with_capacity(unique.len());
        for id in &unique {
            let (object, size) = self.blob_header(&meta::event_path(id)?)?;
            objects.push(object);
            sizes.push(size);
        }
        super::validate_event_sizes(&unique, &sizes, maximum.min(MAX_EVENT_BYTES))?;
        let bytes = super::expanded_sequence_size(ids, &indexes, &sizes, maximum)?;
        record_timing(
            "identity_seed_native_headers_ms",
            started.elapsed().as_secs_f64() * 1000.0,
        );
        let started = std::time::Instant::now();
        for id in ids {
            let index = indexes[id.as_str()];
            let blob = self.objects.find_blob(objects[index])?;
            anyhow::ensure!(
                blob.data.len() == sizes[index],
                "snapshot event size mismatch"
            );
            let line = std::str::from_utf8(&blob.data).context("identity event is not UTF-8")?;
            let envelope = super::parse_envelope_line(line)?;
            use sha2::{Digest, Sha256};
            let actual = hex::encode(Sha256::digest(line.as_bytes()));
            anyhow::ensure!(
                &actual[..meta::EVENT_ID_HEX_LEN] == id,
                "identity event id mismatch"
            );
            visit(&envelope)?;
        }
        record_timing(
            "identity_seed_native_visit_ms",
            started.elapsed().as_secs_f64() * 1000.0,
        );
        Ok(bytes)
    }
}
