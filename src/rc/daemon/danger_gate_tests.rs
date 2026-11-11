use super::*;
use crate::protocol::CallerClaim;

/// A mode change from `from` to `mode`, judged as `session.setPermissionMode` judges it.
pub(super) fn require_owner_to_loosen(
    caller: Option<&CallerClaim>,
    mode: crate::protocol::PermissionMode,
    from: Option<crate::protocol::PermissionMode>,
) -> Result<(), RpcError> {
    require_owner_if_loosening(
        caller,
        from.map_or_else(|| mode.loosens_guard(), |from| mode.loosens_from(from)),
    )
}

fn claim(role: &str) -> CallerClaim {
    CallerClaim {
        account_id: Some("acct".into()),
        username: None,
        role: role.into(),
        workspace_id: "ws1".into(),
    }
}

/// Turning the checks off is owner-only; tightening them is anyone's.
///
/// This gate sits on the machine side and cannot rely on the hub alone — the trust boundary is
/// on the machine, and "the hub says he is the owner" is exactly what a compromised hub says. So
/// what is guarded here is **every** path into the dangerous state.
#[test]
fn only_the_owner_may_turn_the_checks_off() {
    use crate::protocol::PermissionMode as M;
    // Tightening is unrestricted: anyone can hit the brakes at any time.
    for role in ["viewer", "operator", "owner"] {
        assert!(
            require_owner_to_loosen(Some(&claim(role)), M::Plan, Some(M::Bypass)).is_ok(),
            "{role} must be able to tighten the guard"
        );
    }
    // Loosening to bypass is owner-only.
    assert!(require_owner_to_loosen(Some(&claim("owner")), M::Bypass, Some(M::Default)).is_ok());
    for role in ["viewer", "operator"] {
        assert!(
            require_owner_to_loosen(Some(&claim(role)), M::Bypass, Some(M::Default)).is_err(),
            "{role} must not be able to turn the checks off"
        );
    }

    // **A mode that is queued but not yet in effect counts too.**
    //
    // Codex switches modes through a sticky override carried by `turn/start`: a switch made
    // mid-run takes effect on the next turn. Look only at the mode already in effect and the
    // `plan` the owner just queued does not exist as far as authorization is concerned — the
    // operator asks for `default` right after, which against the old `auto` reads as a tightening
    // and is allowed; what it overwrites is exactly that `plan`, and the next turn can write
    // files again. The owner's tightening is silently undone by a request that looks stricter.
    {
        use crate::protocol::PermissionMode as P;
        // The baseline is the `plan` the owner queued, not the `auto` still in place.
        assert_eq!(
            authorization_baseline(Some(P::Auto), Some(P::Plan)),
            P::Plan
        );
        // So the operator's `default` becomes a loosening and is blocked.
        assert!(
            require_owner_to_loosen(
                Some(&claim("operator")),
                P::Default,
                Some(authorization_baseline(Some(P::Auto), Some(P::Plan))),
            )
            .is_err(),
            "operator must not overwrite the stricter mode the owner queued"
        );
        // The owner can still queue a second time.
        assert!(
            require_owner_to_loosen(
                Some(&claim("owner")),
                P::Default,
                Some(authorization_baseline(Some(P::Auto), Some(P::Plan))),
            )
            .is_ok()
        );
        // A pending mode that is **looser** must not become the baseline — that would relax
        // the test instead: a queued `bypass` must not turn "switch to `auto`" into a tightening.
        assert_eq!(
            authorization_baseline(Some(P::Default), Some(P::Bypass)),
            P::Default
        );
        // With no mode readable at all, the strictest one applies.
        assert_eq!(authorization_baseline(None, None), P::Plan);
        assert_eq!(authorization_baseline(None, Some(P::Bypass)), P::Plan);
    }

    // **Moving back is a tightening, even when it still lands somewhere loose.**
    //
    // Write the test as "the target mode is itself loose **or** it is looser than the current
    // one" and every target landing on `accept_edits` / `auto` / `bypass` is judged a loosening,
    // with no regard for the starting point. The cost is not one extra question but **the
    // feature is gone**: with the owner away, an operator cannot even tighten a runaway `bypass`
    // session down to `auto` — and "a runaway can still be reined in" is the entire reason the
    // operator role exists.
    for (from, to) in [
        (M::Bypass, M::Auto),
        (M::Bypass, M::AcceptEdits),
        (M::Auto, M::AcceptEdits),
    ] {
        for role in ["viewer", "operator", "owner"] {
            assert!(
                require_owner_to_loosen(Some(&claim(role)), to, Some(from)).is_ok(),
                "{from:?} → {to:?} is a tightening; {role} must not be blocked"
            );
        }
    }
    // The same pairs walked backwards are a loosening: owner only.
    for (from, to) in [
        (M::Auto, M::Bypass),
        (M::AcceptEdits, M::Auto),
        (M::AcceptEdits, M::Bypass),
    ] {
        assert!(require_owner_to_loosen(Some(&claim("owner")), to, Some(from)).is_ok());
        for role in ["viewer", "operator"] {
            assert!(
                require_owner_to_loosen(Some(&claim(role)), to, Some(from)).is_err(),
                "{from:?} → {to:?} is a loosening; {role} must not decide it alone"
            );
        }
    }
    // Staying put is not a loosening.
    for m in [M::Plan, M::Default, M::AcceptEdits, M::Auto, M::Bypass] {
        assert!(require_owner_to_loosen(Some(&claim("operator")), m, Some(m)).is_ok());
    }

    // **`plan → default` is a loosening too.**
    //
    // `plan` is the strictest mode (look, do not touch), while `default` allows editing files —
    // it only asks first. Looking at the target mode alone, `Default.loosens_guard()` is false,
    // so this loosening is judged a tightening and one frame from an operator lets a session the
    // owner deliberately pulled into `plan` back out. `resume_session` keeps that mode across a
    // restart precisely so this cannot happen.
    for role in ["viewer", "operator"] {
        assert!(
            require_owner_to_loosen(Some(&claim(role)), M::Default, Some(M::Plan)).is_err(),
            "{role} must not let a plan session back out"
        );
    }
    assert!(require_owner_to_loosen(Some(&claim("owner")), M::Default, Some(M::Plan)).is_ok());
    // The reverse is still a tightening: anyone can do it.
    assert!(require_owner_to_loosen(Some(&claim("operator")), M::Plan, Some(M::Default)).is_ok());
}

