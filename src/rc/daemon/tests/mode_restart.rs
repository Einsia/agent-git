use super::super::mode_restart::ModeRestart;
use super::*;
use crate::protocol::PermissionMode;

const THREAD: &str = "11111111-2222-4333-8444-555555555555";

/// `session-a`, a Claude Code conversation in Default with its native id recorded, so a mode
/// change for it is prepared at once.
fn claude_daemon(
    status: SessionStatus,
    bypass_needs_restart: bool,
) -> (Arc<Mutex<Daemon>>, mpsc::Receiver<Command>) {
    let (tx, commands) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
    let mut live = rpc_test_live("session-a", 1, tx, PermissionMode::Default);
    live.info.runtime = "claude-code".into();
    live.info.status = status;
    live.runtime_thread_id = Some(THREAD.into());
    live.bypass_needs_restart = bypass_needs_restart;
    let roster: Roster = serde_json::from_value(serde_json::json!({
        "sessions":{"session-a":{"runtime":"claude-code", "thread_id":THREAD,
            "cwd":"/tmp", "workspace_id":"ws-a", "permission_mode":"default"}}
    }))
    .unwrap();
    let daemon = rpc_test_daemon(HashMap::from([("session-a".into(), live)]), roster);
    (daemon, commands)
}

fn prepare(daemon: &mut Daemon, frame: &Frame) -> Result<PreparedSessionRpc, RpcError> {
    with_claude_bypass_offered(|| match daemon.prepare_session_rpc_or_wait(frame)? {
        SessionRpcPreparation::Ready(prepared) => Ok(*prepared),
        SessionRpcPreparation::AwaitingDurableGuardRow(_) => {
            panic!("waited for a native id that is recorded")
        }
    })
}

/// The owner's switch to Bypass on a Claude Code process launched with permission checks sends
/// that process nothing it would refuse: between turns it becomes a restart in Bypass through the
/// resume path, and instructions for the conversation wait for it from then on. The danger bit is
/// left to the launch, which records it before anything runs unchecked. A restart dropped unrun
/// releases what waits on it.
#[tokio::test]
async fn an_idle_claude_conversation_restarts_into_bypass() {
    let (daemon, mut commands) = claude_daemon(SessionStatus::Idle, true);
    let prepared = prepare(
        &mut *daemon.lock().await,
        &rpc_mode_frame(PermissionMode::Bypass),
    )
    .unwrap();
    let SessionRpcOperation::RestartInMode {
        mode,
        resume,
        caller,
        ..
    } = &prepared.operation
    else {
        panic!("a live switch was prepared for a process that refuses it");
    };
    assert_eq!(*mode, PermissionMode::Bypass);
    assert_eq!(
        (resume.session_id.as_str(), resume.permission_mode),
        ("session-a", Some(PermissionMode::Bypass))
    );
    assert!(!resume.attach_only && resume.prompt.is_none());
    assert!(caller.is_owner());
    assert!(
        commands.try_recv().is_err(),
        "the process was sent nothing yet"
    );
    {
        let mut state = daemon.lock().await;
        assert!(state.mode_restart_of("session-a").is_some());
        assert!(!state.roster.sessions["session-a"].ever_dangerous);
        assert!(!state.sessions["session-a"].rpc_guard_sensitive);
    }
    drop(prepared);
    let mut state = daemon.lock().await;
    assert!(state.mode_restart_of("session-a").is_none());
    assert!(state.mode_restarts.is_empty());
}

