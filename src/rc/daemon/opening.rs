use super::*;
use futures_util::FutureExt;
use std::collections::BTreeSet;
use std::panic::AssertUnwindSafe;
use tokio::sync::watch;

const LAUNCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub(super) struct LaunchReservation {
    pub(super) generation: u64,
    pub(super) runtime: String,
    pub(super) native_id: Option<String>,
}

pub(super) enum OpeningReply {
    Start(Option<String>),
    Resume,
}

pub(super) enum SessionOpening {
    Ready(serde_json::Value),
    Launch(Box<PreparedSpawn>, OpeningReply),
}

pub(super) struct PreparedSpawn {
    pub(super) authority: crate::rc::authority::Guard,
    pub(super) epoch: u64,
    #[cfg(test)]
    pub(super) launch_pause: Option<Arc<tokio::sync::Notify>>,
    pub(super) info: SessionInfo,
    pub(super) spec: LaunchSpec,
    pub(super) generation: u64,
    pub(super) restart_guard_attempts: BTreeSet<String>,
    pub(super) restart_guard_mode: Option<crate::protocol::PermissionMode>,
    pub(super) frames: mpsc::Sender<Frame>,
    pub(super) notes: mpsc::Sender<SessionNote>,
    pub(super) confinement: watch::Receiver<crate::rc::Confinement>,
    pub(super) settlement: watch::Receiver<SettlementState>,
    pub(super) secret_filter: crate::domain::secret_filter::MatcherHandle,
    pub(super) prompt: Option<String>,
    pub(super) attribution: MessageAttribution,
}

pub(super) struct Spawned {
    session: Session,
    frames: mpsc::Receiver<Frame>,
}

impl SessionOpening {
    pub(super) fn launch(spawn: PreparedSpawn, reply: OpeningReply) -> Self {
        Self::Launch(Box::new(spawn), reply)
    }

    #[cfg(test)]
    pub(super) async fn run_inline(
        self,
        daemon: &mut Daemon,
    ) -> Result<serde_json::Value, RpcError> {
        match self {
            Self::Ready(value) => Ok(value),
            Self::Launch(spawn, reply) => {
                let result = spawn.execute().await;
                let result = daemon.finish_spawn(*spawn, result);
                reply.finish(daemon, result)
            }
        }
    }

    pub(super) async fn serve(
        self,
        daemon: Arc<Mutex<Daemon>>,
        outbound: crate::rc::outbound::OutboundTx,
        id: crate::protocol::RequestId,
        mut stop: watch::Receiver<bool>,
    ) {
        let result = match self {
            Self::Ready(value) => Ok(value),
            Self::Launch(spawn, reply) => {
                // Cancellation or panic cannot prove that the OS spawn boundary was not crossed.
                // Keep the reservation until recovery instead of admitting another writer.
                let result = if *stop.borrow() {
                    Err(SpawnFailure::before_launch(RpcError::new(
                        ErrorCode::SessionBusy,
                        "the daemon is stopping; nothing was launched",
                    )))
                } else {
                    let launch = AssertUnwindSafe(spawn.execute()).catch_unwind();
                    tokio::select! {
                        biased;
                        _ = stop.changed() => Err(unknown_launch("the daemon stopped during launch")),
                        result = tokio::time::timeout(LAUNCH_TIMEOUT, launch) => match result {
                            Ok(Ok(result)) => result,
                            Ok(Err(_)) => Err(unknown_launch("the harness launch worker panicked")),
                            Err(_) => Err(unknown_launch("the harness launch timed out")),
                        },
                    }
                };
                let mut state = daemon.lock().await;
                let result = state.finish_spawn(*spawn, result);
                reply.finish(&mut state, result)
            }
        };
        let frame = match result {
            Ok(value) => Frame::response(id, value),
            Err(error) => Frame::error_response(id, error),
        };
        let _ = outbound.send(frame);
    }
}

