use super::*;
use crate::protocol::CallerClaim;

fn claim(role: &str) -> CallerClaim {
    CallerClaim {
        account_id: Some("acct".into()),
        username: None,
        role: role.into(),
        workspace_id: "ws1".into(),
    }
}

/// Turning the checks off, or touching a session that has ever had them off, is owner-only.
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

    // Starting a new session (there is no current mode): only the absolute test applies —
    // coming from nothing is not a loosening, but asking for `bypass` up front is still the
    // owner's call.
    assert!(require_owner_to_loosen(Some(&claim("operator")), M::Default, None).is_ok());
    assert!(require_owner_to_loosen(Some(&claim("operator")), M::Plan, None).is_ok());
    assert!(require_owner_to_loosen(Some(&claim("operator")), M::Bypass, None).is_err());
}

/// A session that has **ever** run without approval stays owner-only, even once it is tightened
/// back.
///
/// Danger is a property of what the session has done, not of the mode it is in now — its context
/// may still hold what it read unreviewed at the time.
#[test]
fn a_session_that_was_ever_dangerous_stays_owner_only() {
    assert!(
        require_owner_to_drive(Some(&claim("operator")), true).is_err(),
        "tightening back to default must not clear the danger"
    );
    assert!(require_owner_to_drive(Some(&claim("owner")), true).is_ok());
}

/// The executor's current danger state must dominate a controller's cached safe snapshot.
#[cfg(unix)]
#[tokio::test]
async fn cloud_admin_with_operator_ceiling_cannot_drive_a_newly_dangerous_session() {
    use agit_peer::access::{Access, Policy, Principal, Resource, Rule};
    use serde_json::json;
    let principal = Principal {
        issuer: "https://cloud.example".into(),
        account_id: "admin".into(),
    };
    let registry = crate::rc::cloud::ingress::Registry::fixed(
        Policy::new(
            1,
            vec![Rule {
                principal: principal.clone(),
                resource: Resource::Machine,
                access: Access::Admin,
            }],
        )
        .unwrap(),
    );
    let client = registry.client(principal, i64::MAX);
    let list = Frame::request("session.list", json!({}));
    client.authorize(list.clone()).unwrap();
    client
        .project(
            &Frame::response(
                list.id.unwrap(),
                json!({
                    "local":[{"runtime_session_id":"native","runtime":"codex","cwd":"/project"}]
                }),
            )
            .to_json(),
        )
        .unwrap();
    let limited = client
        .authorize(Frame::request(
            "turn.start",
            json!({
                "session_id":"native","access_ceiling":"control"
            }),
        ))
        .unwrap();
    assert!(require_owner_to_drive(limited.caller.as_ref(), false).is_ok());
    assert!(require_owner_to_drive(limited.caller.as_ref(), true).is_err());
    let owner = client
        .authorize(Frame::request(
            "turn.start",
            json!({
                "session_id":"native"
            }),
        ))
        .unwrap();
    assert!(require_owner_to_drive(owner.caller.as_ref(), true).is_ok());
}