/// A process launched in Bypass, or a switch to any other mode, is switched live as before.
#[tokio::test]
async fn only_bypass_on_a_checked_process_takes_a_restart() {
    for (mode, needs_restart) in [
        (PermissionMode::Bypass, false),
        (PermissionMode::Plan, true),
        (PermissionMode::Auto, true),
    ] {
        let (daemon, _commands) = claude_daemon(SessionStatus::Idle, needs_restart);
        let mut state = daemon.lock().await;
        // Bypass arms the durable danger bit before the live switch, which writes the roster.
        let home = tempfile::tempdir().unwrap();
        let prepared =
            crate::rc::with_agit_home(home.path(), || prepare(&mut state, &rpc_mode_frame(mode)))
                .unwrap();
        assert!(
            matches!(
                prepared.operation,
                SessionRpcOperation::SetPermissionMode { .. }
            ),
            "{mode:?}"
        );
        assert!(state.mode_restarts.is_empty(), "{mode:?}");
    }
}

/// While a turn runs, a decision waits, or a terminal is taking the conversation over, the switch
/// is refused with a sentence that says why. Nothing is cut short: the process is sent nothing,
/// and nothing waits on a restart.
#[tokio::test]
async fn a_switch_to_bypass_never_cuts_a_turn_short() {
    for status in [SessionStatus::Running, SessionStatus::AwaitingApproval] {
        let (daemon, mut commands) = claude_daemon(status, true);
        let mut state = daemon.lock().await;
        let error = prepare(&mut state, &rpc_mode_frame(PermissionMode::Bypass))
            .err()
            .expect("the switch was prepared");
        assert!(
            error.message.contains("a turn is running"),
            "{status:?}: {}",
            error.message
        );
        assert_eq!(error.data.as_ref().unwrap()["retryable"], false);
        assert!(commands.try_recv().is_err(), "{status:?}");
        assert!(state.mode_restarts.is_empty(), "{status:?}");
    }

    let (daemon, mut commands) = claude_daemon(SessionStatus::Idle, true);
    let mut state = daemon.lock().await;
    state
        .sessions
        .get_mut("session-a")
        .unwrap()
        .approval_requests
        .insert("approval-1".into(), None);
    let error = prepare(&mut state, &rpc_mode_frame(PermissionMode::Bypass))
        .err()
        .expect("the switch was prepared");
    assert!(
        error.message.contains("a turn is running"),
        "{}",
        error.message
    );
    assert!(commands.try_recv().is_err());
    assert!(state.mode_restarts.is_empty());

    // An unconfirmed permission change brings the conversation back only in Plan.
    let (daemon, mut commands) = claude_daemon(SessionStatus::Idle, true);
    let mut state = daemon.lock().await;
    state
        .roster
        .sessions
        .get_mut("session-a")
        .unwrap()
        .guard_attempts
        .insert(
            "guard-1".into(),
            roster::GuardAttempt {
                expected_mode: PermissionMode::Plan,
                observed: false,
            },
        );
    let error = prepare(&mut state, &rpc_mode_frame(PermissionMode::Bypass))
        .err()
        .expect("the switch was prepared");
    assert!(
        error.message.contains("still being confirmed"),
        "{}",
        error.message
    );
    assert!(commands.try_recv().is_err());
    assert!(state.mode_restarts.is_empty());
}

/// A terminal opening the conversation takes it over from the daemon's process; a restart then
/// would launch a second writer beside it, so the switch waits for the handover.
#[cfg(unix)]
#[test]
fn a_switch_to_bypass_waits_for_a_terminal_taking_over() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (daemon, mut commands) = claude_daemon(SessionStatus::Idle, true);
                let mut state = daemon.lock().await;
                state.terminal_opened(THREAD, std::process::id(), true, None);
                assert!(state.leases.releasing(THREAD));
                while commands.try_recv().is_ok() {}
                let error = prepare(&mut state, &rpc_mode_frame(PermissionMode::Bypass))
                    .err()
                    .expect("the switch was prepared");
                assert!(
                    error
                        .message
                        .contains("a terminal is taking this conversation over"),
                    "{}",
                    error.message
                );
                assert!(commands.try_recv().is_err());
                assert!(state.mode_restarts.is_empty());
            })
    });
}

