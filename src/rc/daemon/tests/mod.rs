mod guards;
#[cfg(unix)]
mod leases;
mod metadata;
mod mode_restart;
#[cfg(unix)]
mod publication;
mod sessions;
mod stopping;
#[cfg(unix)]
mod terminals;
mod turns;

pub(super) fn rpc_test_live(
    session_id: &str,
    generation: u64,
    tx: mpsc::Sender<Command>,
    mode: crate::protocol::PermissionMode,
) -> Live {
    Live {
        generation,
        info: SessionInfo {
            interrupt_fenced: None,
            interrupted: None,
            paused: None,
            lease: None,
            publication: None,
            session_id: session_id.into(),
            native_source: None,
            runtime_session_id: None,
            workspace_id: "ws-a".into(),
            project_id: None,
            runtime: "codex".into(),
            agent: None,
            branch: None,
            status: SessionStatus::Running,
            last_seq: 0,
            gist: None,
            title: None,
            labels_withheld: None,
            dangerous: false,
            permission_mode: Some(mode),
            created_at: "now".into(),
            updated_at: "now".into(),
        },
        shared_executor: false,
        started_process: true,
        bypass_needs_restart: false,
        tx,
        runtime_thread_id: None,
        task: tokio::spawn(async {}),
        danger_arm: 0,
        pending_mode: None,
        approval_requests: HashMap::new(),
        settlement_unstartable: false,
        handed_approvals: Default::default(),
        rpc_gate: Arc::new(Mutex::new(())),
        interrupt_gate: Arc::new(Mutex::new(())),
        approval_gate: Arc::new(Mutex::new(())),
        rpc_guard_sensitive: false,
        confirmed_turn_guards: Default::default(),
        inflight_turn_guard: None,
        restart_guard_attempts: Default::default(),
        restart_guard_mode: None,
        ended: false,
        writer_released: false,
        writer_process: None,
        status_since: std::time::Instant::now(),
        last_named: std::time::Instant::now(),
    }
}

pub(super) fn rpc_test_daemon(
    sessions: HashMap<String, Live>,
    roster: Roster,
) -> Arc<Mutex<Daemon>> {
    let (notes, _notes_rx) = mpsc::channel(1);
    let (settlement, _) = tokio::sync::watch::channel(SettlementState::default());
    Arc::new(Mutex::new(Daemon {
        identity: crate::rc::build_identity::DaemonIdentity::current().unwrap(),
        deferred: vec![],
        deferred_slot: None,
        replay_slots: Arc::new(tokio::sync::Semaphore::new(REPLAY_SLOTS)),
        outbound: None,
        opts: Options {
            local_owner: false,
            hub: "https://hub.invalid".into(),
        },
        journal: Journal::new(),
        mirror: Mirror::default(),
        roster,
        sessions,
        latest_session_generations: HashMap::new(),
        opening_sessions: HashMap::new(),
        mode_restarts: HashMap::new(),
        session_frames: None,
        exiting_launches: HashMap::new(),
        archive_recovering: HashMap::new(),
        watches: HashMap::new(),
        terminals: HashMap::new(),
        terminal_delivery_blockers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        term_tx: None,
        online: true,
        stop_cause: StopCause::Person,
        replaced_by: None,

        settlement,
        started_at: std::time::Instant::now(),
        notes,
        grants: crate::rc::grants::Grants::default(),
        watch_generation: 0,
        session_generation: 2,
        confinement: HashMap::new(),
        leases: Default::default(),
    }))
}

pub(super) fn rpc_test_roster_entry(
    session_id: &str,
    mode: crate::protocol::PermissionMode,
) -> crate::rc::roster::Entry {
    crate::rc::roster::Entry {
        native_source: None,
        runtime: "codex".into(),
        thread_id: format!("thread-{session_id}"),
        cwd: "/tmp/project".into(),
        workspace_id: "ws-a".into(),
        project_id: None,
        agit_session: None,
        expected_agent_id: None,
        permission_mode: Some(mode),
        guard_attempts: Default::default(),
        prior_threads: vec![],
        ever_dangerous: false,
        project_share: false,
    }
}

fn hard_stop_guard(session_id: &str, generation: u64, suffix: &str) -> HardStopGuard {
    HardStopGuard {
        session_id: session_id.into(),
        generation,
        token: format!("{}{suffix}", roster::SHUTDOWN_GUARD_PREFIX),
        dangerous: false,
    }
}

