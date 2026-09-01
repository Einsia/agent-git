use super::*;
use crate::protocol::CallerClaim;

fn claim(role: &str) -> CallerClaim {
    CallerClaim {
        account_id: Some("acct".into()),
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
                    session_id: "session-1".into(),
                    workspace_id: "ws1".into(),
                    project_id: Some("project-1".into()),
                    runtime: "claude-code".into(),
                    agent: None,
                    branch: None,
                    status: SessionStatus::AwaitingApproval,
                    last_seq: 0,
                    gist: None,
                    dangerous: false,
                    permission_mode: Some(crate::protocol::PermissionMode::Default),
                    created_at: "now".into(),
                    updated_at: "now".into(),
                };
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "session-1".into(),
                    roster::Entry {
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
                    tx: cmd_tx,
                    runtime_thread_id: Some("native-1".into()),
                    task: tokio::spawn(async {}),
                    danger_arm: 0,
                    pending_mode: None,
                    approval_session_modes: [(
                        "approval-1".to_string(),
                        crate::protocol::PermissionMode::Bypass,
                    )]
                    .into_iter()
                    .collect(),
                    rpc_gate: Arc::new(Mutex::new(())),
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
                    deferred: vec![],
                    deferred_slot: None,
                    replay_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(REPLAY_SLOTS)),
                    outbound: None,
                    opts: Options {
                        hub: "https://hub.invalid".into(),
                        token: "test".into(),
                        connection_id: None,
                    },
                    journal: Journal::new(),
                    mirror: Mirror::default(),
                    roster,
                    sessions: [("session-1".to_string(), live)].into_iter().collect(),
                    latest_session_generations: HashMap::new(),
                    watches: HashMap::new(),
                    terminals: HashMap::new(),
                    terminal_delivery_blockers: blockers,
                    term_tx: None,
                    online: false,
                    connection_id: None,
                    secret_filter: Default::default(),
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