/// A colleague's switch to Bypass is refused whether or not the process could take it live: the
/// daemon's owner check stands even where Claude itself would accept the switch.
#[tokio::test]
async fn a_colleague_cannot_turn_on_bypass_even_where_claude_would_take_it() {
    for needs_restart in [false, true] {
        let (daemon, mut commands) = claude_daemon(SessionStatus::Idle, needs_restart);
        let mut state = daemon.lock().await;
        let mut frame = rpc_mode_frame(PermissionMode::Bypass);
        frame.caller = Some(claim("operator", "ws-a"));
        let error = prepare(&mut state, &frame)
            .err()
            .expect("the switch was prepared");
        assert_eq!(
            error.code,
            ErrorCode::DangerousSessionLocked as i32,
            "{needs_restart}"
        );
        assert!(commands.try_recv().is_err(), "{needs_restart}");
        assert!(state.mode_restarts.is_empty(), "{needs_restart}");
        assert!(!state.roster.sessions["session-a"].ever_dangerous);
    }
}

/// An instruction that arrives while its conversation restarts waits for it, then goes to the
/// conversation as it runs then, rather than being answered busy or "no such session": a page
/// told either sends again, and one message becomes several.
#[tokio::test]
async fn an_instruction_waits_for_its_conversation_to_restart() {
    let (daemon, mut commands) = claude_daemon(SessionStatus::Idle, false);
    let (done, waiting) = tokio::sync::watch::channel(false);
    let epoch = {
        let mut state = daemon.lock().await;
        state.mode_restarts.insert(
            "session-a".into(),
            ModeRestart::waiting_on(waiting.clone()).0,
        );
        state.settlement.borrow().epoch
    };
    let (outbound, _replies) = crate::rc::outbound::channel();
    let (_stop_tx, stop) = tokio::sync::watch::channel(false);
    let held = tokio::spawn(super::super::mode_restart::serve_after_restart(
        daemon.clone(),
        outbound,
        crate::protocol::RequestId::Num(7),
        rpc_turn_frame("session-a"),
        epoch,
        waiting,
        stop,
    ));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        commands.try_recv().is_err(),
        "nothing went while it restarted"
    );
    daemon.lock().await.mode_restarts.remove("session-a");
    done.send(true).unwrap();
    let command = tokio::time::timeout(std::time::Duration::from_secs(5), commands.recv())
        .await
        .expect("the instruction went once the restart finished")
        .unwrap();
    assert!(matches!(command, Command::Turn { .. }));
    held.abort();
}

/// An instruction waiting on a restart that is dropped unrun goes as soon as it is dropped.
#[tokio::test]
async fn a_dropped_restart_releases_what_waits_on_it() {
    let (daemon, mut commands) = claude_daemon(SessionStatus::Idle, false);
    let (done, waiting) = tokio::sync::watch::channel(false);
    let epoch = {
        let mut state = daemon.lock().await;
        state.mode_restarts.insert(
            "session-a".into(),
            ModeRestart::waiting_on(waiting.clone()).0,
        );
        state.settlement.borrow().epoch
    };
    let (outbound, _replies) = crate::rc::outbound::channel();
    let (_stop_tx, stop) = tokio::sync::watch::channel(false);
    let held = tokio::spawn(super::super::mode_restart::serve_after_restart(
        daemon.clone(),
        outbound,
        crate::protocol::RequestId::Num(8),
        rpc_turn_frame("session-a"),
        epoch,
        waiting,
        stop,
    ));
    drop(done);
    let command = tokio::time::timeout(std::time::Duration::from_secs(5), commands.recv())
        .await
        .expect("a dropped restart released the instruction")
        .unwrap();
    assert!(matches!(command, Command::Turn { .. }));
    assert!(daemon.lock().await.mode_restarts.is_empty());
    held.abort();
}