/// With no caller claim, the caller is treated as the **weakest** one.
///
/// An older hub does not send this field. Missing credentials must never read as permission —
/// the degradation points at "nobody gets bypass", never at "anybody does".
#[test]
fn a_missing_claim_is_never_read_as_permission() {
    assert!(require_owner_to_loosen(None, crate::protocol::PermissionMode::Bypass, None).is_err());
    // It must not stand in the way of tightening.
    assert!(require_owner_to_loosen(None, crate::protocol::PermissionMode::Plan, None).is_ok());
}

/// The approval side door must cross the exact same durable boundary as
/// session.setPermissionMode. This drives the real dispatch arm with a
/// deliberately unwritable sessions ledger and proves no supervisor
/// command can be observed on the other side.
#[test]
fn a_failed_danger_ledger_write_never_reaches_the_approval_driver() {
    let home = tempfile::tempdir().unwrap();
    // A directory at the final file path makes atomic rename fail even for
    // privileged test users; chmod-based fixtures are unreliable as root.
    std::fs::create_dir_all(home.path().join("rc/sessions.json")).unwrap();

    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (cmd_tx, mut cmd_rx) = mpsc::channel(1);
                let info = SessionInfo {
                    interrupt_fenced: None,
                    interrupted: None,
                    paused: None,
                    lease: None,
                    publication: None,
                    session_id: "session-1".into(),
                    native_source: None,
                    runtime_session_id: None,
                    workspace_id: "ws1".into(),
                    project_id: Some("project-1".into()),
                    runtime: "claude-code".into(),
                    agent: None,
                    branch: None,
                    status: SessionStatus::AwaitingApproval,
                    last_seq: 0,
                    gist: None,
                    title: None,
                    labels_withheld: None,
                    dangerous: false,
                    permission_mode: Some(crate::protocol::PermissionMode::Default),
                    created_at: "now".into(),
                    updated_at: "now".into(),
                };
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "session-1".into(),
                    roster::Entry {
                        native_source: None,
                        runtime: "claude-code".into(),
                        thread_id: "native-1".into(),
                        cwd: "/tmp".into(),
                        workspace_id: "ws1".into(),
                        project_id: Some("project-1".into()),
                        agit_session: None,
                        expected_agent_id: None,
                        permission_mode: Some(crate::protocol::PermissionMode::Default),
                        guard_attempts: Default::default(),
                        prior_threads: vec![],
                        ever_dangerous: false,
                        project_share: false,
                    },
                );
                let live = Live {
                    generation: 3,
                    info,
                    shared_executor: false,
                    started_process: true,
                    tx: cmd_tx,
                    runtime_thread_id: Some("native-1".into()),
                    task: tokio::spawn(async {}),
                    danger_arm: 0,
                    pending_mode: None,
                    approval_requests: [(
                        "approval-1".to_string(),
                        Some(crate::protocol::PermissionMode::Bypass),
                    )]
                    .into_iter()
                    .collect(),
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
                };
                let (notes, _notes_rx) = mpsc::channel(1);
                let (settlement, _) = tokio::sync::watch::channel(SettlementState::default());
                let blockers = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let mut daemon = Daemon {
                    identity: crate::rc::build_identity::DaemonIdentity::current().unwrap(),
                    deferred: vec![],
                    deferred_slot: None,
                    replay_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(REPLAY_SLOTS)),
                    outbound: None,
                    opts: Options {
                        local_owner: false,
                        hub: "https://hub.invalid".into(),
                    },
                    journal: Journal::new(),
                    mirror: Mirror::default(),
                    roster,
                    sessions: [("session-1".to_string(), live)].into_iter().collect(),
                    latest_session_generations: HashMap::new(),
                    opening_sessions: HashMap::new(),
                    exiting_launches: HashMap::new(),
                    archive_recovering: HashMap::new(),
                    watches: HashMap::new(),
                    terminals: HashMap::new(),
                    terminal_delivery_blockers: blockers,
                    term_tx: None,
                    online: false,
                    stop_cause: StopCause::Person,
                    replaced_by: None,

                    settlement,
                    started_at: std::time::Instant::now(),
                    notes,
                    grants: crate::rc::grants::Grants::default(),
                    watch_generation: 0,
                    session_generation: 3,
                    confinement: HashMap::new(),
                    leases: Default::default(),
                };
                let mut request = Frame::request(
                    method::APPROVAL_DECIDE,
                    serde_json::to_value(crate::protocol::ApprovalResponse {
                        approval_id: "approval-1".into(),
                        session_id: "session-1".into(),
                        decision: crate::protocol::ApprovalDecision::Allow,
                        scope: crate::protocol::ApprovalScope::Session,
                        message: None,
                        answers: None,
                        by: Some("owner".into()),
                    })
                    .unwrap(),
                );
                request.caller = Some(claim("owner"));
                let error = match daemon.prepare_session_rpc(&request) {
                    Ok(_) => panic!("an unwritable danger ledger must fail closed"),
                    Err(error) => error,
                };
                assert_eq!(error.code, ErrorCode::Internal as i32);
                assert!(matches!(
                    cmd_rx.try_recv(),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty)
                ));
                let live = daemon.sessions.get("session-1").unwrap();
                assert!(!live.info.dangerous, "in-memory arm must roll back too");
                assert_eq!(live.danger_arm, 0, "failed persistence owns no arm");
                assert!(
                    !daemon.roster.get("session-1").unwrap().ever_dangerous,
                    "a failed transaction must not poison the in-memory roster"
                );
            });
    });
}

