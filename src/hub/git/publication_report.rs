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

/// What the Hub's refs, read once after a push, say of the refs the push left without the Hub's
/// answer ([`PublicationStatus::Unconfirmed`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unanswered {
    /// The push left no ref without an answer.
    None,
    /// The Hub holds every unanswered ref at the object the push sent: they landed.
    Landed,
    /// Some unanswered ref is where it was before the push, or still absent where the push was
    /// creating it: it did not land. The ones that landed count as landed.
    NotLanded,
    /// This ref holds an object the push neither found nor sent, or is gone although it was
    /// there: someone else changed it, or the Hub hides it from this account now.
    Moved(String),
}

impl PublicationReport {
    /// Whether some ref of the push was left without the Hub's answer.
    pub fn unanswered(&self) -> bool {
        [&self.heads, &self.tags]
            .into_iter()
            .flatten()
            .flat_map(|phase| &phase.attempts)
            .flat_map(|attempt| &attempt.refs)
            .any(|reference| reference.status == PublicationStatus::Unconfirmed)
    }

    /// Judges every unanswered ref by `after`, the Hub's refs read once after the push, against
    /// `before`, the refs the publication was frozen against. A ref the Hub holds at the object
    /// the push sent is [`PublicationStatus::Updated`], and an attempt whose refs all landed
    /// counts as complete: the Hub took it and only its answer was lost. A ref absent from
    /// `after` landed nowhere only when it was absent from `before` too, as a ref the Hub hides
    /// from this account is absent from its listing as well.
    pub fn settle_unanswered(
        &mut self,
        before: &BTreeMap<String, String>,
        after: &BTreeMap<String, String>,
    ) -> Unanswered {
        let (mut any, mut not_landed, mut moved) = (false, false, None);
        for phase in [&mut self.heads, &mut self.tags].into_iter().flatten() {
            let last = phase.attempts.len().checked_sub(1);
            for (index, attempt) in phase.attempts.iter_mut().enumerate() {
                let mut settled_here = false;
                for reference in &mut attempt.refs {
                    if reference.status != PublicationStatus::Unconfirmed {
                        continue;
                    }
                    any = true;
                    let name = reference.reference.name();
                    match (before.get(name), after.get(name)) {
                        (_, Some(now)) if now == reference.reference.oid() => {
                            reference.status = PublicationStatus::Updated;
                            settled_here = true;
                        }
                        (Some(was), Some(now)) if was == now => not_landed = true,
                        (None, None) => not_landed = true,
                        _ => {
                            moved.get_or_insert_with(|| name.to_owned());
                        }
                    }
                }
                let all_landed = attempt.refs.iter().all(|reference| {
                    matches!(
                        reference.status,
                        PublicationStatus::Updated | PublicationStatus::UpToDate
                    )
                });
                if settled_here && all_landed {
                    attempt.complete = true;
                    attempt.error = None;
                    attempt.outcome.code = 0;
                    // A phase's error is its last run's, which this attempt was.
                    if Some(index) == last {
                        phase.error = None;
                    }
                }
            }
        }
        match (any, moved, not_landed) {
            (false, ..) => Unanswered::None,
            (true, Some(name), _) => Unanswered::Moved(name),
            (true, None, true) => Unanswered::NotLanded,
            (true, None, false) => Unanswered::Landed,
        }
    }

    pub fn ok(&self) -> bool {
        self.error.is_none()
            && self.lfs.as_ref().is_some_and(LfsPublicationPhase::ok)
            && self.heads.as_ref().is_some_and(PublicationPhase::ok)
            && self.tags.as_ref().is_some_and(PublicationPhase::ok)
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
    /// Refs the Hub refused because they are withdrawn from this account. They are among
    /// `unattempted`.
    pub withdrawn: Vec<String>,
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
            && self.refs.iter().all(|reference| {
                matches!(
                    reference.status,
                    PublicationStatus::Updated | PublicationStatus::UpToDate
                )
            })
    }

    /// Refs the destination advertised at their frozen object immediately before this batch,
    /// acknowledged as Git would report them had they been sent.
    pub(super) fn already_published(batch: usize, refs: &[FrozenRef]) -> Self {
        Self {
            batch,
            outcome: Outcome {
                code: 0,
                stderr: String::new(),
            },
            refs: refs
                .iter()
                .map(|reference| PublishedRef {
                    reference: reference.clone(),
                    status: PublicationStatus::UpToDate,
                    detail: None,
                })
                .collect(),
            complete: true,
            error: None,
        }
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
                "!" if refused(detail) => PublicationStatus::Rejected,
                // The remote failed to report this ref, or an atomic push failed as a whole: the
                // update may have landed after the acknowledgment was lost.
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

/// Whether a porcelain `!` record's detail says this ref was refused, so the remote ref did not
/// change: Git refused the update before sending it, or the Hub answered `ng` for it (a hidden
/// ref, a withdrawn or erased tombstone, a gate). Git's other `!` records, a remote that failed to
/// report and an atomic push that failed as a whole, say nothing about this ref; a refusal read
/// into one of them would drop an update the Hub may hold.
fn refused(detail: &str) -> bool {
    const LOCAL: [&str; 8] = [
        "[rejected] (non-fast-forward)",
        "[rejected] (already exists)",
        "[rejected] (fetch first)",
        "[rejected] (needs force)",
        "[rejected] (stale info)",
        "[rejected] (remote ref updated since checkout)",
        "[rejected] (new shallow roots not allowed)",
        "[rejected] (remote does not support deleting refs)",
    ];
    // The Hub writes its own reason after `[remote rejected]`.
    detail.starts_with("[remote rejected]") || LOCAL.contains(&detail)
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
    /// The ref was refused, by Git before sending or by the Hub, so the remote ref did not change.
    Rejected,
    Unconfirmed,
}
