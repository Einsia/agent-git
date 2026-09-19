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
    pub watermark: BranchWatermark,
    pub commit: tokio::process::Command,
    pub result_file: tempfile::NamedTempFile,
    pub prepared_file: tempfile::NamedTempFile,
}

pub(super) struct BranchWatermark {
    pub repository: PathBuf,
    pub reference: String,
}

#[cfg(feature = "cli")]
static NATIVE_WATERMARK_READS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

#[cfg(feature = "cli")]
async fn guarded_native_watermark(
    state: &mut tokio::sync::watch::Receiver<SettlementState>,
    lease: SettlementState,
    slots: &'static tokio::sync::Semaphore,
    read: impl FnOnce() -> Option<String> + Send + 'static,
) -> Option<Option<String>> {
    let Ok(permit) = slots.try_acquire() else {
        return Some(None);
    };
    let mut reading = tokio::task::spawn_blocking(move || {
        // A cancelled waiter cannot release the slot of a filesystem read still in progress.
        let _permit = permit;
        read()
    });
    loop {
        tokio::select! {
            biased;
            changed = state.changed() => {
                if changed.is_err() || !settlement_lease_is_current(state, lease) {
                    return None;
                }
            }
            result = &mut reading => {
                return settlement_lease_is_current(state, lease)
                    .then(|| result.ok().flatten());
            }
        }
    }
}

impl BranchWatermark {
    // Cancellation and an unreadable ref remain distinct at the settlement boundary.
    pub(super) async fn read(
        &self,
        state: &mut tokio::sync::watch::Receiver<SettlementState>,
        lease: SettlementState,
    ) -> Option<Option<String>> {
        if !settlement_lease_is_current(state, lease) {
            return None;
        }
        #[cfg(feature = "cli")]
        {
            let repository = self.repository.clone();
            let reference = self.reference.clone();
            if let Some(commit) =
                guarded_native_watermark(state, lease, &NATIVE_WATERMARK_READS, move || {
                    crate::domain::repo::Repo::at(repository).native_branch_commit(&reference)
                })
                .await?
            {
                return Some(Some(commit));
            }
        }
        let mut command = crate::infra::git_runtime::async_command();
        command
            .args(crate::domain::meta::GIT_SAFE)
            .arg("-C")
            .arg(&self.repository)
            .args(["rev-parse", "--verify", "--quiet", &self.reference])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0");
        let output = guarded_output(state, lease, command).await?;
        Some(
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned()),
        )
    }
}

#[cfg(all(test, feature = "cli"))]
mod tests {
    use super::*;

    /// Revoking a lease discards its result without admitting unbounded stuck reads.
    #[tokio::test]
    async fn cancelled_native_read_retains_its_slot_until_io_finishes() {
        static SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
        let lease = SettlementState {
            local_owner: true,
            ..Default::default()
        };
        let (authority, mut state) = tokio::sync::watch::channel(lease);
        let (started, started_rx) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let waiting = tokio::spawn(async move {
            guarded_native_watermark(&mut state, lease, &SLOTS, move || {
                let _ = started.send(());
                released
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap();
                Some("stale".into())
            })
            .await
        });
        started_rx.await.unwrap();
        authority.send_modify(|state| state.epoch += 1);
        assert!(waiting.await.unwrap().is_none());
        assert_eq!(SLOTS.available_permits(), 0);

        let current = *authority.borrow();
        let mut state = authority.subscribe();
        assert_eq!(
            guarded_native_watermark(&mut state, current, &SLOTS, || {
                panic!("saturated native reads must retain the guarded subprocess path")
            })
            .await,
            Some(None)
        );
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while SLOTS.available_permits() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}

impl LocalCommitRequest {
    pub async fn run(mut self) -> Option<LocalCommit> {
        let _prepared_file = self.prepared_file;
        let started = std::time::Instant::now();
        let before = self.watermark.read(&mut self.settlement, self.lease).await;
        trace_phase(&self.session_id, "settlement.read_before", started);
        let before = before?.unwrap_or_default();

        let started = std::time::Instant::now();
        let output = guarded_output(&mut self.settlement, self.lease, self.commit).await;
        trace_phase(&self.session_id, "settlement.commit", started);
        let output = output?;
        let started = std::time::Instant::now();
        let after = self.watermark.read(&mut self.settlement, self.lease).await;
        trace_phase(&self.session_id, "settlement.read_after", started);
        let after = after??;
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