fn unknown_launch(message: &str) -> SpawnFailure {
    SpawnFailure::after_launch(RpcError::new(ErrorCode::SessionBusy, message).with_hint(
        "launch completion is unknown; this session remains reserved and will not be launched again automatically",
    ))
}

impl OpeningReply {
    fn finish(
        self,
        daemon: &mut Daemon,
        result: Result<SessionInfo, SpawnFailure>,
    ) -> Result<serde_json::Value, RpcError> {
        match self {
            Self::Resume => {
                Ok(serde_json::to_value(SessionResumeResult { session: result? }).unwrap())
            }
            Self::Start(start_id) => {
                let session = result.map_err(|failure| match &start_id {
                    Some(id) => daemon.failed_start(id, failure),
                    None => failure.error,
                })?;
                let result = SessionStartResult {
                    start_id: start_id.clone(),
                    session,
                };
                if let Some(id) = start_id {
                    daemon.persist_completed_start(&id, result.clone())?;
                }
                Ok(serde_json::to_value(result).unwrap())
            }
        }
    }
}

impl PreparedSpawn {
    pub(super) async fn execute(&self) -> Result<Spawned, SpawnFailure> {
        #[cfg(test)]
        if let Some(pause) = &self.launch_pause {
            pause.notified().await;
        }
        if self.settlement.borrow().epoch != self.epoch {
            return Err(SpawnFailure::before_launch(RpcError::new(
                ErrorCode::SessionBusy,
                "the connection authority changed before launch",
            )));
        }
        // Confinement may change after admission while this worker waits for executor time.
        policy::require_within(&self.spec.cwd, &self.confinement.borrow().roots).map_err(
            |error| {
                SpawnFailure::before_launch(RpcError::new(
                    ErrorCode::PathNotAllowed,
                    error.to_string(),
                ))
            },
        )?;
        let (out, frames) = mpsc::channel(1024);
        self.authority
            .check()
            .map_err(SpawnFailure::before_launch)?;
        let session = Session::launch(
            self.info.clone(), self.spec.clone(), out, self.notes.clone(),
            self.confinement.clone(), self.settlement.clone(), self.generation,
            self.secret_filter.clone(),
        ).await.map_err(|failure| {
            if self.spec.resume_from.is_some() && failure.is_external_writer() {
                return SpawnFailure {
                    error: RpcError::new(ErrorCode::SessionBusy, failure.to_string()).with_hint(
                        "this session is read-only while another application controls it; retry resume after that application releases control",
                    ),
                    reached_launch: true,
                    release_reservation: true,
                };
            }
            let reached_spawn = failure.reached_spawn();
            let error = RpcError::new(ErrorCode::RuntimeUnavailable, failure.to_string());
            if reached_spawn {
                SpawnFailure::after_launch(error)
            } else {
                SpawnFailure::before_launch(error.with_hint(
                    "nothing was launched on this machine; fix the runtime and retry the same start_id",
                ))
            }
        })?;
        Ok(Spawned { session, frames })
    }
}

impl Daemon {
    pub(super) fn prepare_opening(
        &mut self,
        frame: &Frame,
        frames: &mpsc::Sender<Frame>,
    ) -> Result<SessionOpening, RpcError> {
        let caller = caller_scope(frame)?;
        require_role(&caller, frame.method())?;
        let mut opening = match frame.method() {
            method::SESSION_START => {
                self.prepare_start_session(frame.params_as()?, &caller, frames, &frame.authority)
            }
            method::SESSION_RESUME => {
                self.prepare_resume_session(frame.params_as()?, &caller, frames)
            }
            _ => Err(RpcError::new(
                ErrorCode::UnknownMethod,
                "not a session opening request",
            )),
        }?;
        if let SessionOpening::Launch(spawn, _) = &mut opening {
            spawn.authority = frame.authority.clone();
        }
        Ok(opening)
    }