/// Pins that a Claude Code transcript's own permission mode decides whether it ran unchecked,
/// whatever the ledger knows, for the session's marker. A conversation started in a terminal with
/// `--dangerously-skip-permissions` and closed before the daemon saw it has no ledger row; its
/// transcript records `bypassPermissions`, and that alone marks it, through `danger::authorize`
/// and `danger::judge` alike. A transcript that records no mode, or that cannot be found, counts
/// as unchecked too. One that records only checked modes is not marked, and another runtime's
/// unknown thread is judged by the ledger alone.
#[test]
fn a_claude_transcripts_own_bypass_mode_marks_it_as_run_unchecked() {
    if crate::rc::in_isolated_test(
        "rc::daemon::danger_gate_tests::a_claude_transcripts_own_bypass_mode_marks_it_as_run_unchecked",
    ) {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("CLAUDE_CONFIG_DIR", home.path().join("claude"));
        std::env::set_var("AGIT_HOME", home.path().join("agit"));
    }
    let cwd = std::fs::canonicalize(home.path()).unwrap();
    let cwd_text = cwd.to_string_lossy().into_owned();
    let native = home
        .path()
        .join("claude/projects")
        .join(crate::adapter::claude_code::slug_for(&cwd));
    std::fs::create_dir_all(&native).unwrap();
    let transcript = |id: &str, mode: Option<&str>| {
        let mut record = serde_json::json!({"type":"user","sessionId":id,"cwd":cwd_text,
            "timestamp":"2026-11-01T00:00:00Z","message":{"role":"user","content":"hello"}});
        if let Some(mode) = mode {
            record["permissionMode"] = mode.into();
        }
        std::fs::write(native.join(format!("{id}.jsonl")), format!("{record}\n")).unwrap();
    };
    const BYPASS: &str = "aaaaaaaa-0000-4000-8000-0000000002b1";
    const CHECKED: &str = "bbbbbbbb-0000-4000-8000-0000000002b2";
    const SILENT: &str = "cccccccc-0000-4000-8000-0000000002b3";
    const MISSING: &str = "dddddddd-0000-4000-8000-0000000002b4";
    transcript(BYPASS, Some("bypassPermissions"));
    transcript(CHECKED, Some("default"));
    transcript(SILENT, None);
    let roster = Roster::default();
    let authorize =
        |runtime: &str, thread: &str| danger::authorize(&roster, runtime, thread, "ws1", &cwd_text);

    for thread in [BYPASS, SILENT, MISSING] {
        assert!(
            authorize("claude-code", thread).ever_dangerous(),
            "{thread}"
        );
        assert!(
            danger::judge(&roster, "claude-code", thread, "ws1", &cwd_text).ever_dangerous(),
            "{thread}"
        );
    }
    assert!(!authorize("claude-code", CHECKED).ever_dangerous());
    assert!(!authorize("unsupported-test-runtime", MISSING).ever_dangerous());
}

