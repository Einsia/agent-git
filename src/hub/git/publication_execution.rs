//! Publication retains inspected payload ownership across every native phase and retry.

use super::CompleteInspection;
use crate::domain::lfs::Pointer;
use crate::hub::git::frozen::{Execution, LFS_REMOTE};
use crate::hub::git::{
    LfsAttemptKind, LfsPublicationAttempt, LfsPublicationPhase, OutputMode, PublicationReport,
    SecretFindingsAcceptance, TransportRun, execute_transport_in,
};
use anyhow::{Context, Result, ensure};

const LFS_BATCH_POINTERS: usize = 100;

impl CompleteInspection {
    /// Only a complete deterministic inspection can reach publication.
    /// Accepting findings applies to this operation and does not constitute model approval.
    /// Native Git LFS retains its own User-Agent and server-directed transfer policy.
    pub fn publish(mut self, acceptance: SecretFindingsAcceptance) -> PublicationReport {
        if self.has_findings() && acceptance == SecretFindingsAcceptance::Reject {
            return PublicationReport::refused(
                "complete secret findings require explicit acceptance for this publication".into(),
            );
        }
        self.prepared.publication.transport.accept_secret_findings =
            acceptance == SecretFindingsAcceptance::Accept;
        let lfs = self.publish_lfs();
        if !lfs.ok() {
            return PublicationReport {
                lfs: Some(lfs),
                heads: None,
                tags: None,
                error: None,
            };
        }
        let (heads, tags) = self.prepared.publication.push_refs();
        PublicationReport {
            lfs: Some(lfs),
            heads: Some(heads),
            tags,
            error: None,
        }
    }

    fn publish_lfs(&self) -> LfsPublicationPhase {
        let selected = self.prepared.upload_missing.clone();
        let present = self
            .prepared
            .pointers()
            .iter()
            .filter(|pointer| {
                selected
                    .binary_search_by(|candidate| candidate.oid.cmp(&pointer.oid))
                    .is_err()
            })
            .cloned()
            .collect();
        let mut phase = LfsPublicationPhase::selected(present, selected.clone());
        if selected.is_empty() {
            phase.finish();
            return phase;
        }
        let execution = match self.lfs_preflight(&selected) {
            Ok(execution) => execution,
            Err(error) => {
                phase.error = Some(error.to_string());
                return phase;
            }
        };
        let transport = &self.prepared.publication.transport;
        let version = execute_transport_in(
            None,
            &["lfs", "version"],
            transport,
            OutputMode::Captured,
            Some(&execution),
        );
        if !retain_lfs_attempts(&mut phase, LfsAttemptKind::Version, &[], version) {
            return phase;
        }
        let version = &phase
            .attempts
            .last()
            .expect("version attempt is retained")
            .stdout;
        let version = std::str::from_utf8(version)
            .context("unrecognized Git LFS version")
            .and_then(crate::domain::lfs::local::validate_client_version);
        if let Err(error) = version {
            phase.error = Some(error.to_string());
            return phase;
        }
        for (batch, pointers) in selected.chunks(LFS_BATCH_POINTERS).enumerate() {
            let mut args = vec!["lfs", "push", "--object-id", "--", LFS_REMOTE];
            args.extend(pointers.iter().map(|pointer| pointer.oid.as_str()));
            let run = execute_transport_in(
                None,
                &args,
                transport,
                OutputMode::Captured,
                Some(&execution),
            );
            if !run.attempts.is_empty() {
                let remaining = (batch + 1) * LFS_BATCH_POINTERS;
                phase.unattempted = selected.get(remaining..).unwrap_or_default().to_vec();
            }
            if !retain_lfs_attempts(&mut phase, LfsAttemptKind::Objects { batch }, pointers, run) {
                return phase;
            }
        }
        phase.finish();
        phase
    }

    fn lfs_preflight(&self, selected: &[Pointer]) -> Result<Execution> {
        for pointers in selected.chunks(LFS_BATCH_POINTERS) {
            let units = ["lfs", "push", "--object-id", "--", LFS_REMOTE]
                .into_iter()
                .chain(pointers.iter().map(|pointer| pointer.oid.as_str()))
                .map(crate::hub::git::frozen::argument_units)
                .sum::<usize>();
            ensure!(
                units < crate::hub::git::frozen::REQUEST_UNITS,
                "LFS publication batch exceeds its command limit"
            );
        }
        // All private payloads are verified before the first upload can have effects.
        for pointer in selected {
            let reader = self.prepared.open_payload(pointer)?;
            pointer
                .verify(reader)
                .map_err(|_| anyhow::anyhow!("private LFS payload changed after inspection"))?;
        }
        let storage = self
            .prepared
            .staged
            .as_ref()
            .context("prepared LFS payload storage is unavailable")?
            .storage();
        self.prepared.publication.lfs_execution(&storage)
    }
}

fn retain_lfs_attempts(
    phase: &mut LfsPublicationPhase,
    kind: LfsAttemptKind,
    pointers: &[Pointer],
    run: TransportRun,
) -> bool {
    let attempted = !run.attempts.is_empty();
    phase.attempts.extend(
        run.attempts
            .into_iter()
            .map(|output| LfsPublicationAttempt::observed(kind, pointers, output)),
    );
    phase.error = run.error.map(|error| format!("{error:#}"));
    if phase.error.is_none()
        && (!attempted || !phase.attempts.last().is_some_and(LfsPublicationAttempt::ok))
    {
        phase.error = Some("native LFS publication did not complete successfully".into());
    }
    phase.error.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::git::{Outcome, ProcessOutput};

    #[test]
    fn incomplete_native_capture_retains_bytes_and_cannot_complete_a_phase() {
        let pointer = Pointer {
            oid: "a".repeat(64),
            size: 1,
        };
        for (complete, error) in [
            (false, None),
            (true, Some("capture was interrupted".into())),
        ] {
            let mut phase = LfsPublicationPhase::selected(vec![], vec![pointer.clone()]);
            let run = TransportRun {
                attempts: vec![ProcessOutput {
                    outcome: Outcome {
                        code: 0,
                        stderr: "bounded stderr".into(),
                    },
                    stdout: b"bounded stdout".to_vec(),
                    complete,
                    error: error.clone(),
                }],
                error: None,
            };
            assert!(!retain_lfs_attempts(
                &mut phase,
                LfsAttemptKind::Objects { batch: 0 },
                std::slice::from_ref(&pointer),
                run
            ));
            assert!(!phase.ok());
            assert_eq!(phase.unattempted, vec![pointer.clone()]);
            assert_eq!(phase.attempts[0].outcome.stderr, "bounded stderr");
            assert_eq!(phase.attempts[0].stdout, b"bounded stdout");
            assert_eq!(phase.attempts[0].complete, complete);
            assert_eq!(phase.attempts[0].error, error);
        }
    }
}