/// A person's stop while the conversation restarts wins. With its old process already gone, the
/// stop is answered as stopping and reaches the restart; a restart told to stop before it launches
/// leaves the conversation stopped in its old mode, launching nothing, and says so.
#[tokio::test]
async fn a_stop_during_a_restart_keeps_the_conversation_stopped() {
    let (daemon, _commands) = claude_daemon(SessionStatus::Idle, true);
    let (done, waiting) = tokio::sync::watch::channel(false);
    let stopped = {
        let mut state = daemon.lock().await;
        // The old process has ended; the new one is not up yet.
        state.sessions.clear();
        let (restart, stopped) = ModeRestart::waiting_on(waiting);
        state.mode_restarts.insert("session-a".into(), restart);
        let (frames, _frames_rx) = mpsc::channel(4);
        let mut stop = Frame::request(
            method::SESSION_STOP,
            serde_json::json!({"session_id":"session-a"}),
        );
        stop.caller = Some(claim("operator", "ws-a"));
        let reply = state.dispatch(&stop, &frames).await.unwrap();
        assert_eq!(reply["stopping"], true);
        stopped
    };
    assert!(*stopped.borrow(), "the stop reached the restart");

    let (frames, _frames_rx) = mpsc::channel(4);
    daemon.lock().await.session_frames = Some(frames);
    let (_stop_tx, mut daemon_stop) = tokio::sync::watch::channel(false);
    let error = super::super::mode_restart::restart_in_mode(
        daemon.clone(),
        "session-a",
        1,
        PermissionMode::Bypass,
        SessionResume {
            workspace_id: "ws-a".into(),
            session_id: "session-a".into(),
            project_id: None,
            prompt: None,
            by: None,
            agent: None,
            expected_agent_id: None,
            branch: None,
            attach_only: false,
            permission_mode: Some(PermissionMode::Bypass),
        },
        claim("owner", "ws-a"),
        Default::default(),
        stopped,
        &mut daemon_stop,
    )
    .await
    .err()
    .expect("a stopped conversation was brought back");
    assert_eq!(error.data.as_ref().unwrap()["reason"], "stopped");
    assert!(
        error.message.contains("mode is unchanged"),
        "{}",
        error.message
    );
    let state = daemon.lock().await;
    assert!(state.sessions.is_empty() && state.opening_sessions.is_empty());
    assert_eq!(
        state.roster.sessions["session-a"].permission_mode,
        Some(PermissionMode::Default)
    );
    drop(done);
}

/// A restart still waiting for its old process to end stops waiting when a person stops it.
#[tokio::test]
async fn a_stop_ends_a_restart_still_waiting_for_its_old_process() {
    let (daemon, _commands) = claude_daemon(SessionStatus::Idle, true);
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let (_stop_tx, mut daemon_stop) = tokio::sync::watch::channel(false);
    let restart = tokio::spawn({
        let daemon = daemon.clone();
        async move {
            super::super::mode_restart::restart_in_mode(
                daemon,
                "session-a",
                1,
                PermissionMode::Bypass,
                SessionResume {
                    workspace_id: "ws-a".into(),
                    session_id: "session-a".into(),
                    project_id: None,
                    prompt: None,
                    by: None,
                    agent: None,
                    expected_agent_id: None,
                    branch: None,
                    attach_only: false,
                    permission_mode: Some(PermissionMode::Bypass),
                },
                claim("owner", "ws-a"),
                Default::default(),
                stopped,
                &mut daemon_stop,
            )
            .await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !restart.is_finished(),
        "it waits while the old process runs"
    );
    stop.send(true).unwrap();
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), restart)
        .await
        .expect("the stop ended the wait")
        .unwrap()
        .err()
        .expect("a stopped conversation was brought back");
    assert_eq!(error.data.as_ref().unwrap()["reason"], "stopped");
}