/// With no caller claim, the caller is treated as the **weakest** one.
///
/// An older hub does not send this field. Missing credentials must never read as permission —
/// the degradation points at "nobody gets bypass", never at "anybody does".
#[test]
fn a_missing_claim_is_never_read_as_permission() {
    assert!(require_owner_to_loosen(None, crate::protocol::PermissionMode::Bypass, None).is_err());
    assert!(require_owner_to_drive(None, true).is_err());
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
                    },
                );
                let live = Live {
                    generation: 3,
                    info,
                    shared_executor: false,
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
                    rpc_gate: Arc::new(Mutex::new(())),
                    interrupt_gate: Arc::new(Mutex::new(())),
                    approval_gate: Arc::new(Mutex::new(())),
                    rpc_guard_sensitive: false,
                    confirmed_turn_guards: Default::default(),
                    inflight_turn_guard: None,
                    restart_guard_attempts: Default::default(),
                    restart_guard_mode: None,
                    ended: false,
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

                    settlement,
                    started_at: std::time::Instant::now(),
                    notes,
                    grants: crate::rc::grants::Grants::default(),
                    watch_generation: 0,
                    session_generation: 3,
                    confinement: HashMap::new(),
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

/// Pins the mode a resume starts in by caller: the owner gets the last mode back, Auto
/// included; anyone else starts in Default, or Plan when Plan was last, and never Auto or
/// AcceptEdits. A daemon that restored the last mode for every caller would let a non-owner
/// run tools the owner had stopped approving.
#[test]
fn a_non_owner_resume_starts_no_looser_than_default() {
    use crate::protocol::PermissionMode as M;
    let (owner, operator) = (claim("owner"), claim("operator"));
    assert_eq!(danger::resume_mode(&owner, Some(M::Auto)), Some(M::Auto));
    assert_eq!(danger::resume_mode(&owner, None), None);
    for (last, started) in [
        (Some(M::Auto), M::Default),
        (Some(M::AcceptEdits), M::Default),
        (Some(M::Default), M::Default),
        (Some(M::Plan), M::Plan),
        (None, M::Default),
    ] {
        assert_eq!(
            danger::resume_mode(&operator, last),
            Some(started),
            "{last:?}"
        );
    }
}

/// Pins that a Claude Code transcript's own permission mode decides whether it ran unchecked,
/// whatever the ledger knows. A conversation started in a terminal with
/// `--dangerously-skip-permissions` and closed before the daemon saw it has no ledger row; its
/// transcript records `bypassPermissions`, and that alone makes it owner-only to drive: the
/// takeover, the logical-id resume and the Claude `session.enqueue` gate all ask through
/// `danger::authorize`, and `session.watch` through `danger::judge`. A transcript that records no
/// mode, or that cannot be found, counts as unchecked too. One that records only checked modes
/// is open to an operator, and another runtime's unknown thread is judged by the ledger alone, as
/// before. A judgment that read only the ledger lets the operator through on the bypass
/// transcript.
#[test]
fn a_claude_transcripts_own_bypass_mode_keeps_it_owner_only() {
    if crate::rc::in_isolated_test(
        "rc::daemon::danger_gate_tests::a_claude_transcripts_own_bypass_mode_keeps_it_owner_only",
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
    let (owner, operator) = (claim("owner"), claim("operator"));
    let authorize = |caller: &CallerClaim, runtime: &str, thread: &str| {
        danger::authorize(&roster, caller, runtime, thread, "ws1", &cwd_text)
    };

    for thread in [BYPASS, SILENT, MISSING] {
        let error = authorize(&operator, "claude-code", thread).expect_err(thread);
        assert_eq!(
            error.code,
            ErrorCode::DangerousSessionLocked as i32,
            "{thread}"
        );
        let allowed = authorize(&owner, "claude-code", thread).expect("the owner drives it");
        assert!(allowed.ever_dangerous(), "{thread}");
        assert!(
            danger::judge(&roster, "claude-code", thread, "ws1", &cwd_text).ever_dangerous(),
            "{thread}"
        );
    }
    assert!(
        !authorize(&operator, "claude-code", CHECKED)
            .unwrap()
            .ever_dangerous()
    );
    assert!(
        !authorize(&operator, "unsupported-test-runtime", MISSING)
            .unwrap()
            .ever_dangerous()
    );
}

fn caller_in(role: &str, workspace: &str) -> CallerClaim {
    CallerClaim {
        account_id: Some("acct".into()),
        username: None,
        role: role.into(),
        workspace_id: workspace.into(),
    }
}

/// Pins the gate on messages sent to a live terminal Claude Code session (`session.enqueue`)
/// against the transcript's own mode. The terminal runs in a folder of the workspace and the
/// ledger has no row for it. While its transcript records `bypassPermissions`, an operator's
/// message is refused with `DangerousSessionLocked` and the owner's passes the gate; once it
/// records only checked modes, the operator's passes too. A gate that read only the ledger let
/// the operator's message through to a session that ran with no checks.
#[cfg(unix)]
#[test]
fn a_message_to_a_bypass_terminal_is_owner_only() {
    if crate::rc::in_isolated_test(
        "rc::daemon::danger_gate_tests::a_message_to_a_bypass_terminal_is_owner_only",
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
            let native = root
                .path()
                .join("projects")
                .join(crate::adapter::claude_code::slug_for(&cwd));
            std::fs::create_dir_all(&native).unwrap();
            let write = |mode: &str| {
                let record = serde_json::json!({"type":"user","sessionId":session,"cwd":cwd,
                    "timestamp":"2026-11-01T00:00:00Z","permissionMode":mode,
                    "message":{"role":"user","content":"hello"}});
                std::fs::write(
                    native.join(format!("{session}.jsonl")),
                    format!("{record}\n"),
                )
                .unwrap();
            };
            let daemon = super::tests::rpc_test_daemon(HashMap::new(), Roster::default());
            let mut state = daemon.lock().await;
            state.mirror.bind("ws-a", "project-a", &cwd).unwrap();
            let local = || -> crate::protocol::LocalSession {
                serde_json::from_value(serde_json::json!({"runtime_session_id":session,
                    "runtime":"claude-code","cwd":cwd,"modified_at":"2026-11-01T00:00:00Z",
                    "likely_active":true}))
                .unwrap()
            };
            let enqueue = |role: &str| {
                let mut frame = Frame::request(
                    method::SESSION_ENQUEUE,
                    serde_json::json!({"workspace_id":"ws-a","session_id":session,
                        "client_msg_id":uuid::Uuid::new_v4().to_string(),"message":"hello"}),
                );
                frame.caller = Some(caller_in(role, "ws-a"));
                frame
            };
            let refused = |result: Result<_, RpcError>| {
                result
                    .err()
                    .is_some_and(|error| error.code == ErrorCode::DangerousSessionLocked as i32)
            };

            write("bypassPermissions");
            assert!(refused(
                state.prepare_native_inbox(&enqueue("operator"), Some(local()))
            ));
            assert!(!refused(
                state.prepare_native_inbox(&enqueue("owner"), Some(local()))
            ));
            write("default");
            assert!(!refused(
                state.prepare_native_inbox(&enqueue("operator"), Some(local()))
            ));
            drop(registered);
        });
}

/// Pins that attaching to a terminal Claude Code session stays fail-closed. The native driver
/// cannot know what mode the terminal runs in, so the session it opens records no mode and
/// counts as dangerous: an operator's turn or steer into it is refused with
/// `DangerousSessionLocked`, and the owner's goes through. This holds whatever the terminal's
/// transcript says, since the terminal may change mode without recording it. A driver that
/// reported a known Default mode would let the operator drive a terminal that may run with no
/// checks.
#[cfg(unix)]
#[test]
fn an_attached_terminal_stays_owner_only_to_drive() {
    if crate::rc::in_isolated_test(
        "rc::daemon::danger_gate_tests::an_attached_terminal_stays_owner_only_to_drive",
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
            let driver = crate::rc::harness::claude_native::ClaudeNativeDriver::attach(
                crate::rc::harness::LaunchSpec {
                    cwd: registered.registration.cwd.clone(),
                    resume_from: Some(registered.registration.session.clone()),
                    agit_session: None,
                    model: None,
                    dangerous: false,
                    permission_mode: None,
                },
            )
            .map_err(|error| format!("{error:?}"))
            .expect("the driver attaches to the registered terminal");
            let driver = crate::rc::harness::AnyDriver::ClaudeNative(Box::new(driver));
            let (tx, _rx) = mpsc::channel(1);
            let mut live = super::tests::rpc_test_live(
                "agit-attached",
                1,
                tx,
                crate::protocol::PermissionMode::Default,
            );
            live.info.runtime = "claude-code".into();
            live.info.dangerous = false;
            crate::rc::supervisor::record_opening_mode(&driver, &mut live.info);
            assert_eq!(live.info.permission_mode, None);
            assert!(live.info.dangerous);

            let daemon = super::tests::rpc_test_daemon(
                HashMap::from([("agit-attached".to_owned(), live)]),
                Roster::default(),
            );
            let state = daemon.lock().await;
            let error = state
                .session_channel("agit-attached", &caller_in("operator", "ws-a"), Need::Drive)
                .err()
                .expect("an operator cannot drive an attached terminal");
            assert_eq!(error.code, ErrorCode::DangerousSessionLocked as i32);
            assert!(
                state
                    .session_channel("agit-attached", &caller_in("owner", "ws-a"), Need::Drive)
                    .is_ok()
            );
            drop(state);
            drop(driver);
            drop(registered);
        });
}

/// Pins that this build advertises `resume-caller-gate-v1` in the identity the daemon reports
/// (`rpc_features`). The Hub lifts its stopgap for an executor that advertises it, so it sits
/// beside the tests of the four parts it promises, all in this module and in the takeover test:
/// the transcript's own mode on takeover and resume, the same read on a native message, a
/// non-owner's resume no looser than Default, and attach staying fail-closed. A build that
/// dropped any of them breaks its test here; one that dropped the advert would leave executors
/// that enforce them behind the Hub's stopgap.
#[test]
fn the_resume_caller_gate_is_advertised() {
    let identity = crate::rc::build_identity::DaemonIdentity::current().unwrap();
    assert!(
        identity
            .rpc_features
            .iter()
            .any(|feature| feature == "resume-caller-gate-v1"),
        "{:?}",
        identity.rpc_features
    );
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