    pub(super) fn require_launch_slot(
        &self,
        info: &SessionInfo,
        spec: &LaunchSpec,
    ) -> Result<(), SpawnFailure> {
        let native_conflict = |runtime: &str, native: Option<&str>| {
            runtime == info.runtime
                && spec
                    .resume_from
                    .as_deref()
                    .is_some_and(|wanted| Some(wanted) == native)
        };
        if self.sessions.contains_key(&info.session_id)
            || self.opening_sessions.contains_key(&info.session_id)
            || self
                .sessions
                .values()
                .any(|live| native_conflict(&live.info.runtime, live.runtime_thread_id.as_deref()))
            || self
                .opening_sessions
                .values()
                .any(|opening| native_conflict(&opening.runtime, opening.native_id.as_deref()))
        {
            return Err(SpawnFailure::before_launch(RpcError::new(
                ErrorCode::SessionBusy,
                "this conversation already has a live or unresolved harness launch",
            )));
        }
        Ok(())
    }

    pub(super) fn failed_start(&mut self, start_id: &str, failure: SpawnFailure) -> RpcError {
        if failure.reached_launch {
            return RpcError::new(ErrorCode::SessionBusy, format!(
                "session.start was durably reserved but launch completion is unknown: {}", failure.error.message,
            )).with_hint(format!(
                "no second launch will be attempted for {start_id}; inspect this machine and retry the same start_id after recovery",
            ));
        }
        self.roster.forget_start(start_id);
        if let Err(error) = self.roster.save() {
            eprintln!(
                "agitd: released session.start {start_id} in memory but could not persist it: {error:#}"
            );
        }
        failure.error
    }