fn caller_in(role: &str, workspace: &str) -> CallerClaim {
    CallerClaim {
        account_id: Some("acct".into()),
        username: None,
        role: role.into(),
        workspace_id: workspace.into(),
    }
}

/// Pins the check a source attach makes on the mode it opened in: it holds only when the runtime
/// confirms exactly the mode the launch asked for. A non-owner's source resume asks for the
/// native mode it found to be Default or Plan; an attach that then opened in Auto, or in a mode
/// it cannot confirm, is refused. A launch that asked for no mode is not held to one. A check that
/// passed every attach let a native thread loosened between the check and the attach be driven by
/// an operator.
#[test]
fn a_source_attach_holds_only_in_the_mode_it_asked_for() {
    use crate::protocol::PermissionMode as M;
    use crate::rc::supervisor::opened_as_asked;
    assert!(opened_as_asked(Some(M::Default), Some(M::Default)));
    assert!(opened_as_asked(Some(M::Plan), Some(M::Plan)));
    assert!(!opened_as_asked(Some(M::Default), Some(M::Auto)));
    assert!(!opened_as_asked(Some(M::Plan), Some(M::Default)));
    assert!(!opened_as_asked(Some(M::Default), None));
    assert!(opened_as_asked(None, None));
}

/// Pins that `attach_only` lets a takeover through to a Claude Code terminal registered for
/// native control: the owner's takeover with the flag reaches a launch, which attaches to the
/// running terminal, while the same conversation with no terminal registered is refused with
/// `not_running` and nothing reserved. `attach_only` widens nothing: an operator's takeover of the
/// registered terminal is still refused by the gate that keeps an open terminal's writer the
/// owner's, and nothing is reserved for it.
#[cfg(unix)]
#[test]
fn attach_only_joins_a_live_terminal() {
    if crate::rc::in_isolated_test(
        "rc::daemon::danger_gate_tests::attach_only_joins_a_live_terminal",
    ) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("AGIT_HOME", root.path().join("agit"));
        std::env::set_var("CLAUDE_CONFIG_DIR", root.path());
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let registered = crate::rc::native_claude::fixture::registered(root.path());
            let session = registered.registration.session.clone();
            let cwd = registered.registration.cwd.clone();
            let daemon = super::tests::rpc_test_daemon(HashMap::new(), Roster::default());
            let mut state = daemon.lock().await;
            state.mirror.bind("ws-a", "project-a", &cwd).unwrap();
            let (frames, _frames_rx) = mpsc::channel(1);
            let takeover = |thread: &str| {
                (
                    LocatedLocal {
                        title: None,
                        gist: None,
                        runtime: "claude-code".into(),
                        cwd: cwd.clone(),
                        project_id: Some("project-a".into()),
                        likely_active: true,
                    },
                    SessionResume {
                        project_id: None,
                        workspace_id: "ws-a".into(),
                        session_id: thread.into(),
                        prompt: None,
                        by: None,
                        agent: None,
                        expected_agent_id: None,
                        branch: None,
                        attach_only: true,
                    },
                )
            };
            let (located, resume) = takeover(&session);
            let error = state
                .prepare_local_takeover(located, resume, &caller_in("operator", "ws-a"), &frames)
                .err()
                .expect("an operator cannot attach to the owner's terminal");
            assert_eq!(
                error.code,
                ErrorCode::SessionBusy as i32,
                "{}",
                error.message
            );
            let reason = error.data.as_ref().and_then(|data| data["reason"].as_str());
            assert!(
                matches!(
                    reason,
                    Some("native_writer_active" | "native_recent_activity")
                ),
                "{:?}",
                error.data
            );
            assert!(state.opening_sessions.is_empty());
            assert!(state.sessions.is_empty());

            let (located, resume) = takeover(&session);
            let opening = state
                .prepare_local_takeover(located, resume, &caller_in("owner", "ws-a"), &frames)
                .unwrap_or_else(|error| panic!("{}", error.message));
            assert!(matches!(opening, SessionOpening::Launch(..)));
            state.opening_sessions.clear();

            let (mut located, resume) = takeover("bbbbbbbb-0000-4000-8000-000000000a02");
            located.likely_active = false;
            let error = state
                .prepare_local_takeover(located, resume, &caller_in("owner", "ws-a"), &frames)
                .err()
                .expect("no terminal runs this conversation");
            assert_eq!(
                error.data,
                Some(serde_json::json!({"retryable": false, "reason": "not_running"}))
            );
            assert!(state.opening_sessions.is_empty());
            drop(state);
            drop(registered);
        });
}

