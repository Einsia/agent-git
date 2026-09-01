mod guards;
mod sessions;
mod stopping;
mod turns;

fn rpc_test_live(
    session_id: &str,
    generation: u64,
    tx: mpsc::Sender<Command>,
    mode: crate::protocol::PermissionMode,
) -> Live {
    Live {
        generation,
        info: SessionInfo {
            session_id: session_id.into(),
            workspace_id: "ws-a".into(),
            project_id: None,
            runtime: "codex".into(),
            agent: None,
            branch: None,
            status: SessionStatus::Running,
            last_seq: 0,
            gist: None,
            dangerous: false,
            permission_mode: Some(mode),
            created_at: "now".into(),
            updated_at: "now".into(),
        },
        tx,
        runtime_thread_id: None,
        task: tokio::spawn(async {}),
        danger_arm: 0,
        pending_mode: None,
        approval_session_modes: HashMap::new(),
        rpc_gate: Arc::new(Mutex::new(())),
        rpc_guard_sensitive: false,
        confirmed_turn_guards: Default::default(),
        inflight_turn_guard: None,
        restart_guard_attempts: Default::default(),
        restart_guard_mode: None,
        ended: false,
    }
}

fn rpc_test_daemon(sessions: HashMap<String, Live>, roster: Roster) -> Arc<Mutex<Daemon>> {
    let (notes, _notes_rx) = mpsc::channel(1);
    let (settlement, _) = tokio::sync::watch::channel(SettlementState::default());
    Arc::new(Mutex::new(Daemon {
        deferred: vec![],
        deferred_slot: None,
        replay_slots: Arc::new(tokio::sync::Semaphore::new(REPLAY_SLOTS)),
        outbound: None,
        opts: Options {
            hub: "https://hub.invalid".into(),
            token: "test".into(),
            connection_id: None,
        },
        journal: Journal::new(),
        mirror: Mirror::default(),
        roster,
        sessions,
        latest_session_generations: HashMap::new(),
        watches: HashMap::new(),
        terminals: HashMap::new(),
        terminal_delivery_blockers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        term_tx: None,
        online: true,
        connection_id: Some("conn".into()),
        secret_filter: Default::default(),
        settlement,
        started_at: std::time::Instant::now(),
        notes,
        grants: crate::rc::grants::Grants::default(),
        watch_generation: 0,
        session_generation: 2,
        confinement: HashMap::new(),
    }))
}

fn rpc_test_roster_entry(
    session_id: &str,
    mode: crate::protocol::PermissionMode,
) -> crate::rc::roster::Entry {
    crate::rc::roster::Entry {
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

fn rpc_turn_frame(session_id: &str) -> Frame {
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

fn claim(role: &str, ws: &str) -> crate::protocol::CallerClaim {
    crate::protocol::CallerClaim {
        account_id: Some("a".into()),
        role: role.into(),
        workspace_id: ws.into(),
    }
}

fn frame_with(caller: Option<crate::protocol::CallerClaim>, params: serde_json::Value) -> Frame {
    let mut f = Frame::request(method::SESSION_LIST, params);
    f.caller = caller;
    f
}
