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
    pub prepared_file: tempfile::NamedTempFile,
}

impl LocalCommitRequest {
    pub async fn run(mut self) -> Option<LocalCommit> {
        let _prepared_file = self.prepared_file;
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

struct LocalCommitTask(tokio::task::JoinHandle<Option<LocalCommit>>);

impl Drop for LocalCommitTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) struct LocalSettlementContext {
    pub lease: SettlementState,
    pub receipt_path: PathBuf,
    pub repo_dir_s: String,
    pub slug: String,
    pub branch: String,
    pub expected_agent_id: String,
    pub journal_boundary: Option<Arc<std::sync::atomic::AtomicU64>>,
    pub push: tokio::process::Command,
}

pub(super) struct LocalSettlement {
    task: LocalCommitTask,
    context: LocalSettlementContext,
}

impl Session {
    pub(super) async fn finish_local_settlement(&mut self, wait: bool) {
        if !self
            .local_settlement
            .as_ref()
            .is_some_and(|task| wait || task.task.0.is_finished())
        {
            return;
        }
        let LocalSettlement { mut task, context } = self
            .local_settlement
            .take()
            .expect("local settlement is present");
        match (&mut task.0).await {
            Ok(Some(result)) => self.finish_local_commit(context, result).await,
            Ok(None) => {}
            Err(error) => tracing_note(&format!("local settlement worker failed: {error}")),
        }
    }

    pub(super) async fn local_commit_with_reads(
        &mut self,
        request: LocalCommitRequest,
        context: LocalSettlementContext,
        commands: Option<&mut mpsc::Receiver<Command>>,
        deferred: &mut Option<Option<Command>>,
    ) {
        let prepared_path = request.prepared_file.path().to_owned();
        self.local_settlement = Some(LocalSettlement {
            task: LocalCommitTask(tokio::spawn(request.run())),
            context,
        });
        let Some(commands) = commands else {
            self.finish_local_settlement(true).await;
            return;
        };
        let started = std::time::Instant::now();
        let mut command_wait = None;
        let mut readiness = tokio::time::interval(std::time::Duration::from_millis(10));
        readiness.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            if self
                .local_settlement
                .as_ref()
                .is_some_and(|task| task.task.0.is_finished())
            {
                self.finish_local_settlement(true).await;
                if let Some(started) = command_wait {
                    trace_phase(&self.info.session_id, "settlement.command_wait", started);
                }
                return;
            }
            if std::fs::read(&prepared_path).ok().as_deref() == Some(b"prepared\n") {
                trace_phase(&self.info.session_id, "settlement.prepared", started);
                if let Some(started) = command_wait {
                    trace_phase(&self.info.session_id, "settlement.command_wait", started);
                }
                return;
            }
            tokio::select! {
                _ = readiness.tick() => {}
                command = commands.recv(), if deferred.is_none() => match command {
                    Some(Command::Model { model: None, reply }) => {
                        if reply.accept() {
                            reply.finish(self.driver.model_control(None).await);
                        }
                    }
                    command => {
                        command_wait = Some(std::time::Instant::now());
                        *deferred = Some(command);
                    }
                }
            }
        }
    }
}