/// The terminal an attach-only takeover was admitted for exits before its driver is chosen: the
/// prepared opening refuses with `not_running` and launches nothing, instead of starting Claude
/// Code itself, and leaves nothing reserved.
#[cfg(unix)]
#[test]
fn attach_only_launches_nothing_when_the_terminal_exits_before_the_driver_is_chosen() {
    if crate::rc::in_isolated_test(
        "rc::daemon::danger_gate_tests::attach_only_launches_nothing_when_the_terminal_exits_before_the_driver_is_chosen",
    ) {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("AGIT_HOME", root.path().join("agit"));
        std::env::set_var("CLAUDE_CONFIG_DIR", root.path());
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let registered = crate::rc::native_claude::fixture::registered(root.path());
            let session = registered.registration.session.clone();
            let cwd = registered.registration.cwd.clone();
            let daemon = super::tests::rpc_test_daemon(HashMap::new(), Roster::default());
            let mut state = daemon.lock().await;
            state.mirror.bind("ws-a", "project-a", &cwd).unwrap();
            let (frames, _frames_rx) = mpsc::channel(1);
            let opening = state
                .prepare_local_takeover(
                    LocatedLocal {
                        title: None,
                        gist: None,
                        runtime: "claude-code".into(),
                        cwd: cwd.clone(),
                        project_id: Some("project-a".into()),
                        likely_active: true,
                    },
                    SessionResume {
                        project_id: None,
                        workspace_id: "ws-a".into(),
                        session_id: session.clone(),
                        prompt: None,
                        by: None,
                        agent: None,
                        expected_agent_id: None,
                        branch: None,
                        attach_only: true,
                    },
                    &caller_in("owner", "ws-a"),
                    &frames,
                )
                .unwrap_or_else(|error| panic!("{}", error.message));
            let SessionOpening::Launch(spawn, _) = opening else {
                panic!("the owner's attach-only takeover of a live terminal is admitted");
            };
            assert!(spawn.spec.attach_only);
            drop(state);

            // The terminal exits: its control endpoint closes and its registry record goes.
            drop(registered);
            let _ = std::fs::remove_file(
                cwd.join("sessions")
                    .join(format!("{}.json", std::process::id())),
            );
            let result = spawn.execute().await;
            let mut state = daemon.lock().await;
            let failure = state
                .finish_spawn(*spawn, result)
                .err()
                .expect("an attach-only opening never launches");
            assert!(!failure.reached_launch, "{}", failure.error.message);
            assert_eq!(
                failure.error.data,
                Some(serde_json::json!({"retryable": false, "reason": "not_running"})),
                "{}",
                failure.error.message
            );
            assert!(state.opening_sessions.is_empty());
            assert!(state.sessions.is_empty());
        });
}

