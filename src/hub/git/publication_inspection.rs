//! Completed inspection owns the content it describes; copied reports carry no authority.

#[path = "publication_execution.rs"]
mod execution;

use super::{PreparedPayloadAvailability, PreparedPublication};
use crate::domain::lfs::Pointer;
use crate::domain::repo::{Repo, publication::PublicationPlan};
use crate::domain::secrets::publication::{CapturedPolicy, InspectionFailure, Inspector};
use crate::domain::secrets::{ScanLimits, ScanReport, Unscanned};
use crate::hub::git::frozen::CapturedPublication;
use crate::hub::git::inspection_summary::InspectionSummary;
use crate::hub::identity::RemoteIdentity;
use std::io::Read;

pub struct InspectionReport {
    scan: ScanReport,
    binary_git_objects: u64,
    binary_lfs: Vec<Pointer>,
    remote_present: Vec<Pointer>,
}

impl InspectionReport {
    pub fn scan(&self) -> &ScanReport {
        &self.scan
    }
    pub fn binary_git_objects(&self) -> u64 {
        self.binary_git_objects
    }
    pub fn binary_lfs(&self) -> &[Pointer] {
        &self.binary_lfs
    }
    /// Availability does not imply that these remote payload bytes were inspected.
    pub fn remote_present(&self) -> &[Pointer] {
        &self.remote_present
    }
}

/// Only a complete deterministic pass can construct this owner.
/// Complete findings are retained; this type is neither consent nor model approval.
pub struct CompleteInspection {
    prepared: PreparedPublication,
    report: InspectionReport,
}

pub struct BlockedInspection {
    prepared: PreparedPublication,
    report: InspectionReport,
    reason: InspectionFailure,
}

pub enum PublicationInspection {
    Complete(CompleteInspection),
    Blocked(BlockedInspection),
}

impl PublicationInspection {
    pub fn summary(&self) -> InspectionSummary<'_> {
        match self {
            Self::Complete(complete) => complete.summary(),
            Self::Blocked(blocked) => blocked.summary(),
        }
    }
}

impl CompleteInspection {
    pub fn summary(&self) -> InspectionSummary<'_> {
        InspectionSummary::from_inspection(&self.prepared, &self.report, None)
    }

    pub fn prepared(&self) -> &PreparedPublication {
        &self.prepared
    }
    pub fn report(&self) -> &InspectionReport {
        &self.report
    }
    pub fn has_findings(&self) -> bool {
        !self.report.scan.hits.is_empty()
    }
}

impl BlockedInspection {
    pub fn summary(&self) -> InspectionSummary<'_> {
        InspectionSummary::from_inspection(&self.prepared, &self.report, Some(self.reason))
    }

    pub fn prepared(&self) -> &PreparedPublication {
        &self.prepared
    }
    pub fn report(&self) -> &InspectionReport {
        &self.report
    }
    pub fn reason(&self) -> InspectionFailure {
        self.reason
    }
}

impl PreparedPublication {
    #[cfg(test)]
    pub(crate) fn damage_staged_payload_for_test(&self, pointer: &Pointer, bytes: &[u8]) {
        let path = self
            .staged
            .as_ref()
            .unwrap()
            .storage()
            .join("objects")
            .join(&pointer.oid[..2])
            .join(&pointer.oid[2..4])
            .join(&pointer.oid);
        std::fs::write(path, bytes).unwrap();
    }

    /// Inspect captured Git objects and owned LFS payloads without new network requests.
    /// Ambient acceptance flags cannot complete an unread or truncated carrier.
    pub fn inspect(self, limits: ScanLimits) -> PublicationInspection {
        let repo = Repo::at(self.publication.directory.path()).exact_bare_root_inspection();
        let (report, reason) = inspect_content(
            &self.publication.inspection_policy,
            &repo,
            self.plan(),
            self.pointers(),
            limits,
            |pointer| match self.availability(pointer)? {
                PreparedPayloadAvailability::RemotePresent => Ok(None),
                PreparedPayloadAvailability::Staged => Ok(Some(
                    Box::new(self.open_payload(pointer)?) as Box<dyn Read + '_>,
                )),
            },
        );
        match reason {
            None => PublicationInspection::Complete(CompleteInspection {
                prepared: self,
                report,
            }),
            Some(reason) => PublicationInspection::Blocked(BlockedInspection {
                prepared: self,
                report,
                reason,
            }),
        }
    }
}

/// Completed local inspection carries no assertion about an as-yet uncreated destination.
/// The caller separately obtains model review and human publication consent.
pub struct CompleteContentInspection {
    captured: CapturedPublication,
    report: InspectionReport,
}

