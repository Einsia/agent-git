//! Recovery reserves a capture writer only until its immutable publication intent is durable.

use super::*;
use crate::rc::archive_jobs::Job;

pub(super) struct Worker(tokio::task::JoinHandle<()>);
impl Drop for Worker {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// Recovery keeps its admission until completion or shutdown. A wall-clock cutoff can kill Git
// while it owns the index, leaving the durable retry unable to acquire repository locks.
async fn until_stopped<T>(
    stop: &mut tokio::sync::watch::Receiver<bool>,
    work: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        _ = stop.wait_for(|stopped| *stopped) => None,
        result = work => Some(result),
    }
}

pub(super) fn start(
    daemon: Arc<Mutex<Daemon>>,
    admission: crate::rc::admission::Admission,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> Worker {
    Worker(tokio::spawn(async move {
        let mut cursor = 0usize;
        let mut observed = super::observed_capture::Collector::default();
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = stop.wait_for(|stopped| *stopped) => break,
                _ = interval.tick() => {}
            }
            observed.reconcile(&daemon, &admission, &mut stop).await;
            let mut jobs = match tokio::task::spawn_blocking(crate::rc::archive_jobs::pending).await
            {
                Ok(Ok(jobs)) => jobs,
                result => {
                    eprintln!("agitd: pending archives could not be read: {result:?}");
                    continue;
                }
            };
            if !jobs.is_empty() {
                let offset = cursor % jobs.len();
                jobs.rotate_left(offset);
                cursor = cursor.wrapping_add(16);
            }
            for job in recovery_batch(jobs) {
                let Some(_work) = admission.enter() else {
                    return;
                };
                let authority = {
                    let mut state = daemon.lock().await;
                    if !state.reserve_archive(&job) {
                        continue;
                    }
                    state.settlement.subscribe()
                };
                let prepared = until_stopped(
                    &mut stop,
                    crate::rc::supervisor::archive_recovery::prepare(job.clone(), authority),
                )
                .await;
                daemon.lock().await.archive_recovering.remove(&job.logical);
                let completed = match prepared {
                    Some(Ok(prepared)) => {
                        until_stopped(
                            &mut stop,
                            crate::rc::supervisor::archive_recovery::publish(prepared),
                        )
                        .await
                    }
                    Some(Err(error)) => Some(Err(error)),
                    None => None,
                };
                match completed {
                    Some(Ok(())) => {}
                    Some(result) => eprintln!(
                        "agitd: completed turn archive remains pending ({}): {result:?}",
                        job.logical
                    ),
                    None => return,
                }
            }
            interval.reset();
        }
    }))
}

fn recovery_batch(jobs: Vec<Job>) -> Vec<Job> {
    let mut captures: Vec<Job> = Vec::new();
    for job in jobs {
        if captures.iter().any(|capture| capture.same_capture(&job)) {
            continue;
        }
        captures.push(job);
        if captures.len() == 16 {
            break;
        }
    }
    captures
}

impl Daemon {
    fn reserve_archive(&mut self, job: &Job) -> bool {
        if !self.opts.local_owner
            || !self.settlement.borrow().local_owner
            || self.sessions.contains_key(&job.logical)
            || self.opening_sessions.contains_key(&job.logical)
            || self.archive_recovering.contains_key(&job.logical)
        {
            return false;
        }
        let matches_native = |runtime: &str,
                              source: Option<&crate::protocol::NativeSourceRef>,
                              native: Option<&str>| {
            runtime == job.runtime
                && source == job.native_source.as_ref()
                && native == Some(job.native.as_str())
        };
        if self.sessions.values().any(|live| {
            matches_native(
                &live.info.runtime,
                live.info.native_source.as_ref(),
                live.runtime_thread_id.as_deref(),
            )
        }) || self.opening_sessions.values().any(|opening| {
            matches_native(
                &opening.runtime,
                opening.native_source.as_ref(),
                opening.native_id.as_deref(),
            )
        }) || self.archive_recovering.values().any(|pending| {
            matches_native(
                &pending.runtime,
                pending.native_source.as_ref(),
                Some(&pending.native),
            )
        }) {
            return false;
        }
        let Some(row) = self.roster.get(&job.logical) else {
            return false;
        };
        if row.workspace_id != crate::rc::endpoint::WORKSPACE
            || row.cwd != job.cwd.to_string_lossy()
            || row.runtime != job.runtime
            || row.native_source != job.native_source
            || (row.thread_id != job.native && !row.prior_threads.contains(&job.native))
            || row.agit_session.as_deref() != Some(job.lineage.as_str())
            || row.expected_agent_id.as_deref() != Some(job.repository_id.as_str())
            || self.roster.captures.get(&job.logical) != job.capture.as_ref()
        {
            return false;
        }
        self.archive_recovering
            .insert(job.logical.clone(), job.clone());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpublished_turns_share_a_recovery_attempt_without_excluding_other_captures() {
        let root = tempfile::tempdir().unwrap();
        let first = crate::rc::archive_jobs::tests::fixture(root.path());
        let mut next_turn = first.clone();
        next_turn.turn_id = "next-turn".into();
        let mut other = first.clone();
        other.native = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            recovery_batch(vec![first.clone(), next_turn, other.clone()]),
            vec![first, other]
        );
    }