/// #341: a colleague drives a Claude Code session that ran without permission checks, as the owner
/// does. One was started in bypassPermissions from its first prompt; the other's transcript
/// records no permissionMode, as a session started from the web may, which counts as unchecked.
/// Both are marked as having run unchecked, and a workspace Operator's turn still goes in. A gate
/// that kept such a session for the owner refused the Operator with `DangerousSessionLocked`.
#[tokio::test]
async fn a_colleague_drives_a_session_that_ran_without_checks() {
    use crate::protocol::PermissionMode;
    let home = tempfile::tempdir().unwrap();
    for (session, started, observed) in [
        (
            "bypass-started",
            PermissionMode::Bypass,
            Some(PermissionMode::Bypass),
        ),
        ("no-recorded-mode", PermissionMode::Default, None),
    ] {
        let (tx, _rx) = mpsc::channel(4);
        let mut live = super::tests::rpc_test_live(session, 1, tx, started);
        live.info.runtime = "claude-code".into();
        live.info.dangerous = started.is_dangerous();
        let daemon = super::tests::rpc_test_daemon(
            [(session.to_owned(), live)].into_iter().collect(),
            Roster::default(),
        );
        let mut state = daemon.lock().await;
        // What the harness reports of the transcript's mode marks the session; recording it to
        // disk needs a bound session, which this test doesn't have, and isn't what is tested.
        crate::rc::with_agit_home(home.path(), || {
            let _ = state.observe_native_settings(session, 1, observed);
        });
        assert!(state.sessions[session].info.dangerous, "{session}");
        for role in ["operator", "owner"] {
            assert!(
                state
                    .session_channel(session, &caller_in(role, "ws-a"), Need::Drive)
                    .is_ok(),
                "{session}: {role}"
            );
            let mut turn = super::tests::rpc_turn_frame(session);
            turn.caller = Some(caller_in(role, "ws-a"));
            if let Err(error) = state.prepare_session_rpc(&turn) {
                assert_ne!(
                    error.code,
                    ErrorCode::DangerousSessionLocked as i32,
                    "{session}: {role}: {}",
                    error.message
                );
            }
        }
    }
}
