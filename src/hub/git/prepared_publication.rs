//! Preparation owns the exact bytes made available for later inspection.
//! It grants no scan, consent or publication authority.

#[path = "publication_inspection.rs"]
mod inspection;
pub use inspection::{
    BlockedContentInspection, BlockedInspection, CompleteContentInspection, CompleteInspection,
    ContentInspection, InspectionReport, PublicationInspection,
};

use super::FrozenPublication;
use crate::domain::lfs::Pointer;
use crate::domain::repo::publication::PublicationPlan;
use crate::hub::git::StagedLfsPayloads;
use crate::hub::identity::RemoteIdentity;
use anyhow::{Context, Result, ensure};
use std::io::Read;

/// Remote availability is an observation, not a reservation or a content-review verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparedPayloadAvailability {
    Staged,
    RemotePresent,
}

/// Owns a captured publication and its locally prepared LFS payload copies for inspection.
/// Inspection ownership alone does not authorize publication.
pub struct PreparedPublication {
    pub(super) staged: Option<StagedLfsPayloads>,
    pub(super) publication: FrozenPublication,
    pub(super) upload_missing: Vec<Pointer>,
}

impl FrozenPublication {
    /// Query the captured endpoint and stage only its validated missing-pointer subset.
    /// This performs availability requests and private writes, never payload uploads or ref pushes.
    /// The caller's budget bounds staged bytes, not an inferred model or review scope.
    pub fn prepare_payloads(mut self, byte_budget: u64) -> Result<PreparedPublication> {
        let missing = self.missing_lfs_payloads()?;
        let staged = if missing.is_empty() {
            None
        } else {
            let source = self.source_lfs_objects()?;
            // Validate the chosen parent before creating a child, including empty-object cases.
            super::super::frozen_lfs_stage::source_outside_directory(
                source,
                self.directory.path(),
            )?;
            let directory = tempfile::Builder::new()
                .prefix("lfs-stage-")
                .tempdir_in(self.directory.path())
                .map_err(|_| anyhow::anyhow!("cannot create private LFS staging directory"))?;
            let staged = StagedLfsPayloads::stage(source, &missing, byte_budget, directory)
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            ensure!(
                staged.pointers().len() == missing.len(),
                "prepared LFS inventory differs from availability"
            );
            Some(staged)
        };
        Ok(PreparedPublication {
            publication: self,
            staged,
            upload_missing: missing,
        })
    }

    pub(super) fn missing_lfs_payloads(&mut self) -> Result<Vec<Pointer>> {
        let mut missing = if self.lfs_inventory.is_empty() {
            Vec::new()
        } else {
            let (endpoint, agent) = self
                .http
                .as_ref()
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            self.transport.lfs = Some((self.url.clone(), endpoint.clone()));
            super::super::missing_lfs_uploads_with_transport(
                &self.lfs_inventory,
                &self.transport,
                agent,
            )
            .map_err(|_| anyhow::anyhow!("cannot prepare LFS availability"))?
        };
        missing.sort_by(|left, right| left.oid.cmp(&right.oid));
        Ok(missing)
    }
}

impl PreparedPublication {
    pub fn url(&self) -> &str {
        self.publication.url()
    }

    pub fn identity(&self) -> &RemoteIdentity {
        self.publication.identity()
    }

    pub fn plan(&self) -> &PublicationPlan {
        &self.publication.plan
    }

    /// The complete captured inventory includes objects already present remotely.
    pub fn pointers(&self) -> &[Pointer] {
        &self.publication.lfs_inventory
    }

    /// Upload selection is independent of the payload bytes retained for inspection.
    pub fn missing_uploads(&self) -> &[Pointer] {
        &self.upload_missing
    }

    /// A caller cannot supply another identity or size to acquire an inspection reader.
    pub fn availability(&self, pointer: &Pointer) -> Result<PreparedPayloadAvailability> {
        let index = self
            .pointers()
            .binary_search_by(|selected| selected.oid.cmp(&pointer.oid))
            .map_err(|_| anyhow::anyhow!("pointer is outside the prepared publication"))?;
        ensure!(
            self.pointers()[index] == *pointer,
            "pointer differs from the prepared publication"
        );
        let staged = self.staged.as_ref().is_some_and(|staged| {
            staged
                .pointers()
                .binary_search_by(|selected| selected.oid.cmp(&pointer.oid))
                .is_ok()
        });
        Ok(if staged {
            PreparedPayloadAvailability::Staged
        } else {
            PreparedPayloadAvailability::RemotePresent
        })
    }

    /// Remote-present payloads have no implied local bytes or model-clean status.
    /// Inspection reads this owned copy and never falls back to the original cache.
    /// Readers stop beyond the declared size so integrity checks can detect trailing data.
    pub fn open_payload(&self, pointer: &Pointer) -> Result<impl Read + '_> {
        ensure!(
            self.availability(pointer)? == PreparedPayloadAvailability::Staged,
            "prepared payload bytes are not available locally"
        );
        let reader = self
            .staged
            .as_ref()
            .context("prepared payload bytes are unavailable")?
            .open_payload(pointer)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        Ok(reader.take(pointer.size + 1))
    }
}
