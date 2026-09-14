//! A copied inspection summary describes evidence without owning content or authorizing publication.

use super::{
    InspectionFailure, InspectionReport, PreparedPayloadAvailability, PreparedPublication,
};
use crate::domain::lfs::Pointer;
use crate::hub::identity::RemoteIdentity;
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DeterministicInspectionState {
    Complete,
    Blocked { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelReviewState {
    NotPerformed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LfsInspectionCoverage {
    RemotePresentNotInspected,
    BinaryNotScannedAsText,
    TextInspected,
    InspectionNotConfirmed,
    AvailabilityNotConfirmed,
}

#[derive(Debug, Serialize)]
pub struct SelectedRef<'a> {
    pub name: &'a str,
    pub oid: &'a str,
}

#[derive(Debug, Serialize)]
pub struct Finding<'a> {
    pub rule: &'a str,
    pub file: Option<&'a str>,
    pub source: &'static str,
    pub line: usize,
    pub redacted: &'a str,
}

#[derive(Debug, Serialize)]
pub struct BudgetExceeded {
    pub counted_bytes_at_least: u64,
    pub budget_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct UnscannedSummary<'a> {
    pub over_budget: Option<BudgetExceeded>,
    pub oversized_objects: &'a [(String, u64)],
    pub oversized_files: &'a [(String, u64)],
}

#[derive(Debug, Serialize)]
pub struct LfsPayloadSummary<'a> {
    pub pointer: &'a Pointer,
    pub coverage: LfsInspectionCoverage,
}

/// Borrowing or serializing this data never supplies a completed inspection owner.
/// Binary and remote-present payloads have no implied text or model review.
#[derive(Debug, Serialize)]
pub struct InspectionSummary<'a> {
    pub target: &'a str,
    pub identity: &'a RemoteIdentity,
    pub heads: Vec<SelectedRef<'a>>,
    pub tags: Vec<SelectedRef<'a>>,
    pub deterministic: DeterministicInspectionState,
    pub model_review: ModelReviewState,
    pub findings: Vec<Finding<'a>>,
    pub findings_truncated: bool,
    pub unscanned: UnscannedSummary<'a>,
    pub binary_git_objects: u64,
    pub lfs: Vec<LfsPayloadSummary<'a>>,
}

