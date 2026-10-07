use super::*;
use futures_util::FutureExt;
use std::collections::BTreeSet;
use std::panic::AssertUnwindSafe;
use tokio::sync::watch;

const LAUNCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub(super) struct LaunchReservation {
    pub(super) generation: u64,
    pub(super) runtime: String,
    pub(super) native_source: Option<crate::protocol::NativeSourceRef>,
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
    pub(super) capture_project: Option<(String, PathBuf)>,
    pub(super) local_owner: bool,
    pub(super) prior_entry: Option<roster::Entry>,
    pub(super) prior_capture: Option<crate::rc::capture::RepositoryKind>,
    pub(super) incarnation: String,
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
            Self::Launch(mut spawn, reply) => {
                let result = async {
                    spawn
                        .resolve_capture()
                        .await
                        .map_err(SpawnFailure::before_launch)?;
                    daemon
                        .persist_capture(&spawn)
                        .map_err(SpawnFailure::before_launch)?;
                    spawn.execute().await
                }
                .await;
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
            Self::Launch(mut spawn, reply) => {
                // Cancellation or panic cannot prove that the OS spawn boundary was not crossed.
                // Keep the reservation until recovery instead of admitting another writer.
                let result = if *stop.borrow() {
                    Err(SpawnFailure::before_launch(RpcError::new(
                        ErrorCode::SessionBusy,
                        "the daemon is stopping; nothing was launched",
                    )))
                } else {
                    let launch = AssertUnwindSafe(async {
                        spawn
                            .resolve_capture()
                            .await
                            .map_err(SpawnFailure::before_launch)?;
                        {
                            let mut state = daemon.lock().await;
                            state
                                .persist_capture(&spawn)
                                .map_err(SpawnFailure::before_launch)?;
                        }
                        spawn.execute().await
                    })
                    .catch_unwind();
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
    async fn resolve_capture(&mut self) -> Result<(), RpcError> {
        if !self.local_owner {
            return Ok(());
        }
        let Some(native) = self.spec.resume_from.clone() else {
            return Ok(());
        };
        let lineage = super::capture_binding::Request {
            logical: self.info.session_id.clone(),
            runtime: self.info.runtime.clone(),
            native,
            native_source: self.info.native_source.clone(),
            cwd: self.spec.cwd.clone(),
            project: self.capture_project.clone(),
            prior_entry: self.prior_entry.clone(),
            prior_capture: self.prior_capture.clone(),
            wire: self.spec.agit_session.clone(),
            authority: self.authority.clone(),
            settlement: self.settlement.clone(),
        }
        .resolve()
        .await?;
        self.info.agent = lineage.as_ref().map(|lineage| lineage.slug());
        self.info.branch = lineage.as_ref().map(|lineage| lineage.branch().into());
        self.spec.agit_session = lineage;
        Ok(())
    }

    async fn revalidate_capture(&self) -> Result<(), RpcError> {
        if self.local_owner
            && let Some(lineage) = self.spec.agit_session.clone()
        {
            let runtime = self.info.runtime.clone();
            let native = self.spec.resume_from.as_ref().map(|native| {
                self.info
                    .native_source
                    .as_ref()
                    .map(|source| source.session_ref(native))
                    .unwrap_or_else(|| native.clone())
            });
            let cwd = self.spec.cwd.clone();
            tokio::task::spawn_blocking(move || -> crate::Result<()> {
                crate::rc::capture::require(&lineage)?;
                if let Some(native) = native {
                    crate::rc::capture::resolve(
                        &runtime,
                        &native,
                        &cwd,
                        Some(&lineage),
                        lineage.capture.as_ref(),
                    )?;
                }
                Ok(())
            })
            .await
            .map_err(|_| RpcError::new(ErrorCode::Internal, "capture revalidation failed"))?
            .map_err(|_| {
                RpcError::new(
                    ErrorCode::Forbidden,
                    "capture binding changed before launch",
                )
            })?;
        }
        Ok(())
    }

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
        self.revalidate_capture()
            .await
            .map_err(SpawnFailure::before_launch)?;
        let mut session = Session::launch(
            self.info.clone(), self.spec.clone(), out, self.notes.clone(),
            self.confinement.clone(), self.settlement.clone(), self.generation,
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
            if self.spec.resume_from.is_some() && failure.is_resume_rejected() {
                return SpawnFailure {
                    error: RpcError::new(ErrorCode::RuntimeUnavailable, failure.to_string()),
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
        session.publication_incarnation = Some(self.incarnation.clone());
        Ok(Spawned { session, frames })
    }
}

impl Daemon {
    fn persist_capture(&mut self, spawn: &PreparedSpawn) -> Result<(), RpcError> {
        if !spawn.local_owner {
            return Ok(());
        }
        spawn.authority.check()?;
        if let Some((project_id, project)) = &spawn.capture_project {
            spawn.authority.check_project(project_id, project)?;
            if self
                .mirror
                .project_path(&spawn.info.workspace_id, project_id)
                .as_ref()
                != Some(project)
            {
                return Err(RpcError::new(
                    ErrorCode::Forbidden,
                    "capture project binding changed",
                ));
            }
        }
        if self.settlement.borrow().epoch != spawn.epoch
            || self
                .opening_sessions
                .get(&spawn.info.session_id)
                .is_none_or(|reservation| reservation.generation != spawn.generation)
        {
            return Err(RpcError::new(
                ErrorCode::SessionBusy,
                "capture launch authority changed",
            ));
        }
        let id = &spawn.info.session_id;
        let coordinates = |entry: &roster::Entry| {
            (
                entry.runtime.clone(),
                entry.thread_id.clone(),
                entry.native_source.clone(),
                entry.cwd.clone(),
                entry.workspace_id.clone(),
                entry.agit_session.clone(),
                entry.expected_agent_id.clone(),
            )
        };
        if self.roster.get(id).map(coordinates) != spawn.prior_entry.as_ref().map(coordinates)
            || self.roster.captures.get(id) != spawn.prior_capture.as_ref()
        {
            return Err(RpcError::new(
                ErrorCode::SessionBusy,
                "capture association changed during inspection",
            ));
        }
        let previous = self.roster.clone();
        if let Some(lineage) = &spawn.spec.agit_session
            && let Some(kind) = &lineage.capture
        {
            if self
                .roster
                .captures
                .get(id)
                .is_some_and(|current| current != kind)
            {
                return Err(RpcError::new(
                    ErrorCode::Forbidden,
                    "capture repository identity changed",
                ));
            }
            self.roster.captures.insert(id.clone(), kind.clone());
        }
        let prior = self.roster.get(id);
        let entry = roster::Entry {
            native_source: spawn.info.native_source.clone(),
            runtime: spawn.info.runtime.clone(),
            thread_id: spawn.spec.resume_from.clone().unwrap_or_default(),
            cwd: spawn.spec.cwd.to_string_lossy().into_owned(),
            workspace_id: spawn.info.workspace_id.clone(),
            project_id: spawn.info.project_id.clone(),
            agit_session: spawn.spec.agit_session.as_ref().map(ToString::to_string),
            expected_agent_id: spawn
                .spec
                .agit_session
                .as_ref()
                .map(|lineage| lineage.agent_id().into()),
            permission_mode: spawn.info.permission_mode,
            guard_attempts: prior
                .map(|entry| entry.guard_attempts.clone())
                .unwrap_or_default(),
            prior_threads: prior
                .map(|entry| entry.prior_threads.clone())
                .unwrap_or_default(),
            ever_dangerous: spawn.info.dangerous,
        };
        if let Err(error) = self.roster.record(id, entry) {
            self.roster = previous;
            return Err(RpcError::new(ErrorCode::Forbidden, error.to_string()));
        }
        if self.roster.save().is_err() {
            self.roster = previous;
            return Err(RpcError::new(
                ErrorCode::Internal,
                "capture binding could not be persisted; nothing was launched",
            ));
        }
        Ok(())
    }
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
                self.prepare_resume_session(frame.params_as()?, &caller, frames, &frame.authority)
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
        let native_conflict = |runtime: &str,
                               source: Option<&crate::protocol::NativeSourceRef>,
                               native: Option<&str>| {
            runtime == info.runtime
                && source.map(|source| &source.source_id)
                    == info.native_source.as_ref().map(|source| &source.source_id)
                && spec
                    .resume_from
                    .as_deref()
                    .is_some_and(|wanted| Some(wanted) == native)
        };
        let archive_busy = self.archive_recovering.contains_key(&info.session_id)
            || self.archive_recovering.values().any(|job| {
                native_conflict(&job.runtime, job.native_source.as_ref(), Some(&job.native))
            });
        let observation_busy = self.opening_sessions.iter().any(|(logical, opening)| {
            opening.generation == 0
                && self.roster.observed.contains(logical)
                && (logical == &info.session_id
                    || native_conflict(
                        &opening.runtime,
                        opening.native_source.as_ref(),
                        opening.native_id.as_deref(),
                    ))
        });
        if self.sessions.contains_key(&info.session_id)
            || self.opening_sessions.contains_key(&info.session_id)
            || archive_busy
            || self.sessions.values().any(|live| {
                native_conflict(
                    &live.info.runtime,
                    live.info.native_source.as_ref(),
                    live.runtime_thread_id.as_deref(),
                )
            })
            || self.opening_sessions.values().any(|opening| {
                native_conflict(
                    &opening.runtime,
                    opening.native_source.as_ref(),
                    opening.native_id.as_deref(),
                )
            })
        {
            let mut error = RpcError::new(
                ErrorCode::SessionBusy,
                "this conversation already has a live or unresolved harness launch",
            );
            if archive_busy || observation_busy {
                error.data = Some(serde_json::json!({
                    "retryable": true,
                    "outcome": "not_sent",
                    "reason": "archive_settlement"
                }));
            }
            return Err(SpawnFailure::before_launch(error));
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
        let info = session.info.clone();
        self.latest_session_generations
            .insert(session_id.clone(), generation);
        self.journal.resume(&session_id);
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(COMMAND_QUEUE_CAPACITY);
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
        let shared_executor = session.shared_executor();
        let runtime_thread_id = session.runtime_thread_id().or(spec.resume_from);
        let task = tokio::spawn(session.run(cmd_rx));
        self.sessions.insert(
            session_id.clone(),
            Live {
                generation,
                task,
                danger_arm: 0,
                pending_mode: None,
                approval_requests: HashMap::new(),
                rpc_gate: Arc::new(Mutex::new(())),
                interrupt_gate: Arc::new(Mutex::new(())),
                approval_gate: Arc::new(Mutex::new(())),
                rpc_guard_sensitive: false,
                confirmed_turn_guards: Default::default(),
                inflight_turn_guard: None,
                restart_guard_attempts,
                restart_guard_mode,
                ended: false,
                info: info.clone(),
                shared_executor,
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
            interrupt_fenced: None,
            publication: None,
            session_id: "agit-opening".into(),
            native_source: None,
            runtime_session_id: None,
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

    /// An unbound roster can acquire exact local capture only after its durable save succeeds.
    #[cfg(unix)]
    #[tokio::test]
    async fn imported_capture_upgrades_an_unbound_roster_before_launch() {
        if crate::rc::in_isolated_test(
            "rc::daemon::opening::tests::imported_capture_upgrades_an_unbound_roster_before_launch",
        ) {
            return;
        }
        use crate::{
            domain::{link, repo::Repo, store::Store},
            rc::lineage::AgitSession,
        };
        let home = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", home.path().join("agit"));
        }
        crate::rc::select_local_authority();
        let (_root, daemon, mut spawn) = fixture().await;
        let native = uuid::Uuid::now_v7().to_string();
        let codex_home = home.path().join("codex");
        std::fs::create_dir(&codex_home).unwrap();
        let transcript = codex_home.join(format!("rollout-{native}.jsonl"));
        std::fs::write(
            &transcript,
            format!("{}\n", serde_json::json!({"type":"session_meta","payload":{"id":native,"cwd":spawn.spec.cwd}})),
        ).unwrap();
        let db = rusqlite::Connection::open(codex_home.join("state_1.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, archived INTEGER, first_user_message TEXT, thread_source TEXT, updated_at_ms INTEGER)").unwrap();
        db.execute(
            "INSERT INTO threads VALUES (?1,?2,?3,0,'question','cli',1)",
            rusqlite::params![native, transcript.to_str(), spawn.spec.cwd.to_str()],
        )
        .unwrap();
        let source = crate::rc::runtime_sources::Registry::open()
            .unwrap()
            .register(&codex_home, None, None, None)
            .unwrap();
        let binding = link::NativeBinding {
            source: crate::protocol::NativeSourceRef {
                source_id: source.source_id,
                generation: source.generation,
            },
            thread_id: native.clone(),
        };
        let store = Store::open_or_init().unwrap();
        let mut claim = link::Link::from_native(binding.clone(), &spawn.spec.cwd).unwrap();
        claim.owner = Some("alice".into());
        claim.agent = Some("imported".into());
        claim.branch = Some("work".into());
        link::write(&store, &claim).unwrap();
        let lineage = AgitSession::new(
            "alice/imported",
            "00000000-0000-0000-0000-000000000001",
            "work",
        )
        .unwrap();
        let repo = Repo::init(&lineage.repo_dir().unwrap()).unwrap();
        repo.git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ])
        .unwrap();
        repo.git(&["branch", "work"]).unwrap();
        crate::hub::identity::pin(
            &repo,
            &crate::hub::identity::RemoteIdentity::new("https://hub.invalid", lineage.agent_id())
                .unwrap(),
        )
        .unwrap();
        let row: roster::Entry = serde_json::from_value(serde_json::json!({
            "runtime":"codex", "thread_id":native, "cwd":spawn.spec.cwd,
            "native_source":binding.source,
            "workspace_id":"ws", "project_id":"project"
        }))
        .unwrap();
        spawn.local_owner = true;
        spawn.info.runtime = "codex".into();
        spawn.info.native_source = Some(binding.source.clone());
        spawn.spec.resume_from = Some(native);
        spawn.prior_entry = Some(row.clone());
        daemon
            .lock()
            .await
            .roster
            .record(&spawn.info.session_id, row)
            .unwrap();
        spawn.resolve_capture().await.unwrap();
        assert_eq!(spawn.info.agent.as_deref(), Some("alice/imported"));
        assert_eq!(spawn.info.branch.as_deref(), Some("work"));
        spawn.revalidate_capture().await.unwrap();
        spawn.info.native_source.as_mut().unwrap().source_id = uuid::Uuid::now_v7().to_string();
        assert!(spawn.revalidate_capture().await.is_err());
        spawn.info.native_source = Some(binding.source.clone());
        let mut state = daemon.lock().await;
        roster::fail_next_saves(1, 0);
        assert!(state.persist_capture(&spawn).is_err());
        assert!(state.roster.captures.is_empty());
        assert!(
            state
                .roster
                .get(&spawn.info.session_id)
                .unwrap()
                .agit_session
                .is_none()
        );
        assert!(state.sessions.is_empty());
        state.persist_capture(&spawn).unwrap();
        let restarted = Roster::try_load().unwrap();
        assert_eq!(
            restarted.get(&spawn.info.session_id).unwrap().native_source,
            Some(binding.source.clone())
        );
        assert_eq!(
            restarted
                .get(&spawn.info.session_id)
                .unwrap()
                .agit_session
                .as_deref(),
            Some("alice/imported@work")
        );
        assert_eq!(
            restarted.captures.get(&spawn.info.session_id),
            spawn.spec.agit_session.as_ref().unwrap().capture.as_ref()
        );
        assert!(
            state.persist_capture(&spawn).is_err(),
            "an obsolete inspection cannot overwrite the association"
        );
        assert_eq!(
            link::get_checked(&store, "codex", &claim.session_id)
                .unwrap()
                .unwrap()
                .to_json()
                .unwrap(),
            claim.to_json().unwrap()
        );
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

    #[tokio::test]
    async fn launch_reservations_distinguish_sources_but_not_their_generations() {
        let (_dir, daemon, spawn) = fixture().await;
        let mut state = daemon.lock().await;
        let source = crate::protocol::NativeSourceRef {
            source_id: "alpha".into(),
            generation: 1,
        };
        let reserved = state
            .opening_sessions
            .get_mut(&spawn.info.session_id)
            .unwrap();
        reserved.native_source = Some(source.clone());
        reserved.native_id = Some("copied-native".into());
        let mut alias = spawn.info.clone();
        alias.session_id = "other-logical".into();
        alias.native_source = Some(source);
        let mut spec = spawn.spec.clone();
        spec.resume_from = Some("copied-native".into());
        assert!(state.require_launch_slot(&alias, &spec).is_err());
        alias.native_source.as_mut().unwrap().generation = 2;
        assert!(state.require_launch_slot(&alias, &spec).is_err());
        alias.native_source.as_mut().unwrap().source_id = "beta".into();
        assert!(state.require_launch_slot(&alias, &spec).is_ok());
        alias.native_source = None;
        assert!(state.require_launch_slot(&alias, &spec).is_ok());
        alias.session_id = spawn.info.session_id;
        assert!(state.require_launch_slot(&alias, &spec).is_err());
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
