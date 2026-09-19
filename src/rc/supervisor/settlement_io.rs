//! Local settlement I/O owns its lease and subprocesses independently of harness reads.

use super::*;

pub(super) struct LocalCommit {
    pub before: String,
    pub output: std::process::Output,
    pub after: String,
    pub reported: Option<String>,
}

pub(super) struct LocalCommitRequest {
    pub session_id: String,
    pub settlement: tokio::sync::watch::Receiver<SettlementState>,
    pub lease: SettlementState,
    pub read_before: tokio::process::Command,
    pub commit: tokio::process::Command,
    pub read_after: tokio::process::Command,
    pub result_file: tempfile::NamedTempFile,
}

impl LocalCommitRequest {
    pub async fn run(mut self) -> Option<LocalCommit> {
        let started = std::time::Instant::now();
        let before = guarded_output(&mut self.settlement, self.lease, self.read_before).await;
        trace_phase(&self.session_id, "settlement.read_before", started);
        let before = before?;
        let before = if before.status.success() {
            String::from_utf8_lossy(&before.stdout).trim().to_string()
        } else {
            String::new()
        };

        let started = std::time::Instant::now();
        let output = guarded_output(&mut self.settlement, self.lease, self.commit).await;
        trace_phase(&self.session_id, "settlement.commit", started);
        let output = output?;
        let started = std::time::Instant::now();
        let after = guarded_output(&mut self.settlement, self.lease, self.read_after).await;
        trace_phase(&self.session_id, "settlement.read_after", started);
        let after = after?;
        if !after.status.success() {
            return None;
        }
        let after = String::from_utf8_lossy(&after.stdout).trim().to_string();
        let Ok(report) = std::fs::read_to_string(self.result_file.path()) else {
            tracing_note("strict settlement result could not be read");
            return None;
        };
        let reported = Some(report.trim().to_string()).filter(|report| !report.is_empty());
        Some(LocalCommit {
            before,
            output,
            after,
            reported,
        })
    }
}

impl Session {
    pub(super) async fn local_commit_with_reads(
        &mut self,
        request: LocalCommitRequest,
        commands: Option<&mut mpsc::Receiver<Command>>,
        deferred: &mut Option<Option<Command>>,
    ) -> Option<LocalCommit> {
        let commit = request.run();
        tokio::pin!(commit);
        let Some(commands) = commands else {
            return commit.await;
        };
        loop {
            tokio::select! {
                result = &mut commit => return result,
                command = commands.recv() => match command {
                    Some(Command::Model { model: None, reply }) => {
                        if !reply.accept() {
                            continue;
                        }
                        let metadata = self.driver.model_control(None);
                        tokio::pin!(metadata);
                        // Keep polling the owned Git transaction while the harness answers.
                        tokio::select! {
                            result = &mut commit => {
                                reply.finish(metadata.await);
                                return result;
                            }
                            result = &mut metadata => reply.finish(result),
                        }
                    }
                    command => {
                        *deferred = Some(command);
                        let started = std::time::Instant::now();
                        let result = commit.await;
                        trace_phase(&self.info.session_id, "settlement.command_wait", started);
                        return result;
                    }
                }
            }
        }
    }
}