pub struct BlockedContentInspection {
    captured: CapturedPublication,
    report: InspectionReport,
    reason: InspectionFailure,
}

pub enum ContentInspection {
    Complete(CompleteContentInspection),
    Blocked(BlockedContentInspection),
}

impl CapturedPublication {
    /// Every captured payload participates even when a destination already holds its object ID.
    pub fn inspect(self, limits: ScanLimits) -> ContentInspection {
        let repo = Repo::at(self.snapshot_git_dir()).exact_bare_root_inspection();
        let (report, reason) = inspect_content(
            &self.git.inspection_policy,
            &repo,
            self.plan(),
            self.pointers(),
            limits,
            |pointer| {
                Ok(Some(
                    Box::new(self.open_payload(pointer)?) as Box<dyn Read + '_>
                ))
            },
        );
        match reason {
            None => ContentInspection::Complete(CompleteContentInspection {
                captured: self,
                report,
            }),
            Some(reason) => ContentInspection::Blocked(BlockedContentInspection {
                captured: self,
                report,
                reason,
            }),
        }
    }
}

impl CompleteContentInspection {
    pub fn captured(&self) -> &CapturedPublication {
        &self.captured
    }

    pub fn report(&self) -> &InspectionReport {
        &self.report
    }

    pub fn has_findings(&self) -> bool {
        !self.report.scan.hits.is_empty()
    }

    /// Call before creating or promoting a repository; binding repeats this observation.
    pub fn verify_source(&self, repo: &Repo) -> anyhow::Result<()> {
        self.captured.verify_source(repo)
    }

    /// Bind only after review and publication consent, using the validated actual destination.
    /// Binding retains the same content owners and does not restage mutable cache payloads.
    pub fn bind_destination(
        self,
        repo: &Repo,
        canonical_url: &str,
        identity: &RemoteIdentity,
    ) -> anyhow::Result<CompleteInspection> {
        let prepared = self
            .captured
            .bind_destination(repo, canonical_url, identity)?;
        Ok(CompleteInspection {
            prepared,
            report: self.report,
        })
    }

    /// The authenticated caller retains its account across review, destination creation and push.
    /// A current login cannot replace this client's identity during binding or refresh.
    pub fn bind_destination_with_client(
        self,
        repo: &Repo,
        canonical_url: &str,
        identity: &RemoteIdentity,
        client: crate::hub::Client,
    ) -> anyhow::Result<CompleteInspection> {
        let prepared =
            self.captured
                .bind_destination_with_client(repo, canonical_url, identity, client)?;
        Ok(CompleteInspection {
            prepared,
            report: self.report,
        })
    }
}

impl BlockedContentInspection {
    pub fn captured(&self) -> &CapturedPublication {
        &self.captured
    }

    pub fn report(&self) -> &InspectionReport {
        &self.report
    }

    pub fn reason(&self) -> InspectionFailure {
        self.reason
    }
}

fn inspect_content<R: Read>(
    policy: &Result<CapturedPolicy, InspectionFailure>,
    repo: &Repo,
    plan: &PublicationPlan,
    pointers: &[Pointer],
    limits: ScanLimits,
    mut payload: impl FnMut(&Pointer) -> anyhow::Result<Option<R>>,
) -> (InspectionReport, Option<InspectionFailure>) {
    let policy = match policy {
        Ok(policy) => policy,
        Err(reason) => {
            return (
                InspectionReport {
                    scan: ScanReport {
                        binary_carriers: 0,
                        hits: Vec::new(),
                        truncated: false,
                        unscanned: Unscanned::default(),
                    },
                    binary_git_objects: 0,
                    binary_lfs: Vec::new(),
                    remote_present: Vec::new(),
                },
                Some(*reason),
            );
        }
    };
    let mut inspector = Inspector::new(policy, limits);
    let mut binary_lfs = Vec::new();
    let mut remote_present = Vec::new();
    let result = (|| {
        inspector.git(repo, plan)?;
        for pointer in pointers {
            match payload(pointer).map_err(|_| InspectionFailure::Content)? {
                None => remote_present.push(pointer.clone()),
                Some(reader) => {
                    if inspector.lfs(reader, pointer)? {
                        binary_lfs.push(pointer.clone());
                    }
                }
            }
        }
        inspector.require_complete()
    })();
    let (scan, binary_git_objects) = inspector.finish();
    (
        InspectionReport {
            scan,
            binary_git_objects,
            binary_lfs,
            remote_present,
        },
        result.err(),
    )
}
