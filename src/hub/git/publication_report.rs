//! Publication receipts retain observations without asserting a later remote state.

use super::{Outcome, ProcessOutput};
use crate::domain::repo::publication::FrozenRef;
use std::collections::{BTreeMap, BTreeSet};

/// Acceptance applies to complete deterministic findings for one publication operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretFindingsAcceptance {
    Reject,
    Accept,
}

#[derive(Debug)]
pub struct PublicationReport {
    pub lfs: Option<LfsPublicationPhase>,
    pub heads: Option<PublicationPhase>,
    pub tags: Option<PublicationPhase>,
    pub error: Option<String>,
}

impl PublicationReport {
    pub fn ok(&self) -> bool {
        self.error.is_none()
            && self.lfs.as_ref().is_some_and(LfsPublicationPhase::ok)
            && self.heads.as_ref().is_some_and(PublicationPhase::ok)
            && self.tags.as_ref().is_some_and(PublicationPhase::ok)
    }

    pub(super) fn refused(error: String) -> Self {
        Self {
            lfs: None,
            heads: None,
            tags: None,
            error: Some(error),
        }
    }
}

/// Native completion does not distinguish a new upload from an object already present.
#[derive(Debug)]
pub struct LfsPublicationPhase {
    pub remote_present: Vec<crate::domain::lfs::Pointer>,
    pub attempts: Vec<LfsPublicationAttempt>,
    pub unattempted: Vec<crate::domain::lfs::Pointer>,
    pub error: Option<String>,
    completed: bool,
}

impl LfsPublicationPhase {
    pub fn ok(&self) -> bool {
        self.completed
            && self.error.is_none()
            && self.unattempted.is_empty()
            && self.attempts.last().is_none_or(LfsPublicationAttempt::ok)
    }

    pub(super) fn selected(
        remote_present: Vec<crate::domain::lfs::Pointer>,
        selected: Vec<crate::domain::lfs::Pointer>,
    ) -> Self {
        Self {
            remote_present,
            attempts: Vec::new(),
            unattempted: selected,
            error: None,
            completed: false,
        }
    }

    pub(super) fn finish(&mut self) {
        self.completed = true;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LfsAttemptKind {
    Version,
    Objects { batch: usize },
}

/// Failed or incomplete native batches leave their object effects unconfirmed.
#[derive(Debug)]
pub struct LfsPublicationAttempt {
    pub kind: LfsAttemptKind,
    pub pointers: Vec<crate::domain::lfs::Pointer>,
    pub outcome: Outcome,
    pub stdout: Vec<u8>,
    pub complete: bool,
    pub error: Option<String>,
}

impl LfsPublicationAttempt {
    pub fn ok(&self) -> bool {
        self.complete && self.error.is_none() && self.outcome.ok()
    }

    pub(super) fn observed(
        kind: LfsAttemptKind,
        pointers: &[crate::domain::lfs::Pointer],
        output: ProcessOutput,
    ) -> Self {
        Self {
            kind,
            pointers: pointers.to_vec(),
            outcome: output.outcome,
            stdout: output.stdout,
            complete: output.complete,
            error: output.error,
        }
    }
}

#[derive(Debug, Default)]
pub struct PublicationPhase {
    pub attempts: Vec<PublicationAttempt>,
    pub unattempted: Vec<FrozenRef>,
    pub error: Option<String>,
}

impl PublicationPhase {
    pub fn ok(&self) -> bool {
        self.error.is_none()
            && self.unattempted.is_empty()
            && self.attempts.last().is_none_or(PublicationAttempt::ok)
    }
}

#[derive(Debug)]
pub struct PublicationAttempt {
    pub batch: usize,
    pub outcome: Outcome,
    pub refs: Vec<PublishedRef>,
    pub complete: bool,
    pub error: Option<String>,
}

impl PublicationAttempt {
    pub fn ok(&self) -> bool {
        self.complete
            && self.error.is_none()
            && self.outcome.ok()
            && self
                .refs
                .iter()
                .all(|reference| reference.status != PublicationStatus::Unconfirmed)
    }

    pub(super) fn parse(batch: usize, refs: &[FrozenRef], output: ProcessOutput) -> Self {
        let mut result = Self {
            batch,
            outcome: output.outcome,
            refs: refs
                .iter()
                .map(|reference| PublishedRef {
                    reference: reference.clone(),
                    status: PublicationStatus::Unconfirmed,
                    detail: None,
                })
                .collect(),
            complete: output.complete,
            error: output.error,
        };
        let expected: BTreeMap<_, _> = refs
            .iter()
            .enumerate()
            .map(|(index, reference)| (reference.refspec(), index))
            .collect();
        let mut seen = BTreeSet::new();
        let mut duplicated = BTreeSet::new();
        // Interrupted captures cannot acknowledge an unterminated record.
        for bytes in output.stdout.split_inclusive(|byte| *byte == b'\n') {
            let Some(bytes) = bytes.strip_suffix(b"\n") else {
                result.complete = false;
                continue;
            };
            let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
            let Ok(line) = std::str::from_utf8(bytes) else {
                result.complete = false;
                continue;
            };
            if line == "Done" || line.starts_with("To ") {
                continue;
            }
            let mut fields = line.split('\t');
            let (Some(flag), Some(spec), Some(detail), None) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                result.complete = false;
                continue;
            };
            let Some(&index) = expected.get(spec) else {
                result.complete = false;
                continue;
            };
            if !seen.insert(index) {
                duplicated.insert(index);
                result.complete = false;
            }
            let status = match flag {
                " " | "*" => PublicationStatus::Updated,
                "=" => PublicationStatus::UpToDate,
                // Failure can mean that the server's acknowledgment was lost after publication.
                "!" => PublicationStatus::Unconfirmed,
                _ => {
                    result.complete = false;
                    PublicationStatus::Unconfirmed
                }
            };
            result.refs[index].status = status;
            result.refs[index].detail = Some(detail.to_owned());
        }
        for index in duplicated {
            result.refs[index].status = PublicationStatus::Unconfirmed;
        }
        result.complete &= seen.len() == refs.len();
        result
    }
}

#[derive(Debug)]
pub struct PublishedRef {
    pub reference: FrozenRef,
    pub status: PublicationStatus,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationStatus {
    Updated,
    UpToDate,
    Unconfirmed,
}