    pub(super) fn finish_spawn(
        &mut self,
        prepared: PreparedSpawn,
        result: Result<Spawned, SpawnFailure>,
    ) -> Result<SessionInfo, SpawnFailure> {
        let PreparedSpawn {
            info,
            spec,
            generation,
            restart_guard_attempts,
            restart_guard_mode,
            frames,
            prompt,
            attribution,
            ..
        } = prepared;
        let session_id = info.session_id.clone();
        if self
            .opening_sessions
            .get(&session_id)
            .is_none_or(|reservation| reservation.generation != generation)
        {
            return Err(unknown_launch(
                "the harness launch reservation was superseded",
            ));
        }
        let Spawned {
            session,
            frames: mut tagged_rx,
        } = match result {
            Ok(spawned) => spawned,
            Err(failure) => {
                if failure.release_reservation {
                    self.opening_sessions.remove(&session_id);
                }
                return Err(failure);
            }
        };
        self.latest_session_generations
            .insert(session_id.clone(), generation);
        self.journal.resume(&session_id);
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(64);
        // Bootstrap is queued before the supervisor can run or a caller can address this Live.
        // The private queue has room for the entire bootstrap, so no global lock crosses a wait.
        if needs_claude_restart_guard_barrier(&info.runtime, &restart_guard_attempts) {
            cmd_tx
                .try_send(Command::ClaudeRestartGuardReady)
                .map_err(|_| unknown_launch("the Claude recovery barrier could not be queued"))?;
        }
        if let Some(message) = prompt {
            cmd_tx
                .try_send(Command::InitialTurn {
                    message,
                    attribution,
                })
                .map_err(|_| unknown_launch("the initial instruction could not be queued"))?;
        }
        let runtime_thread_id = session.runtime_thread_id().or(spec.resume_from);
        let task = tokio::spawn(session.run(cmd_rx));
        self.sessions.insert(
            session_id.clone(),
            Live {
                generation,
                task,
                danger_arm: 0,
                pending_mode: None,
                approval_session_modes: HashMap::new(),
                rpc_gate: Arc::new(Mutex::new(())),
                rpc_guard_sensitive: false,
                confirmed_turn_guards: Default::default(),
                inflight_turn_guard: None,
                restart_guard_attempts,
                restart_guard_mode,
                ended: false,
                info: info.clone(),
                tx: cmd_tx,
                runtime_thread_id,
            },
        );
        self.opening_sessions.remove(&session_id);
        tokio::spawn(async move {
            while let Some(mut frame) = tagged_rx.recv().await {
                tag_session_frame(&mut frame, &session_id, generation);
                if frames.send(frame).await.is_err() {
                    break;
                }
            }
        });
        Ok(self.stamped(info))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delegated_start_checks_current_binding_before_creating_repository_or_session() {
        struct CachedAuthority(std::path::PathBuf);
        impl crate::rc::authority::Authority for CachedAuthority {
            fn admit(&self, accept: &mut dyn FnMut() -> bool) -> bool {
                accept()
            }
            fn project(&self) -> Option<(&str, &std::path::Path)> {
                Some(("project", &self.0))
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let old = directory.path().join("old");
        let new = directory.path().join("new");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        let daemon = super::super::tests::rpc_test_daemon(HashMap::new(), Roster::default());
        let mut daemon = daemon.lock().await;
        daemon.opts.local_owner = true;
        daemon
            .settlement
            .send_modify(|state| state.session_start_idempotency_v1 = true);
        let bound = daemon.mirror.bind("local-owner", "project", &old).unwrap();
        let mut request = Frame::request(
            "session.start",
            serde_json::json!({
                "workspace_id":"local-owner", "project_id":"project", "runtime":"codex",
                "start_id":uuid::Uuid::new_v4().to_string()
            }),
        );
        request.caller = Some(crate::protocol::CallerClaim {
            account_id: Some("member".into()),
            username: None,
            role: "operator".into(),
            workspace_id: "local-owner".into(),
        });
        request.authority = crate::rc::authority::Guard::new(CachedAuthority(bound));
        daemon.mirror.bind("local-owner", "project", &new).unwrap();
        request.authority.check().unwrap();
        let (frames, _) = mpsc::channel(1);
        let error = match daemon.prepare_opening(&request, &frames) {
            Err(error) => error,
            Ok(_) => panic!("stale project authority admitted a launch"),
        };
        assert_eq!(error.code, ErrorCode::Forbidden as i32);
        assert!(error.message.contains("project binding changed"));
        assert!(daemon.roster.starts.is_empty());
        assert!(daemon.opening_sessions.is_empty());
        assert!(daemon.sessions.is_empty());
    }

    async fn fixture() -> (tempfile::TempDir, Arc<Mutex<Daemon>>, PreparedSpawn) {
        let dir = tempfile::tempdir().unwrap();
        let daemon = super::super::tests::rpc_test_daemon(HashMap::new(), Roster::default());
        let mut state = daemon.lock().await;
        let cwd = state.mirror.bind("ws", "project", dir.path()).unwrap();
        let info = SessionInfo {
            session_id: "agit-opening".into(),
            workspace_id: "ws".into(),
            project_id: Some("project".into()),
            runtime: "unsupported-test-runtime".into(),
            agent: None,
            branch: None,
            status: SessionStatus::Idle,
            last_seq: 0,
            gist: None,
            title: None,
            dangerous: false,
            permission_mode: Some(crate::protocol::PermissionMode::Default),
            created_at: String::new(),
            updated_at: String::new(),
        };
        let spec = LaunchSpec {
            cwd,
            resume_from: None,
            agit_session: None,
            model: None,
            dangerous: false,
            permission_mode: info.permission_mode,
        };
        let (frames, _) = mpsc::channel(1);
        let spawn = state
            .prepare_spawn(
                info,
                spec,
                danger::TranscriptDanger::fresh_transcript(),
                &frames,
                None,
                Default::default(),
            )
            .unwrap_or_else(|error| panic!("{}", error.error.message));
        drop(state);
        (dir, daemon, spawn)
    }

    /// An unresolved opening excludes native aliases across workspace boundaries.
    #[tokio::test]
    async fn pending_launch_reserves_logical_and_native_identity() {
        let (_dir, daemon, spawn) = fixture().await;
        let mut state = daemon.lock().await;
        assert!(state.require_launch_slot(&spawn.info, &spawn.spec).is_err());
        state
            .opening_sessions
            .get_mut(&spawn.info.session_id)
            .unwrap()
            .native_id = Some("native".into());
        let mut alias = spawn.info.clone();
        alias.session_id = "agit-alias".into();
        alias.workspace_id = "another-workspace".into();
        let mut spec = spawn.spec.clone();
        spec.resume_from = Some("native".into());
        assert!(state.require_launch_slot(&alias, &spec).is_err());
        spec.resume_from = Some("different-native".into());
        assert!(state.require_launch_slot(&alias, &spec).is_ok());
    }

    /// Waiting for one harness leaves daemon state available, including other launch slots.
    #[tokio::test(start_paused = true)]
    async fn stuck_launch_does_not_hold_daemon_lock_and_times_out_once() {
        let (_dir, daemon, mut spawn) = fixture().await;
        spawn.launch_pause = Some(Arc::new(tokio::sync::Notify::new()));
        let (out, mut replies) = crate::rc::outbound::channel();
        let (_stop, stopped) = watch::channel(false);
        let id = crate::protocol::RequestId::Num(42);
        let task = tokio::spawn(SessionOpening::launch(spawn, OpeningReply::Resume).serve(
            daemon.clone(),
            out,
            id.clone(),
            stopped,
        ));
        tokio::task::yield_now().await;
        {
            let state = daemon
                .try_lock()
                .expect("an opening worker must release daemon state");
            assert!(state.sessions.is_empty());
            assert!(state.opening_sessions.contains_key("agit-opening"));
        }
        tokio::time::advance(LAUNCH_TIMEOUT).await;
        task.await.unwrap();
        let reply = replies.next_write().await.unwrap();
        assert_eq!(reply.frame().id, Some(id));
        assert!(reply.frame().error.is_some());
        reply.commit();
        assert!(replies.next_write().await.is_none());
        let state = daemon.lock().await;
        assert!(state.sessions.is_empty());
        assert!(
            state.opening_sessions.contains_key("agit-opening"),
            "an unknown outcome cannot release its writer reservation"
        );
    }

    #[tokio::test]
    async fn pre_spawn_failure_releases_the_reservation_without_registering_a_generation() {
        let (_dir, daemon, spawn) = fixture().await;
        let result = spawn.execute().await;
        let mut state = daemon.lock().await;
        let error = state
            .finish_spawn(spawn, result)
            .expect_err("unsupported runtime cannot spawn");
        assert!(!error.reached_launch);
        assert!(state.opening_sessions.is_empty());
        assert!(state.latest_session_generations.is_empty());
        assert!(state.sessions.is_empty());
    }

    #[tokio::test]
    async fn authority_change_before_execution_does_not_launch() {
        let (_dir, daemon, spawn) = fixture().await;
        daemon
            .lock()
            .await
            .settlement
            .send_modify(|state| state.epoch += 1);
        let error = spawn
            .execute()
            .await
            .err()
            .expect("stale authority cannot launch");
        assert!(!error.reached_launch);
        assert!(error.error.message.contains("authority changed"));
        daemon
            .lock()
            .await
            .finish_spawn(spawn, Err(error))
            .err()
            .unwrap();
        assert!(daemon.lock().await.opening_sessions.is_empty());
    }

    #[tokio::test]
    async fn shutdown_before_launch_releases_the_reservation() {
        let (_dir, daemon, spawn) = fixture().await;
        let (out, mut replies) = crate::rc::outbound::channel();
        let (_stop, stopped) = watch::channel(true);
        SessionOpening::launch(spawn, OpeningReply::Resume)
            .serve(
                daemon.clone(),
                out,
                crate::protocol::RequestId::Num(1),
                stopped,
            )
            .await;
        assert!(replies.next_write().await.unwrap().frame().error.is_some());
        let state = daemon.lock().await;
        assert!(state.opening_sessions.is_empty());
        assert!(state.sessions.is_empty());
    }
}