    /// Long local transactions keep restart admission until completion; shutdown still cancels.
    #[tokio::test(start_paused = true)]
    async fn archive_completion_outlives_request_budgets_but_observes_shutdown() {
        let admission = crate::rc::admission::Admission::default();
        let work = admission.enter().unwrap();
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let running = tokio::spawn(async move {
            let _work = work;
            until_stopped(&mut stopped, async {
                tokio::time::sleep(std::time::Duration::from_secs(120)).await;
                "committed"
            })
            .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(61)).await;
        assert!(!running.is_finished());
        assert!(admission.freeze().is_err());
        assert_eq!(running.await.unwrap(), Some("committed"));
        assert!(admission.freeze().is_ok());

        let mut stopped = stop.subscribe();
        let running =
            tokio::spawn(
                async move { until_stopped(&mut stopped, std::future::pending::<()>()).await },
            );
        stop.send(true).unwrap();
        assert_eq!(running.await.unwrap(), None);
    }

    /// Recovery and a native attachment cannot hold the same capture writer concurrently.
    #[tokio::test]
    async fn recovery_reservations_exclude_live_and_new_native_attachments() {
        let root = tempfile::tempdir().unwrap();
        let job = crate::rc::archive_jobs::tests::fixture(root.path());
        let daemon = super::super::tests::rpc_test_daemon(HashMap::new(), Roster::default());
        let mut state = daemon.lock().await;
        state.opts.local_owner = true;
        state
            .settlement
            .send_modify(|authority| authority.local_owner = true);
        state.roster.sessions.insert(
            job.logical.clone(),
            roster::Entry {
                native_source: job.native_source.clone(),
                runtime: job.runtime.clone(),
                thread_id: job.native.clone(),
                cwd: job.cwd.to_string_lossy().into_owned(),
                workspace_id: crate::rc::endpoint::WORKSPACE.into(),
                project_id: None,
                agit_session: Some(job.lineage.clone()),
                expected_agent_id: Some(job.repository_id.clone()),
                permission_mode: None,
                guard_attempts: Default::default(),
                prior_threads: vec![],
                ever_dangerous: false,
            },
        );
        assert!(state.reserve_archive(&job));
        assert!(!state.reserve_archive(&job));
        let (sender, _receiver) = mpsc::channel(1);
        let mut alias = super::super::tests::rpc_test_live(
            "alias",
            1,
            sender,
            crate::protocol::PermissionMode::Default,
        );
        alias.info.runtime = job.runtime.clone();
        alias.info.native_source = job.native_source.clone();
        alias.runtime_thread_id = Some(job.native.clone());
        let spec = crate::rc::harness::LaunchSpec {
            cwd: job.cwd.clone(),
            resume_from: Some(job.native.clone()),
            agit_session: Some(job.session().unwrap()),
            model: None,
            dangerous: false,
            permission_mode: None,
        };
        let refused = state.require_launch_slot(&alias.info, &spec).unwrap_err();
        assert_eq!(refused.error.data.as_ref().unwrap()["retryable"], true);
        assert_eq!(refused.error.data.as_ref().unwrap()["outcome"], "not_sent");
        assert!(!refused.reached_launch);
        state.archive_recovering.clear();
        assert!(state.require_launch_slot(&alias.info, &spec).is_ok());
        state.sessions.insert("alias".into(), alias);
        assert!(!state.reserve_archive(&job));
        state.sessions.clear();
        assert!(state.reserve_archive(&job));
    }
}