impl<'a> InspectionSummary<'a> {
    pub(super) fn from_inspection(
        prepared: &'a PreparedPublication,
        report: &'a InspectionReport,
        blocked: Option<InspectionFailure>,
    ) -> Self {
        let scan = report.scan();
        let binary: BTreeSet<_> = report
            .binary_lfs()
            .iter()
            .map(|pointer| pointer.oid.as_str())
            .collect();
        let selected = |references: &'a [crate::domain::repo::publication::FrozenRef]| {
            references
                .iter()
                .map(|reference| SelectedRef {
                    name: reference.name(),
                    oid: reference.oid(),
                })
                .collect()
        };
        Self {
            target: prepared.url(),
            identity: prepared.identity(),
            heads: selected(prepared.plan().heads()),
            tags: selected(prepared.plan().tags()),
            deterministic: match blocked {
                Some(reason) => DeterministicInspectionState::Blocked {
                    reason: reason.to_string(),
                },
                None => DeterministicInspectionState::Complete,
            },
            model_review: ModelReviewState::NotPerformed,
            findings: scan
                .hits
                .iter()
                .map(|hit| Finding {
                    rule: &hit.rule,
                    file: hit.file.as_deref(),
                    source: hit.source.as_str(),
                    line: hit.line,
                    redacted: &hit.redacted,
                })
                .collect(),
            findings_truncated: scan.truncated,
            unscanned: UnscannedSummary {
                over_budget: scan
                    .unscanned
                    .over_budget
                    .map(|(counted, budget)| BudgetExceeded {
                        counted_bytes_at_least: counted,
                        budget_bytes: budget,
                    }),
                oversized_objects: &scan.unscanned.oversized,
                oversized_files: &scan.unscanned.oversized_files,
            },
            binary_git_objects: report.binary_git_objects(),
            lfs: prepared
                .pointers()
                .iter()
                .map(|pointer| LfsPayloadSummary {
                    pointer,
                    // A blocked Git pass may precede every LFS visit; availability uses the full plan.
                    coverage: match prepared.availability(pointer) {
                        Ok(PreparedPayloadAvailability::RemotePresent) => {
                            LfsInspectionCoverage::RemotePresentNotInspected
                        }
                        Ok(PreparedPayloadAvailability::Staged) => {
                            if binary.contains(pointer.oid.as_str()) {
                                LfsInspectionCoverage::BinaryNotScannedAsText
                            } else if blocked.is_none() {
                                LfsInspectionCoverage::TextInspected
                            } else {
                                LfsInspectionCoverage::InspectionNotConfirmed
                            }
                        }
                        Err(_) => LfsInspectionCoverage::AvailabilityNotConfirmed,
                    },
                })
                .collect(),
        }
    }

    /// Rendering returns text only; every external string is quoted without terminal controls.
    pub fn render(&self) -> String {
        let mut lines = vec![
            "Publication inspection summary".into(),
            format!("Target: {}", quoted(self.target)),
            format!("Hub: {}", quoted(&self.identity.hub)),
            format!("Agent identity: {}", quoted(&self.identity.agent_id)),
            match &self.deterministic {
                DeterministicInspectionState::Complete => {
                    "Deterministic inspection: complete for captured Git carriers and owned LFS bytes".into()
                }
                DeterministicInspectionState::Blocked { reason } => {
                    format!("Deterministic inspection: blocked; {}", quoted(reason))
                }
            },
            "Model review: not performed".into(),
            "This summary does not authorize publication.".into(),
        ];
        for (kind, references) in [("Branch", &self.heads), ("Tag", &self.tags)] {
            for reference in references {
                lines.push(format!(
                    "{kind}: {} at {}",
                    quoted(reference.name),
                    quoted(reference.oid)
                ));
            }
        }
        lines.push(format!(
            "Deterministic findings retained: {}",
            self.findings.len()
        ));
        for finding in &self.findings {
            lines.push(format!(
                "Finding: {} in {} at {}:{}; {}",
                quoted(finding.rule),
                quoted(finding.source),
                quoted(finding.file.unwrap_or("unspecified carrier")),
                finding.line,
                quoted(finding.redacted),
            ));
        }
        lines.push(format!("Findings truncated: {}", self.findings_truncated));
        if let Some(budget) = &self.unscanned.over_budget {
            lines.push(format!(
                "Unscanned budget: at least {} bytes counted against a {} byte budget",
                budget.counted_bytes_at_least, budget.budget_bytes,
            ));
        }
        for (kind, entries) in [
            ("object", self.unscanned.oversized_objects),
            ("file", self.unscanned.oversized_files),
        ] {
            for (label, size) in entries {
                lines.push(format!(
                    "Unscanned oversized {kind}: {} ({size} bytes)",
                    quoted(label)
                ));
            }
        }
        lines.push(format!(
            "Binary Git objects observed: {} (not scanned as text)",
            self.binary_git_objects,
        ));
        for payload in &self.lfs {
            let coverage = match payload.coverage {
                LfsInspectionCoverage::RemotePresentNotInspected => {
                    "remote present; bytes not inspected"
                }
                LfsInspectionCoverage::BinaryNotScannedAsText => {
                    "owned binary; not scanned as text"
                }
                LfsInspectionCoverage::TextInspected => {
                    "owned text; deterministic inspection complete"
                }
                LfsInspectionCoverage::InspectionNotConfirmed => {
                    "owned bytes; inspection not fully confirmed"
                }
                LfsInspectionCoverage::AvailabilityNotConfirmed => {
                    "availability and inspection not confirmed"
                }
            };
            lines.push(format!(
                "LFS: {} ({} bytes); {coverage}",
                quoted(&payload.pointer.oid),
                payload.pointer.size,
            ));
        }
        lines.join("\n") + "\n"
    }
}