/// Viewers see the stopped process end as restarting, not as ended, while the restart is under
/// way; an ended process with no restart is reported as before.
#[tokio::test]
async fn viewers_see_the_old_process_end_as_restarting() {
    let (daemon, _commands) = claude_daemon(SessionStatus::Idle, true);
    let mut state = daemon.lock().await;
    let ended = || {
        let mut frame = Frame::notification(
            method::SESSION_STATUS,
            serde_json::json!({"session_id":"session-a", "status": SessionStatus::Ended}),
        );
        frame.stream = Some("session-a".into());
        frame
    };
    let plain = state.project_session_frame(ended()).unwrap();
    assert!(plain.params.unwrap().get("reason").is_none());

    let (done, waiting) = tokio::sync::watch::channel(false);
    state
        .mode_restarts
        .insert("session-a".into(), ModeRestart::waiting_on(waiting).0);
    let restarting = state.project_session_frame(ended()).unwrap();
    let params: crate::protocol::SessionStatusChanged =
        serde_json::from_value(restarting.params.unwrap()).unwrap();
    assert_eq!(params.status, SessionStatus::Ended);
    assert_eq!(params.reason.as_deref(), Some("restarting"));
    drop(done);
}

/// Admission that a test revokes.
struct Toggle(Arc<std::sync::RwLock<bool>>);
impl crate::rc::authority::Authority for Toggle {
    fn admit(&self, accept: &mut dyn FnMut() -> bool) -> bool {
        let allowed = self.0.read().unwrap();
        *allowed && accept()
    }
}

/// A switch prepared under an admission that is revoked before it runs (the connection dropped,
/// the grant expired, control was taken away) stops nothing and launches nothing; what waits on
/// the restart is released.
#[tokio::test]
async fn a_revoked_switch_stops_and_launches_nothing() {
    let (daemon, mut commands) = claude_daemon(SessionStatus::Idle, true);
    let allowed = Arc::new(std::sync::RwLock::new(true));
    let mut frame = rpc_mode_frame(PermissionMode::Bypass);
    frame.authority = crate::rc::authority::Guard::new(Toggle(allowed.clone()));
    let prepared = prepare(&mut *daemon.lock().await, &frame).unwrap();
    assert!(matches!(
        prepared.operation,
        SessionRpcOperation::RestartInMode { .. }
    ));
    *allowed.write().unwrap() = false;
    let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
    let executed = prepared.execute(daemon.clone(), &mut stop).await;
    assert!(executed.response.is_err());
    assert!(commands.try_recv().is_err(), "the process was not stopped");
    let state = daemon.lock().await;
    assert!(state.mode_restarts.is_empty());
    assert!(state.sessions.contains_key("session-a"));
    assert!(state.opening_sessions.is_empty());
}

/// The restart checks the switch's admission again before it brings the conversation back: one
/// revoked while the old process ended launches nothing.
#[tokio::test]
async fn a_restart_whose_admission_was_revoked_launches_nothing() {
    let (daemon, _commands) = claude_daemon(SessionStatus::Idle, true);
    let (frames, _frames_rx) = mpsc::channel(4);
    {
        let mut state = daemon.lock().await;
        state.sessions.clear();
        state.session_frames = Some(frames);
    }
    let allowed = Arc::new(std::sync::RwLock::new(false));
    let (_person, stopped) = tokio::sync::watch::channel(false);
    let (_stop_tx, mut daemon_stop) = tokio::sync::watch::channel(false);
    let error = super::super::mode_restart::restart_in_mode(
        daemon.clone(),
        "session-a",
        1,
        PermissionMode::Bypass,
        SessionResume {
            workspace_id: "ws-a".into(),
            session_id: "session-a".into(),
            project_id: None,
            prompt: None,
            by: None,
            agent: None,
            expected_agent_id: None,
            branch: None,
            attach_only: false,
            permission_mode: Some(PermissionMode::Bypass),
        },
        claim("owner", "ws-a"),
        crate::rc::authority::Guard::new(Toggle(allowed)),
        stopped,
        &mut daemon_stop,
    )
    .await
    .err()
    .expect("a revoked restart launched");
    assert_eq!(error.data.as_ref().unwrap()["reason"], "restart_failed");
    let state = daemon.lock().await;
    assert!(state.sessions.is_empty() && state.opening_sessions.is_empty());
}