fn tagged_test_notification(
    session_id: &str,
    generation: u64,
    method: &str,
    params: serde_json::Value,
) -> Frame {
    let mut frame = Frame::notification(method, params);
    tag_session_frame(&mut frame, session_id, generation);
    frame
}

fn test_approval(approval_id: &str) -> crate::protocol::ApprovalRequest {
    crate::protocol::ApprovalRequest {
        approval_id: approval_id.into(),
        session_id: "session-a".into(),
        turn_id: "turn-a".into(),
        kind: crate::protocol::ApprovalKind::Exec,
        tool: "shell".into(),
        input: serde_json::json!({"command":"cargo test"}),
        summary: "cargo test".into(),
        paths: vec![],
        timeout_secs: 30,
        requires_owner: true,
        owner_reason: None,
        can_allow_for_session: true,
        suggested_permission_mode: Some(crate::protocol::PermissionMode::Bypass),
        requested_at: "now".into(),
    }
}

fn rpc_mode_frame(mode: crate::protocol::PermissionMode) -> Frame {
    let mut frame = Frame::request(
        method::SESSION_SET_PERMISSION_MODE,
        crate::protocol::SessionSetPermissionMode {
            session_id: "session-a".into(),
            mode,
            by: Some("owner".into()),
        },
    );
    frame.caller = Some(claim("owner", "ws-a"));
    frame
}

pub(super) fn rpc_turn_frame(session_id: &str) -> Frame {
    let mut frame = Frame::request(
        method::TURN_START,
        TurnStart {
            session_id: session_id.into(),
            message: "inspect".into(),
            by: Some("owner".into()),
            client_msg_id: Some(format!("message-{session_id}")),
        },
    );
    frame.caller = Some(claim("owner", "ws-a"));
    frame
}

use super::*;

/// Runs `f` where Claude Code offers Bypass. Claude refuses to skip permission checks as root
/// outside a sandbox it recognizes, and a runner may be one; `IS_SANDBOX=1` is the sandbox it
/// recognizes, so a test of the Bypass paths runs them on every runner instead of passing on none.
pub(super) fn with_claude_bypass_offered<T>(f: impl FnOnce() -> T) -> T {
    struct Restore(Option<std::ffi::OsString>);
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: the caller holds the process-wide environment lock.
            unsafe {
                match self.0.take() {
                    Some(value) => std::env::set_var("IS_SANDBOX", value),
                    None => std::env::remove_var("IS_SANDBOX"),
                }
            }
        }
    }
    let _environment = crate::infra::config::env_lock();
    let _restore = Restore(std::env::var_os("IS_SANDBOX"));
    // SAFETY: this holds the process-wide environment lock.
    unsafe { std::env::set_var("IS_SANDBOX", "1") };
    assert!(
        crate::rc::harness::capability_of("claude-code")
            .permission_modes
            .contains(&crate::protocol::PermissionMode::Bypass)
    );
    f()
}

pub(super) fn claim(role: &str, ws: &str) -> crate::protocol::CallerClaim {
    crate::protocol::CallerClaim {
        account_id: Some("a".into()),
        username: None,
        role: role.into(),
        workspace_id: ws.into(),
    }
}

fn frame_with(caller: Option<crate::protocol::CallerClaim>, params: serde_json::Value) -> Frame {
    let mut f = Frame::request(method::SESSION_LIST, params);
    f.caller = caller;
    f
}

impl Daemon {
    pub(super) async fn resume_session(
        &mut self,
        p: SessionResume,
        caller: &crate::protocol::CallerClaim,
        frames: &mpsc::Sender<Frame>,
    ) -> Result<serde_json::Value, RpcError> {
        let prepared = self.prepare_resume_session(p, caller, frames, &Default::default())?;
        prepared.run_inline(self).await
    }

    pub(super) async fn start_session(
        &mut self,
        p: SessionStart,
        caller: &crate::protocol::CallerClaim,
        frames: &mpsc::Sender<Frame>,
    ) -> Result<serde_json::Value, RpcError> {
        let prepared = self.prepare_start_session(p, caller, frames, &Default::default())?;
        prepared.run_inline(self).await
    }

    pub(super) async fn spawn_session(
        &mut self,
        info: SessionInfo,
        spec: LaunchSpec,
        danger: danger::TranscriptDanger,
        frames: &mpsc::Sender<Frame>,
        prompt: Option<String>,
        attribution: MessageAttribution,
    ) -> Result<SessionInfo, SpawnFailure> {
        let prepared = self.prepare_spawn(info, spec, danger, frames, prompt, attribution)?;
        let result = prepared.execute().await;
        self.finish_spawn(prepared, result)
    }
}