fn quoted(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .chars()
            .flat_map(char::escape_default)
            .collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty(identity: &RemoteIdentity) -> InspectionSummary<'_> {
        InspectionSummary {
            target: "https://example.invalid/alice/repo.git",
            identity,
            heads: Vec::new(),
            tags: Vec::new(),
            deterministic: DeterministicInspectionState::Complete,
            model_review: ModelReviewState::NotPerformed,
            findings: Vec::new(),
            findings_truncated: false,
            unscanned: UnscannedSummary {
                over_budget: None,
                oversized_objects: &[],
                oversized_files: &[],
            },
            binary_git_objects: 0,
            lfs: Vec::new(),
        }
    }

    #[test]
    fn rendering_contains_untrusted_text_without_terminal_control_or_hidden_lines() {
        let hostile = "\x1b]52;c;clipboard\x07\r\n\t\u{85}\u{2028}\u{2029}\u{202e}\u{2066}\\\"";
        let identity = RemoteIdentity {
            hub: hostile.into(),
            agent_id: hostile.into(),
        };
        let oversized = vec![(hostile.into(), 123)];
        let pointer = Pointer {
            oid: hostile.into(),
            size: 7,
        };
        let mut summary = empty(&identity);
        summary.target = hostile;
        summary.heads.push(SelectedRef {
            name: hostile,
            oid: hostile,
        });
        summary.tags.push(SelectedRef {
            name: hostile,
            oid: hostile,
        });
        summary.deterministic = DeterministicInspectionState::Blocked {
            reason: hostile.into(),
        };
        summary.findings.push(Finding {
            rule: hostile,
            file: Some(hostile),
            source: hostile,
            line: 3,
            redacted: hostile,
        });
        summary.unscanned.oversized_objects = &oversized;
        summary.unscanned.oversized_files = &oversized;
        summary.lfs.push(LfsPayloadSummary {
            pointer: &pointer,
            coverage: LfsInspectionCoverage::RemotePresentNotInspected,
        });
        let text = summary.render();
        assert!(text.is_ascii());
        assert!(
            text.chars()
                .all(|character| character == '\n' || !character.is_control())
        );
        assert_eq!(text.lines().count(), 16);
        assert!(text.contains("\\u{1b}]52;c;clipboard\\u{7}\\r\\n\\t\\u{85}\\u{2028}\\u{2029}\\u{202e}\\u{2066}\\\\\\\""));
        assert!(text.contains("bytes not inspected"));
    }

    #[test]
    fn blocked_empty_findings_preserve_reason_and_lower_bound_without_model_approval() {
        let identity = RemoteIdentity {
            hub: "https://example.invalid".into(),
            agent_id: "identity".into(),
        };
        let mut summary = empty(&identity);
        summary.deterministic = DeterministicInspectionState::Blocked {
            reason: InspectionFailure::Content.to_string(),
        };
        summary.unscanned.over_budget = Some(BudgetExceeded {
            counted_bytes_at_least: 8,
            budget_bytes: 7,
        });
        summary.findings_truncated = true;
        let text = summary.render();
        assert!(text.contains("Deterministic inspection: blocked"));
        assert!(text.contains("cannot read its captured content"));
        assert!(text.contains("Findings truncated: true"));
        assert!(text.contains("at least 8 bytes counted against a 7 byte budget"));
        assert!(text.contains("Model review: not performed"));
        assert!(!text.contains("Deterministic inspection: complete"));
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["model_review"], "not_performed");
        assert_eq!(json["deterministic"]["state"], "blocked");
        assert_eq!(
            json["unscanned"]["over_budget"]["counted_bytes_at_least"],
            8
        );
    }
}
