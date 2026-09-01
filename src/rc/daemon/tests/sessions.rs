use super::*;

#[test]
fn watch_danger_uses_the_logical_roster_identity_and_workspace() {
    let mut roster = crate::rc::roster::Roster::default();
    roster.record(
        "agit-logical",
        crate::rc::roster::Entry {
            runtime: "claude-code".into(),
            thread_id: "native-1".into(),
            cwd: "/tmp/project".into(),
            workspace_id: "ws-a".into(),
            project_id: None,
            agit_session: None,
            expected_agent_id: None,
            permission_mode: None,
            guard_attempts: Default::default(),
            prior_threads: vec![],
            ever_dangerous: true,
        },
    );

    assert!(roster.transcript_ever_dangerous("claude-code", "native-1", "ws-a", "/tmp/project"));
    // When a second workspace binds the same directory, that side sees the same transcript, so
    // the monotonic danger bit follows the **transcript**. Asserting `false` here is exactly the
    // path by which ws-b's operator takes over a conversation that ran with no checks and mints
    // a clean identity for it.
    assert!(roster.transcript_ever_dangerous("claude-code", "native-1", "ws-b", "/tmp/project"));
    // The workspace is still part of the test: a different workspace and a different directory
    // together are another territory.
    assert!(!roster.transcript_ever_dangerous(
        "claude-code",
        "native-1",
        "ws-b",
        "/tmp/other-project"
    ));

    let lost = crate::rc::roster::Roster {
        history_lost: true,
        ..crate::rc::roster::Roster::default()
    };
    assert!(lost.transcript_ever_dangerous(
        "claude-code",
        "unknown-native",
        "ws-a",
        "/tmp/project"
    ));
}

/// A session that starts dangerous and crashes before `Bound` reaches disk leaves one ledger row
/// with an empty thread id, and nobody knows the real thread id of that transcript. Every thread
/// the ledger cannot account for — same runtime, same workspace **or same directory** — must be
/// treated as dangerous; otherwise one takeover by thread id mints a clean identity that inherits
/// everything that unchecked run read into its context.
#[test]
fn an_unaccounted_dangerous_start_locks_unknown_threads_to_the_owner() {
    fn unbound() -> crate::rc::roster::Entry {
        crate::rc::roster::Entry {
            runtime: "codex".into(),
            thread_id: String::new(),
            cwd: "/tmp/project".into(),
            workspace_id: "ws-a".into(),
            project_id: None,
            agit_session: None,
            expected_agent_id: None,
            permission_mode: Some(crate::protocol::PermissionMode::Bypass),
            guard_attempts: Default::default(),
            prior_threads: vec![],
            ever_dangerous: true,
        }
    }
    let mut roster = crate::rc::roster::Roster::default();
    roster.record("agit-crashed", unbound());

    assert!(
        roster.transcript_ever_dangerous("codex", "unknown-native", "ws-a", "/elsewhere"),
        "an unaccounted thread may be that crashed bypass session"
    );
    assert!(
        !roster.transcript_ever_dangerous("codex", "unknown-native", "ws-b", "/elsewhere"),
        "the poisoning reaches only its own workspace"
    );
    assert!(
        roster.transcript_ever_dangerous("codex", "unknown-native", "ws-b", "/tmp/project"),
        "a second workspace binding the same directory sees the same orphan transcript"
    );
    assert!(
        !roster.transcript_ever_dangerous("claude-code", "unknown-native", "ws-a", "/tmp/project"),
        "the poisoning reaches only its own runtime"
    );

    // Once `Bound` fills in the real id the poisoning lifts: an unknown thread is judged
    // normally again, while that session itself is accounted for by thread id and stays
    // dangerous.
    roster.record(
        "agit-crashed",
        crate::rc::roster::Entry {
            thread_id: "native-9".into(),
            ..unbound()
        },
    );
    assert!(!roster.transcript_ever_dangerous("codex", "unknown-native", "ws-a", "/tmp/project"));
    assert!(roster.transcript_ever_dangerous("codex", "native-9", "ws-a", "/tmp/project"));
}

/// The one above pins only the test itself. **This one pins that the takeover path actually asks
/// it.**
///
/// The takeover branch of `session.resume` **mints** a fresh logical id for a transcript the
/// ledger does not know, and that id is forever clean to `Roster::ever_dangerous`. Hang the test
/// back on the logical id and every operator here is let through — holding the entire context of
/// a session that ran with no checks.
#[test]
fn an_operator_cannot_take_over_an_unaccounted_dangerous_transcript() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                // The cwd in the ledger is the canonical path `policy::require_within` hands
                // back (`spawn_session` persists exactly `spec.cwd`), and the other side of the
                // comparison is that same path.
                let cwd = std::fs::canonicalize(home.path()).unwrap();
                // The row for a dangerous start that crashed before `Bound` reached disk: an
                // empty thread id, and the ledger cannot name the transcript of that unchecked
                // run.
                let mut roster = Roster::default();
                let mut orphan =
                    rpc_test_roster_entry("agit-crashed", crate::protocol::PermissionMode::Bypass);
                orphan.runtime = "unsupported-test-runtime".into();
                orphan.thread_id = String::new();
                orphan.ever_dangerous = true;
                orphan.workspace_id = "ws-a".into();
                orphan.cwd = cwd.to_string_lossy().into_owned();
                roster.sessions.insert("agit-crashed".into(), orphan);

                let daemon = rpc_test_daemon(HashMap::new(), roster);
                let mut state = daemon.lock().await;
                state.mirror.bind("ws-a", "project-a", home.path()).unwrap();
                // A second workspace binds the same directory — scope the poisoning by
                // workspace alone and this side is the hole.
                state.mirror.bind("ws-b", "project-b", home.path()).unwrap();
                let (frames, _frames_rx) = mpsc::channel(1);

                let located = |workspace: &str| LocatedLocal {
                    runtime: "unsupported-test-runtime".into(),
                    cwd: cwd.clone(),
                    project_id: Some(format!("project-{workspace}")),
                    likely_active: false,
                };
                let takeover = |workspace: &str| SessionResume {
                    workspace_id: workspace.into(),
                    session_id: "native-nobody-knows".into(),
                    prompt: None,
                    by: None,
                    agent: None,
                    expected_agent_id: None,
                    branch: None,
                };

                for workspace in ["ws-a", "ws-b"] {
                    let error = state
                        .take_over_local_session(
                            located(workspace),
                            takeover(workspace),
                            &claim("operator", workspace),
                            &frames,
                        )
                        .await
                        .expect_err("an unaccounted thread may be that crashed bypass session");
                    assert_eq!(
                        error.code,
                        ErrorCode::DangerousSessionLocked as i32,
                        "{workspace}: {}",
                        error.message
                    );
                }

                // The owner still gets through — this gate locks the role, not the path itself.
                let error = state
                    .take_over_local_session(
                        located("ws-a"),
                        takeover("ws-a"),
                        &claim("owner", "ws-a"),
                        &frames,
                    )
                    .await
                    .expect_err("the unsupported test runtime cannot actually launch");
                assert_eq!(
                    error.code,
                    ErrorCode::RuntimeUnavailable as i32,
                    "{}",
                    error.message
                );
            });
    });
}
/// The one above pins the backstop for when the ledger **cannot account for** the transcript.
/// This one pins the case the ledger **does** account for and still misses, because that row sits
/// under another workspace.
///
/// Two workspaces can each bind the same directory. The bypass session in ws-a bound a real
/// thread id and `Bound` reached disk, so it is neither "thread_id is empty" nor in
/// `unconfirmed_dangerous_bindings` — `dangerous_start_unaccounted` matches nothing; and a lookup
/// by logical identity is held off by the workspace test. That transcript still lies in a
/// directory ws-b binds too, and `local_sessions("ws-b")` lists it as takeable: scope the test by
/// workspace alone and one takeover by ws-b's operator mints a clean `dangerous: false` identity
/// that inherits everything that unchecked run read into its context, and then `Bound` persists
/// that "clean" as another row.
///
/// When the ledger **does** know which transcript this is, there is even less reason to be looser
/// than when it does not.
#[test]
fn a_bound_dangerous_transcript_stays_owner_only_from_a_second_workspace_sharing_its_folder() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let cwd = std::fs::canonicalize(home.path()).unwrap();
                // A dangerous row the ledger **fully accounts for**: a real thread id, and a
                // confirmed binding.
                let mut roster = Roster::default();
                let mut bound =
                    rpc_test_roster_entry("agit-bound", crate::protocol::PermissionMode::Bypass);
                bound.runtime = "unsupported-test-runtime".into();
                bound.thread_id = "native-1".into();
                bound.ever_dangerous = true;
                bound.workspace_id = "ws-a".into();
                bound.cwd = cwd.to_string_lossy().into_owned();
                roster.sessions.insert("agit-bound".into(), bound);
                assert!(
                    roster.unconfirmed_dangerous_bindings.is_empty(),
                    "precondition: this row's binding is confirmed, so \
                     `dangerous_start_unaccounted` does not match it"
                );

                let daemon = rpc_test_daemon(HashMap::new(), roster);
                let mut state = daemon.lock().await;
                state.mirror.bind("ws-a", "project-a", home.path()).unwrap();
                // A second workspace binds the same directory — its listing shows the same
                // transcript.
                state.mirror.bind("ws-b", "project-b", home.path()).unwrap();
                let (frames, _frames_rx) = mpsc::channel(1);

                let located = |workspace: &str| LocatedLocal {
                    runtime: "unsupported-test-runtime".into(),
                    cwd: cwd.clone(),
                    project_id: Some(format!("project-{workspace}")),
                    likely_active: false,
                };
                let takeover = |workspace: &str| SessionResume {
                    workspace_id: workspace.into(),
                    session_id: "native-1".into(),
                    prompt: None,
                    by: None,
                    agent: None,
                    expected_agent_id: None,
                    branch: None,
                };

                for workspace in ["ws-a", "ws-b"] {
                    let error = state
                        .take_over_local_session(
                            located(workspace),
                            takeover(workspace),
                            &claim("operator", workspace),
                            &frames,
                        )
                        .await
                        .expect_err("an unchecked transcript is owner-only in every workspace");
                    assert_eq!(
                        error.code,
                        ErrorCode::DangerousSessionLocked as i32,
                        "{workspace}'s operator took over the transcript that ran unchecked; \
                         the monotonic danger bit did not follow it across the workspace \
                         boundary (got: {})",
                        error.message
                    );
                }

                // The viewing path reads the same test: ws-b cannot call it clean either.
                assert!(state.roster.transcript_ever_dangerous(
                    "unsupported-test-runtime",
                    "native-1",
                    "ws-b",
                    &cwd.to_string_lossy()
                ));

                // The owner still gets through — this gate locks the role, not the path itself.
                let error = state
                    .take_over_local_session(
                        located("ws-b"),
                        takeover("ws-b"),
                        &claim("owner", "ws-b"),
                        &frames,
                    )
                    .await
                    .expect_err("the unsupported test runtime cannot actually launch");
                assert_eq!(
                    error.code,
                    ErrorCode::RuntimeUnavailable as i32,
                    "{}",
                    error.message
                );
            });
    });
}

/// The two above pin the **takeover** half of `session.resume` (what comes in is the harness's
/// own thread id). This one pins the **other** half of the same RPC: what comes in is an `agit-*`
/// logical id.
///
/// That branch finds its own row in the ledger and then `--resume`s with **that row's thread
/// id** — what gets loaded into context is the harness transcript, so the test can only be the
/// transcript. Ask only its own row and a clean sibling row for the same transcript in another
/// workspace is a master key, and two coexisting rows are **by design** the normal case: when two
/// workspaces each bind the same directory, the workspace test in `logical_for_thread` requires
/// each side to mint its own id (see the comment in `take_over_local_session`), while the danger
/// bit is armed per row (`guard.rs`) — so the ws-a row is poisoned and the ws-b row is clean.
///
/// The reported bit is pinned with it: write `SessionInfo.dangerous` as false and every later
/// `turn.start` / `turn.steer` / `approval.decide` goes through the `Need::Drive` gate
/// (`session_channel` in `projection.rs` reads exactly that). It is observed here through
/// "whether this session was recorded in `unconfirmed_dangerous_bindings` before launch": that
/// step happens only when `spawn_session` sees `info.dangerous`.
#[test]
fn resuming_by_logical_id_judges_the_transcript_not_just_its_own_roster_row() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let cwd = std::fs::canonicalize(home.path()).unwrap();
                let cwd = cwd.to_string_lossy().into_owned();
                let mut roster = Roster::default();
                // The ws-a row: this transcript ran unchecked and its binding is confirmed
                // (neither an empty thread id nor in `unconfirmed_dangerous_bindings`).
                let mut poisoned =
                    rpc_test_roster_entry("agit-poisoned", crate::protocol::PermissionMode::Bypass);
                poisoned.runtime = "unsupported-test-runtime".into();
                poisoned.thread_id = "native-1".into();
                poisoned.cwd = cwd.clone();
                poisoned.workspace_id = "ws-a".into();
                poisoned.ever_dangerous = true;
                roster.sessions.insert("agit-poisoned".into(), poisoned);
                // The ws-b row: same runtime, same thread, same directory, and clean in its own
                // cell — this is what ws-b's operator sees in the web interface.
                let mut clean =
                    rpc_test_roster_entry("agit-clean", crate::protocol::PermissionMode::Default);
                clean.runtime = "unsupported-test-runtime".into();
                clean.thread_id = "native-1".into();
                clean.cwd = cwd.clone();
                clean.workspace_id = "ws-b".into();
                clean.ever_dangerous = false;
                roster.sessions.insert("agit-clean".into(), clean);

                let daemon = rpc_test_daemon(HashMap::new(), roster);
                let mut state = daemon.lock().await;
                state.mirror.bind("ws-a", "project-a", home.path()).unwrap();
                state.mirror.bind("ws-b", "project-b", home.path()).unwrap();
                let (frames, _frames_rx) = mpsc::channel(1);

                let resume = || SessionResume {
                    workspace_id: "ws-b".into(),
                    session_id: "agit-clean".into(),
                    prompt: None,
                    by: None,
                    agent: None,
                    expected_agent_id: None,
                    branch: None,
                };

                let error = state
                    .resume_session(resume(), &claim("operator", "ws-b"), &frames)
                    .await
                    .expect_err("an unchecked transcript is owner-only by logical id");
                assert_eq!(
                    error.code,
                    ErrorCode::DangerousSessionLocked as i32,
                    "ws-b's operator resumed the unchecked transcript by logical id; this \
                     branch asked only its own row's `ever_dangerous`, while what it \
                     `--resume`s is the same transcript the ws-a row poisoned (got: {})",
                    error.message
                );

                // The owner still gets through — this gate locks the role, not the path itself.
                let error = state
                    .resume_session(resume(), &claim("owner", "ws-b"), &frames)
                    .await
                    .expect_err("the unsupported test runtime cannot actually launch");
                assert_eq!(
                    error.code,
                    ErrorCode::RuntimeUnavailable as i32,
                    "{}",
                    error.message
                );
                assert!(
                    state
                        .roster
                        .unconfirmed_dangerous_bindings
                        .contains("agit-clean"),
                    "the judged danger bit is not written into this session: `spawn_session` \
                     did not prewrite it as dangerous, which means `SessionInfo.dangerous` is \
                     false — every later turn.start / turn.steer / approval.decide then goes \
                     through `Need::Drive`"
                );
                // This ledger row changes its answer too, and reaches disk **before** launch.
                // With the two sides saying different things, `arm_danger_before_loosening`
                // treats the next mode change as a **new** flip into dangerous, so the moment
                // that change is withdrawn `disarm_danger` washes this unchecked transcript
                // clean.
                assert!(
                    Roster::load().sessions["agit-clean"].ever_dangerous,
                    "the judged danger bit did not reach the ledger; the disk row still says clean"
                );
            });
    });
}

/// A session that **starts** in a dangerous mode must have its danger bit on disk before the
/// harness launches.
///
/// Writing the durable record only in `SessionNote::Bound` (asynchronous after launch for claude,
/// waiting on the native Ready for codex, and a failed save is only an eprintln) leaves a window:
/// agitd crashing inside it leaves nothing on disk, and one takeover by harness thread id hands
/// out a clean `ever_dangerous == false` identity.
#[test]
fn a_dangerous_start_is_durable_before_the_harness_launches() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let daemon = rpc_test_daemon(HashMap::new(), Roster::default());
                let mut state = daemon.lock().await;
                let now = chrono::Utc::now().to_rfc3339();
                let info = SessionInfo {
                    session_id: "agit-danger-start".into(),
                    workspace_id: "ws-a".into(),
                    project_id: Some("project-a".into()),
                    runtime: "unsupported-test-runtime".into(),
                    agent: None,
                    branch: None,
                    status: SessionStatus::Idle,
                    last_seq: 0,
                    gist: None,
                    dangerous: true,
                    permission_mode: Some(crate::protocol::PermissionMode::Bypass),
                    created_at: now.clone(),
                    updated_at: now,
                };
                let spec = LaunchSpec {
                    cwd: home.path().to_path_buf(),
                    resume_from: None,
                    agit_session: None,
                    model: None,
                    dangerous: true,
                    permission_mode: Some(crate::protocol::PermissionMode::Bypass),
                };
                let (frames, _frames_rx) = mpsc::channel(4);
                let error = state
                    .spawn_session(
                        info,
                        spec,
                        danger::TranscriptDanger::fresh_transcript(),
                        &frames,
                        None,
                        None,
                    )
                    .await
                    .expect_err("the unsupported test runtime cannot actually launch");
                assert_eq!(error.error.code, ErrorCode::RuntimeUnavailable as i32);
                assert!(
                    !error.reached_launch,
                    "an unknown runtime never builds a command line, proving no process started"
                );

                // The launch failed and the dangerous record is still on disk: **persist first,
                // launch second**, so a crash before `Bound` still leaves evidence.
                let disk = Roster::load();
                let entry = &disk.sessions["agit-danger-start"];
                assert!(entry.ever_dangerous);
                assert!(entry.thread_id.is_empty());
                // The consequence lands here: every thread in this territory the **ledger
                // cannot account for** is treated as dangerous, and nobody takes this unchecked
                // transcript over as a clean identity.
                assert!(disk.transcript_ever_dangerous(
                    "unsupported-test-runtime",
                    "a-thread-this-ledger-never-saw",
                    "ws-a",
                    &home.path().to_string_lossy()
                ));
            });
    });
}

/// A new resume path that **forgets to ask the ledger** must hit the wall before launch.
///
/// This is the one escape hatch in that verdict: `TranscriptDanger::fresh_transcript()` says
/// "this run does not continue any existing transcript", which holds only for a freshly started
/// conversation. Appearing together with `spec.resume_from`, it means someone loaded a harness
/// transcript into a session that was never judged — exactly the shape of this hole (the path
/// that was missed runs as usual, and nothing turns red). The type system makes every path hand
/// over a verdict; this one keeps that verdict from being a blank cheque.
#[test]
fn a_launch_that_resumes_a_transcript_it_never_cleared_is_refused() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let daemon = rpc_test_daemon(HashMap::new(), Roster::default());
                let mut state = daemon.lock().await;
                let now = chrono::Utc::now().to_rfc3339();
                let info = SessionInfo {
                    session_id: "agit-unjudged".into(),
                    workspace_id: "ws-a".into(),
                    project_id: Some("project-a".into()),
                    runtime: "unsupported-test-runtime".into(),
                    agent: None,
                    branch: None,
                    status: SessionStatus::Idle,
                    last_seq: 0,
                    gist: None,
                    dangerous: false,
                    permission_mode: Some(crate::protocol::PermissionMode::Default),
                    created_at: now.clone(),
                    updated_at: now,
                };
                let spec = LaunchSpec {
                    cwd: home.path().to_path_buf(),
                    // A real harness transcript is loaded in...
                    resume_from: Some("native-1".into()),
                    agit_session: None,
                    model: None,
                    dangerous: false,
                    permission_mode: Some(crate::protocol::PermissionMode::Default),
                };
                let (frames, _frames_rx) = mpsc::channel(4);
                let error = state
                    .spawn_session(
                        info,
                        spec,
                        // ...while carrying the verdict "this run resumes no transcript".
                        danger::TranscriptDanger::fresh_transcript(),
                        &frames,
                        None,
                        None,
                    )
                    .await
                    .expect_err(
                        "an unjudged transcript must not be launched; letting it through \
                         carries a context that may have run unchecked while reporting a clean \
                         session",
                    );
                assert_eq!(
                    error.error.code,
                    ErrorCode::Internal as i32,
                    "carrying `--resume` with a verdict that says there is no transcript to \
                     judge must stop before launch; it went through instead (got: {})",
                    error.error.message
                );
                assert!(
                    !error.reached_launch,
                    "this gate sits before launch, which proves no process started"
                );
                assert!(
                    state.sessions.is_empty() && state.roster.sessions.is_empty(),
                    "a refused attempt leaves no session trace"
                );

                // Another verdict that likewise cannot launch a session: `judge` asks the
                // ledger but **does not judge the caller** — it is for read-only following
                // (`session.watch`). Its signature is shorter and takes no caller, which makes
                // it the one the next resume path picks up by hand; what it bypasses is the
                // owner-only gate.
                let info = SessionInfo {
                    session_id: "agit-unauthorized".into(),
                    workspace_id: "ws-a".into(),
                    project_id: Some("project-a".into()),
                    runtime: "unsupported-test-runtime".into(),
                    agent: None,
                    branch: None,
                    status: SessionStatus::Idle,
                    last_seq: 0,
                    gist: None,
                    dangerous: false,
                    permission_mode: Some(crate::protocol::PermissionMode::Default),
                    created_at: "now".into(),
                    updated_at: "now".into(),
                };
                let spec = LaunchSpec {
                    cwd: home.path().to_path_buf(),
                    resume_from: Some("native-1".into()),
                    agit_session: None,
                    model: None,
                    dangerous: false,
                    permission_mode: Some(crate::protocol::PermissionMode::Default),
                };
                let watched = danger::judge(
                    &state.roster,
                    "unsupported-test-runtime",
                    "native-1",
                    "ws-a",
                    &home.path().to_string_lossy(),
                );
                let error = state
                    .spawn_session(info, spec, watched, &frames, None, None)
                    .await
                    .expect_err("a read-only follow verdict cannot launch a session");
                assert_eq!(
                    error.error.code,
                    ErrorCode::Internal as i32,
                    "a verdict that asked the ledger without judging the caller launched a \
                     transcript; the owner-only gate is bypassed (got: {})",
                    error.error.message
                );
            });
    });
}

/// While the dangerous prewrite row still carries an empty thread id (crashed before the harness
/// reported a native id), resuming by logical id must refuse honestly — an empty string handed to
/// `--resume` launches a **brand new** conversation wearing the old session's logical identity
/// and permission mode.
#[test]
fn a_session_that_crashed_before_binding_cannot_be_resumed_by_logical_id() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut roster = Roster::default();
                let mut entry =
                    rpc_test_roster_entry("agit-crashed", crate::protocol::PermissionMode::Bypass);
                entry.thread_id = String::new();
                entry.ever_dangerous = true;
                entry.runtime = "unsupported-test-runtime".into();
                entry.cwd = home.path().to_string_lossy().into_owned();
                roster.sessions.insert("agit-crashed".into(), entry);
                let daemon = rpc_test_daemon(HashMap::new(), roster);
                let mut state = daemon.lock().await;
                state.mirror.bind("ws-a", "project-a", home.path()).unwrap();
                let (frames, _frames_rx) = mpsc::channel(1);
                let error = state
                    .resume_session(
                        SessionResume {
                            workspace_id: "ws-a".into(),
                            session_id: "agit-crashed".into(),
                            prompt: None,
                            by: None,
                            agent: None,
                            expected_agent_id: None,
                            branch: None,
                        },
                        &claim("owner", "ws-a"),
                        &frames,
                    )
                    .await
                    .expect_err("an unbound thread is not a resumable address");
                assert_eq!(error.code, ErrorCode::SessionNotFound as i32);
                assert!(
                    error
                        .message
                        .contains("before its harness reported a native id"),
                    "{}",
                    error.message
                );
            });
    });
}

#[test]
fn session_start_idempotency_is_an_explicit_per_socket_feature() {
    assert!(
        advertised_connection_features()
            .iter()
            .any(|feature| feature == crate::protocol::feature::SESSION_START_IDEMPOTENCY_V1)
    );
    let mut result = crate::protocol::RcRegisterResult {
        connection_id: "conn".into(),
        accepted_features: vec![],
        workspaces: vec![],
        persisted_seq: Default::default(),
        server_time: "now".into(),
    };
    assert_eq!(accepted_connection_features(&result), (false, false));
    result.accepted_features = vec![crate::protocol::feature::SESSION_START_IDEMPOTENCY_V1.into()];
    assert_eq!(accepted_connection_features(&result), (false, true));

    assert_eq!(negotiated_start_id(false, None).unwrap(), None);
    assert!(negotiated_start_id(false, Some("018f47cb-60ff-7e31-aec9-02d2e39d3114")).is_err());
    assert!(negotiated_start_id(true, None).is_err());
    assert!(negotiated_start_id(true, Some("not-a-uuid")).is_err());
    assert_eq!(
        negotiated_start_id(true, Some("018F47CB-60FF-7E31-AEC9-02D2E39D3114"))
            .unwrap()
            .as_deref(),
        Some("018f47cb-60ff-7e31-aec9-02d2e39d3114")
    );
    let pending = pending_start_error("018f47cb-60ff-7e31-aec9-02d2e39d3114", "agit-one");
    assert_eq!(pending.code, ErrorCode::SessionBusy.code());
    assert!(pending.message.contains("launch outcome"));
    assert!(
        pending.data.unwrap()["hint"]
            .as_str()
            .unwrap()
            .contains("do not choose a new start_id")
    );
}

#[tokio::test]
async fn start_session_replays_a_completed_start_after_a_display_name_change() {
    let start_id = "018f47cb-60ff-7e31-aec9-02d2e39d3114";
    let session = SessionInfo {
        session_id: "agit-existing".into(),
        workspace_id: "ws-a".into(),
        project_id: Some("project-a".into()),
        runtime: "codex".into(),
        agent: None,
        branch: None,
        status: SessionStatus::Idle,
        last_seq: 0,
        gist: None,
        dangerous: false,
        permission_mode: Some(crate::protocol::PermissionMode::Default),
        created_at: "now".into(),
        updated_at: "now".into(),
    };
    let expected = SessionStartResult {
        start_id: Some(start_id.into()),
        session,
    };
    let mut roster = Roster::default();
    roster.starts.insert(
        start_id.into(),
        roster::StartIntent {
            spec: roster::StartSpec {
                workspace_id: "ws-a".into(),
                project_id: "project-a".into(),
                runtime: "codex".into(),
                cwd: "/already-resolved".into(),
                agit_session: None,
                expected_agent_id: None,
                prompt: Some("inspect".into()),
                by: Some("old display name".into()),
                permission_mode: crate::protocol::PermissionMode::Default,
            },
            state: roster::StartState::Completed {
                result: expected.clone(),
            },
        },
    );
    let daemon = rpc_test_daemon(HashMap::new(), roster);
    let (frames, _frames_rx) = mpsc::channel(1);
    let mut state = daemon.lock().await;
    set_connection_features(&state.settlement, 1, false, true);

    let replayed = state
        .start_session(
            SessionStart {
                start_id: Some(start_id.into()),
                workspace_id: "ws-a".into(),
                project_id: "project-a".into(),
                runtime: "codex".into(),
                agent: None,
                expected_agent_id: None,
                branch: None,
                prompt: Some("inspect".into()),
                by: Some("renamed display name".into()),
                permission_mode: Some(crate::protocol::PermissionMode::Default),
            },
            &claim("owner", "ws-a"),
            &frames,
        )
        .await
        .expect("display-only drift must replay the completed launch");

    assert_eq!(
        serde_json::from_value::<SessionStartResult>(replayed).unwrap(),
        expected
    );
}

/// For the case that fails **before** launch, keyed `session.start` must release the reservation.
///
/// A failure to persist the danger bit (the `save` `spawn_session` runs before handing off to the
/// harness) is **provably no process at all**: none started. Treating it like "unknown after
/// launch" as `SessionBusy` leaves that durable Pending on disk forever — every retry takes the
/// early-return branch and gets `pending_start_error` (whose hint still says not to choose a new
/// start_id), a restart does not recover it, and once the disk is writable again this start is
/// still dead.
#[test]
fn a_prewrite_failure_before_launch_releases_the_start_reservation() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let start_id = "018f47cb-60ff-7e31-aec9-02d2e39d3115";
                let daemon = rpc_test_daemon(HashMap::new(), Roster::default());
                let (frames, _frames_rx) = mpsc::channel(1);
                let mut state = daemon.lock().await;
                state.mirror.bind("ws-a", "project-a", home.path()).unwrap();
                set_connection_features(&state.settlement, 1, false, true);

                // The first save persists the reservation (it succeeds, so the reservation **is
                // durable**); the second is the danger-bit prewrite before launch — that is the
                // one singled out to fail.
                roster::fail_primary_save_number(2);
                let error = state
                    .start_session(
                        SessionStart {
                            start_id: Some(start_id.into()),
                            workspace_id: "ws-a".into(),
                            project_id: "project-a".into(),
                            runtime: "codex".into(),
                            agent: None,
                            expected_agent_id: None,
                            branch: None,
                            prompt: None,
                            by: None,
                            permission_mode: Some(crate::protocol::PermissionMode::Bypass),
                        },
                        &claim("owner", "ws-a"),
                        &frames,
                    )
                    .await
                    .expect_err("a danger bit that cannot reach disk must not launch");
                assert_eq!(roster::pending_injected_saves(), (0, 0));

                assert_eq!(
                    error.code,
                    ErrorCode::Internal as i32,
                    "a failure before launch is not \"outcome unknown\": {}",
                    error.message
                );
                assert!(
                    error.message.contains("could not durably record"),
                    "{}",
                    error.message
                );
                assert!(
                    !error.message.contains("launch completion is unknown"),
                    "no process started, so the launch outcome must not be reported as unknown: {}",
                    error.message
                );

                assert!(
                    state.roster.starts.is_empty(),
                    "the reservation must be released or this start_id stays Pending forever"
                );
                assert!(
                    state.roster.sessions.is_empty(),
                    "the dangerous row was inserted by this attempt; the rollback must be complete"
                );
                drop(state);

                // The release itself must reach disk: leave that Pending on disk and after a
                // restart the early-return branch still calls this start_id unretryable.
                let disk = Roster::load();
                assert!(disk.starts.is_empty(), "no Pending is left on disk");
                assert!(disk.sessions.is_empty());
                assert!(
                    !disk.transcript_ever_dangerous(
                        "codex",
                        "a-thread-this-ledger-never-saw",
                        "ws-a",
                        &home.path().to_string_lossy()
                    ),
                    "nothing started, so no orphan record poisoning the whole territory is left"
                );
                roster::fail_next_saves(0, 0);
            });
    });
}

/// The harness executable is not on this machine at all — `Command::spawn` itself reports ENOENT,
/// and **no process is created**. keyed `session.start` must release the reservation so that a
/// retry with the same start_id materializes again.
///
/// A boundary drawn at the return of `Session::launch` inside `Daemon::spawn_session` sees only
/// an `anyhow::Error` there, so "claude is not installed" and "the process is already
/// running with the handshake half-written" conflate into one outcome, both recorded as having
/// crossed the materialization boundary. That scraps this start **permanently**: a retry takes
/// the early-return branch and gets `pending_start_error` (whose hint still says not to choose a
/// new start_id), and that Pending is durable, so a restart says the same thing — while no
/// process exists on the machine at all. The real boundary is the `Command::spawn` call inside
/// `Proc::spawn`, and the fact is carried up from there by `harness::proc::LaunchError`.
///
/// The program name comes from a stand-in rather than from the environment: `claude` is installed
/// on a dev machine and missing on CI, and both must reach this path deterministically. Outside
/// the stand-in everything is real — a real `Command::spawn`, a real `Proc`, a real driver, a
/// real `Daemon::start_session`.
#[cfg(unix)]
#[test]
fn a_missing_harness_binary_releases_the_start_reservation_for_a_retry() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let start_id = "018f47cb-60ff-7e31-aec9-02d2e39d3116";
                let daemon = rpc_test_daemon(HashMap::new(), Roster::default());
                let (frames, _frames_rx) = mpsc::channel(64);
                let mut state = daemon.lock().await;
                state.mirror.bind("ws-a", "project-a", home.path()).unwrap();
                set_connection_features(&state.settlement, 1, false, true);

                let params = || SessionStart {
                    start_id: Some(start_id.into()),
                    workspace_id: "ws-a".into(),
                    project_id: "project-a".into(),
                    runtime: "claude-code".into(),
                    agent: None,
                    expected_agent_id: None,
                    branch: None,
                    prompt: None,
                    by: None,
                    permission_mode: Some(crate::protocol::PermissionMode::Plan),
                };

                // An executable that certainly does not exist on this machine.
                let missing = home.path().join("harness-that-is-not-installed");
                let error = {
                    let _stub = crate::rc::harness::proc::override_harness_program(
                        missing.to_string_lossy().into_owned(),
                    );
                    state
                        .start_session(params(), &claim("owner", "ws-a"), &frames)
                        .await
                        .expect_err("a harness that is not installed cannot start")
                };
                assert_eq!(
                    error.code,
                    ErrorCode::RuntimeUnavailable as i32,
                    "{}",
                    error.message
                );
                assert!(
                    error.message.contains("cannot start"),
                    "the error must be spawn's own: {}",
                    error.message
                );
                assert!(
                    !error.message.contains("launch completion is unknown"),
                    "the OS spawn was never crossed, so the outcome is not unknown: {}",
                    error.message
                );
                assert!(
                    state.roster.starts.is_empty(),
                    "the reservation must be released or this start_id stays Pending forever"
                );
                assert!(state.sessions.is_empty(), "nothing started");
                assert!(
                    Roster::load().starts.is_empty(),
                    "that Pending must not stay on disk either"
                );

                // Retry with the same start_id: the harness is installed this time.
                let stub = home.path().join("stub-harness");
                std::fs::write(&stub, "#!/bin/sh\nexec cat\n").unwrap();
                std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
                let value = {
                    let _stub = crate::rc::harness::proc::override_harness_program(
                        stub.to_string_lossy().into_owned(),
                    );
                    state
                        .start_session(params(), &claim("owner", "ws-a"), &frames)
                        .await
                        .expect("a released start_id materializes again")
                };
                let result: SessionStartResult = serde_json::from_value(value).unwrap();
                assert_eq!(result.start_id.as_deref(), Some(start_id));
                assert!(
                    state.sessions.contains_key(&result.session.session_id),
                    "the retry must actually produce a session"
                );
                assert!(
                    matches!(
                        state.roster.starts[start_id].state,
                        roster::StartState::Completed { .. }
                    ),
                    "the retry's result is recorded as an idempotent completed state"
                );
                drop(state);
                assert!(
                    matches!(
                        Roster::load().starts[start_id].state,
                        roster::StartState::Completed { .. }
                    ),
                    "the completed state reaches disk too"
                );
            });
    });
}

/// The harness executable is on disk but **has no execute bit** — `Command::spawn` itself reports
/// EACCES, and the kernel again creates no process. The second provably-unlaunched path: a
/// different error code, and the accounting must be exactly the same as for "not installed at
/// all" — the keyed `session.start` reservation is released, and a retry with the same start_id
/// materializes again.
///
/// It is pinned separately from ENOENT because **in the code** they are the same `cmd.spawn()`
/// call, while **semantically** they are two classes of operational failure (not installed vs.
/// installed wrong / permissions changed). With the classification pushed down into that call
/// both belong in `before_launch`; pinning only one lets a refactor that sorts by `io::ErrorKind`
/// keep just one of them.
///
/// The permission bits are `0o644` (not one execute bit) rather than "execute for others but not
/// for the owner": the latter does not hold under root, and `execve` requires at least one
/// execute bit even for root, so this path is the same EACCES under any identity on CI.
#[cfg(unix)]
#[test]
fn a_harness_binary_without_an_execute_bit_releases_the_start_reservation_for_a_retry() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let start_id = "018f47cb-60ff-7e31-aec9-02d2e39d3117";
                let daemon = rpc_test_daemon(HashMap::new(), Roster::default());
                let (frames, _frames_rx) = mpsc::channel(64);
                let mut state = daemon.lock().await;
                state.mirror.bind("ws-a", "project-a", home.path()).unwrap();
                set_connection_features(&state.settlement, 1, false, true);

                let params = || SessionStart {
                    start_id: Some(start_id.into()),
                    workspace_id: "ws-a".into(),
                    project_id: "project-a".into(),
                    runtime: "claude-code".into(),
                    agent: None,
                    expected_agent_id: None,
                    branch: None,
                    prompt: None,
                    by: None,
                    permission_mode: Some(crate::protocol::PermissionMode::Plan),
                };

                // The contents are a perfectly normal harness stand-in; only the execute bit
                // is missing.
                let stub = home.path().join("harness-without-exec-bit");
                std::fs::write(&stub, "#!/bin/sh\nexec cat\n").unwrap();
                std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o644)).unwrap();

                let error = {
                    let _stub = crate::rc::harness::proc::override_harness_program(
                        stub.to_string_lossy().into_owned(),
                    );
                    state
                        .start_session(params(), &claim("owner", "ws-a"), &frames)
                        .await
                        .expect_err("a harness without an execute bit cannot start")
                };
                assert_eq!(
                    error.code,
                    ErrorCode::RuntimeUnavailable as i32,
                    "{}",
                    error.message
                );
                assert!(
                    error.message.contains("cannot start"),
                    "the error must be spawn's own: {}",
                    error.message
                );
                assert!(
                    !error.message.contains("launch completion is unknown"),
                    "EACCES likewise never crosses the OS spawn, so the outcome is not unknown: {}",
                    error.message
                );
                assert!(
                    state.roster.starts.is_empty(),
                    "the reservation must be released or this start_id stays Pending forever"
                );
                assert!(state.sessions.is_empty(), "nothing started");
                assert!(
                    Roster::load().starts.is_empty(),
                    "that Pending must not stay on disk either"
                );

                // Retry with the same start_id: the execute bit is set this time (`chmod +x` is
                // the fix on the operations side). A start_id whose reservation was released
                // materializes again instead of hitting a permanent Pending.
                std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
                let value = {
                    let _stub = crate::rc::harness::proc::override_harness_program(
                        stub.to_string_lossy().into_owned(),
                    );
                    state
                        .start_session(params(), &claim("owner", "ws-a"), &frames)
                        .await
                        .expect("a released start_id materializes again")
                };
                let result: SessionStartResult = serde_json::from_value(value).unwrap();
                assert_eq!(result.start_id.as_deref(), Some(start_id));
                assert!(
                    state.sessions.contains_key(&result.session.session_id),
                    "the retry must actually produce a session"
                );
                assert!(
                    matches!(
                        state.roster.starts[start_id].state,
                        roster::StartState::Completed { .. }
                    ),
                    "the retry's result is recorded as an idempotent completed state"
                );
                drop(state);
                assert!(
                    matches!(
                        Roster::load().starts[start_id].state,
                        roster::StartState::Completed { .. }
                    ),
                    "the completed state reaches disk too"
                );
            });
    });
}

/// The **other side** of the boundary: `Command::spawn` succeeded (this machine really does have
/// one more process), and the handshake write after it produced no proof of success. This case is
/// still accounted for as having crossed the materialization boundary — the tombstone stays
/// forever, and a retry with the same start_id gets `pending_start_error` and does **not** launch
/// a second time.
///
/// This test is the control for the two above: pushing the classification down into `Proc::spawn`
/// routes the provably-unlaunched paths through `before_launch`; it does not loosen the whole
/// launch-failure path. Without it, an over-correction that treats everything as "never started"
/// leaves both before_launch tests green, while the real consequence is that a native harness may
/// already be running and we start a second one.
#[cfg(unix)]
#[test]
fn a_launch_that_crossed_os_spawn_keeps_the_start_pending_for_inspection() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let start_id = "018f47cb-60ff-7e31-aec9-02d2e39d3118";
                let daemon = rpc_test_daemon(HashMap::new(), Roster::default());
                let (frames, _frames_rx) = mpsc::channel(64);
                let mut state = daemon.lock().await;
                state.mirror.bind("ws-a", "project-a", home.path()).unwrap();
                set_connection_features(&state.settlement, 1, false, true);

                let params = || SessionStart {
                    start_id: Some(start_id.into()),
                    workspace_id: "ws-a".into(),
                    project_id: "project-a".into(),
                    runtime: "claude-code".into(),
                    agent: None,
                    expected_agent_id: None,
                    branch: None,
                    prompt: None,
                    by: None,
                    permission_mode: Some(crate::protocol::PermissionMode::Plan),
                };

                // A properly installed harness: spawn always succeeds. The failure happens in
                // the handshake write after it, and after that line **has already gone out** —
                // the child may well have consumed it.
                let stub = home.path().join("stub-harness");
                std::fs::write(&stub, "#!/bin/sh\nexec cat\n").unwrap();
                std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

                let error = {
                    let _stub = crate::rc::harness::proc::override_harness_program(
                        stub.to_string_lossy().into_owned(),
                    );
                    crate::rc::harness::proc::fail_next_launch_writes(1);
                    state
                        .start_session(params(), &claim("owner", "ws-a"), &frames)
                        .await
                        .expect_err("a handshake write with no success proof is not a success")
                };
                assert_eq!(
                    error.code,
                    ErrorCode::SessionBusy as i32,
                    "{}",
                    error.message
                );
                assert!(
                    error.message.contains("launch completion is unknown"),
                    "a process was created, so the outcome is unknown: {}",
                    error.message
                );
                assert!(state.sessions.is_empty(), "no live session is registered");
                assert!(
                    matches!(
                        state.roster.starts[start_id].state,
                        roster::StartState::Pending { .. }
                    ),
                    "the tombstone must stay: a native process may really be running"
                );
                assert!(
                    matches!(
                        Roster::load().starts[start_id].state,
                        roster::StartState::Pending { .. }
                    ),
                    "the tombstone must survive a daemon restart"
                );

                // Retry with the same start_id — even with a perfectly healthy harness this
                // time, launching another is **forbidden**: go look on this machine for whether
                // that process is still there.
                let retry = {
                    let _stub = crate::rc::harness::proc::override_harness_program(
                        stub.to_string_lossy().into_owned(),
                    );
                    state
                        .start_session(params(), &claim("owner", "ws-a"), &frames)
                        .await
                        .expect_err("a start_id past materialization must not materialize again")
                };
                assert_eq!(
                    retry.code,
                    ErrorCode::SessionBusy as i32,
                    "{}",
                    retry.message
                );
                assert!(
                    retry.message.contains("launch outcome"),
                    "{}",
                    retry.message
                );
                assert!(state.sessions.is_empty(), "a second launch never happens");
            });
    });
}

#[test]
fn ending_a_socket_epoch_revokes_the_ack_even_after_a_quick_reconnect() {
    let (settlement, receiver) = tokio::sync::watch::channel(SettlementState::default());
    set_connection_features(&settlement, 1, true, true);
    let first = *receiver.borrow();
    assert!(first.agent_identity_v1);
    assert!(first.session_start_idempotency_v1);
    assert!(connection_epoch_is_current(&settlement, 1));

    // This is the transition the reconnect task performs immediately when
    // run_once returns (including an internal event-queue timeout).
    set_connection_features(&settlement, 2, false, false);
    let disconnected = *receiver.borrow();
    assert!(!disconnected.agent_identity_v1);
    assert!(!disconnected.session_start_idempotency_v1);
    assert_ne!(disconnected.epoch, first.epoch);
    assert!(!connection_epoch_is_current(&settlement, 1));

    // A fast new ACK cannot revive work fenced by the old epoch.
    set_connection_features(&settlement, 3, true, true);
    let reconnected = *receiver.borrow();
    assert!(reconnected.agent_identity_v1);
    assert!(reconnected.session_start_idempotency_v1);
    assert_ne!(reconnected.epoch, first.epoch);
    assert!(connection_epoch_is_current(&settlement, 3));
    assert!(
        !connection_epoch_is_current(&settlement, 1),
        "an old queued frame must not borrow the new socket's ACK"
    );
}

/// Through the whole `agit rc stop` shutdown the settlement authorization must **stay alive**
/// until the fleet's exit settlements have actually finished.
///
/// Revoking on disconnect is right: the socket is gone, and a settlement still running must not
/// keep using the old connection's identity to land/push. When we stop on our own the connection
/// is still there and the identity is the same, so that threat is not present — and the first
/// line of `Session::settle_on_exit` → `settle_and_push` takes the lease. Revoke it before the
/// sessions receive `Shutdown` and every `agit rc stop` exit settlement spins with no effect: the
/// last turn's commit stays in the local git while the hub looks as if this session never
/// persisted anything.
///
/// The grace period is pinned with it: waiting for the hub to accept `commit.settled` is already
/// `supervisor::SETTLEMENT_DELIVERY_WAIT`, and land/commit/push come after it. A cap that cannot
/// hold one settlement cuts this path off halfway the moment it actually starts running.
#[test]
fn the_stop_drain_keeps_the_settlement_lease_until_the_fleet_has_settled() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(async {
                let daemon = rpc_test_daemon(HashMap::new(), Roster::default());
                let mut state = daemon.lock().await;
                set_connection_features(&state.settlement, 7, true, true);

                // The first step of stopping: take no new work, and start the RPC hard deadline.
                let mut stopping = false;
                let link_stopping = std::sync::atomic::AtomicBool::new(false);
                let (rpc_stop, _rpc_stop_rx) = tokio::sync::watch::channel(false);
                let deadline = tokio::time::sleep(SESSION_RPC_SHUTDOWN_GRACE);
                tokio::pin!(deadline);
                begin_daemon_stop(&mut stopping, &link_stopping, &rpc_stop, deadline.as_mut());
                assert!(stopping && *rpc_stop.borrow());
                let after_stop = *state.settlement.borrow();
                assert!(
                    after_stop.agent_identity_v1,
                    "the settlement authorization is gone before the sessions receive \
                     Shutdown; the exit settlement returns at its first line"
                );
                assert_eq!(after_stop.epoch, 7, "stopping ourselves keeps the epoch");

                // One session's exit path: take the lease after `Shutdown` (or after the
                // command channel closes), then sit through the longest wait a settlement has.
                let lease_seen = std::sync::Arc::new(AtomicBool::new(false));
                let settled = std::sync::Arc::new(AtomicBool::new(false));
                let (tx, mut rx) = mpsc::channel(1);
                let task = {
                    let settlement = state.settlement.subscribe();
                    let lease_seen = lease_seen.clone();
                    let settled = settled.clone();
                    tokio::spawn(async move {
                        let _ = rx.recv().await;
                        if !settlement.borrow().agent_identity_v1 {
                            return;
                        }
                        lease_seen.store(true, Ordering::SeqCst);
                        // `supervisor::SETTLEMENT_DELIVERY_WAIT`.
                        tokio::time::sleep(std::time::Duration::from_secs(12)).await;
                        settled.store(true, Ordering::SeqCst);
                    })
                };
                let mut live =
                    rpc_test_live("session-a", 1, tx, crate::protocol::PermissionMode::Default);
                live.task = task;
                state.sessions.insert("session-a".into(), live);

                state.shutdown().await;

                assert!(
                    lease_seen.load(Ordering::SeqCst),
                    "the exit settlement cannot take the lease; the last turn lands no commit"
                );
                assert!(
                    settled.load(Ordering::SeqCst),
                    "the grace period cannot hold one exit settlement and cuts it off halfway"
                );
                let after_drain = *state.settlement.borrow();
                assert!(
                    !after_drain.agent_identity_v1 && !after_drain.session_start_idempotency_v1,
                    "the authorization is revoked only after the fleet drains — and at that \
                     moment it must really be revoked"
                );
                assert_ne!(after_drain.epoch, after_stop.epoch);
            });
    });
}

/// The marker that says "a stretch is missing here" **must itself be a well-formed event**.
///
/// The hub decides whether to fan a frame out with `is_event()` (a notification, with a stream,
/// with a seq). A notification built out of nowhere has `seq` `None`, and the far side drops it
/// without a sound — so the marker whose job is to make the gap visible is itself invisible. The
/// same mechanism fails silently on this path: nothing errors and local tests stay green.
#[test]
fn the_gap_notice_is_itself_a_well_formed_event() {
    let mut dropped = Frame::notification(
        method::TERMINAL_OUTPUT,
        serde_json::json!({ "terminal_id": "t-1", "data": "hello" }),
    );
    dropped.stream = Some("term:ws-1".into());
    dropped.seq = Some(42);

    let notice = gap_notice(&dropped, "t-1");
    assert!(
        notice.is_event(),
        "the hub drops it as a non-event; the marker itself becomes invisible"
    );
    assert_eq!(notice.stream.as_deref(), Some("term:ws-1"));
    assert_eq!(notice.seq, Some(42), "it reuses the dropped frame's seq");
    // **Never mark it reliable.** That sends it into the response queue, which drains first, so
    // it cuts ahead of frames on the same stream that carry smaller seqs and are still in the
    // event queue — it inherits the dropped frame's seq, and jumping ahead opens another gap. It
    // survives on `send_ordered` (queue at the tail, wait for a slot).
    assert!(!notice.reliable, "the marker must not jump the queue");
    // Attached to **that** terminal: the web interface dispatches by terminal_id, and the wrong
    // attachment is the same as not sending it.
    assert_eq!(
        notice.params.as_ref().unwrap()["terminal_id"],
        serde_json::json!("t-1")
    );
    assert_eq!(notice.method(), method::TERMINAL_OUTPUT);
}

#[test]
fn terminal_emitters_serialize_the_declared_protocol_types() {
    let output = terminal_output_frame("term-1".into(), "hello".into());
    assert_eq!(output.method(), method::TERMINAL_OUTPUT);
    assert_eq!(
        output.params,
        Some(
            serde_json::to_value(TerminalOutput {
                terminal_id: "term-1".into(),
                data: "hello".into(),
            })
            .unwrap()
        )
    );

    let exited = terminal_exited_frame("term-1".into(), None);
    assert_eq!(exited.method(), method::TERMINAL_EXITED);
    assert_eq!(
        exited.params,
        Some(serde_json::json!({ "terminal_id": "term-1" })),
        "the DTO's optional-code omission is part of the production wire shape"
    );
    let decoded: TerminalExited =
        serde_json::from_value(exited.params.unwrap()).expect("declared exit DTO decodes");
    assert_eq!(
        decoded,
        TerminalExited {
            terminal_id: "term-1".into(),
            code: None,
        }
    );
}

/// Handing a frame to the tail and the tail actually entering the event FIFO are not the same
/// moment.
///
/// This deliberately gives the tail no warm-up in between: as soon as the event queue frees a
/// slot, a larger seq on the same stream grabs it. The seal must send the latecomer down the
/// "congestion drop" path; the stream unseals only once an ack proves the tail frame is queued.
#[tokio::test]
async fn a_terminal_stream_stays_sealed_until_its_tail_frame_is_in_the_fifo() {
    let term = |method: &str, seq: u64| {
        let mut f = Frame::notification(
            method,
            serde_json::json!({ "terminal_id": "t-1", "data": "x" }),
        );
        f.stream = Some("term:ws-1".into());
        f.seq = Some(seq);
        f
    };
    let (out, mut rx) = crate::rc::outbound::channel();
    let (inert_tx, _inert_rx) = mpsc::unbounded_channel();
    let inert_tail = OrderedTail::new(
        inert_tx,
        std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    );
    for seq in 1..=crate::rc::outbound::EVENT_CAP as u64 {
        assert_eq!(
            send_live_frame(&out, &inert_tail, term(method::TERMINAL_OUTPUT, seq)),
            crate::rc::outbound::Sent::Queued
        );
    }

    let blockers = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (tail_tx, tail_rx) = mpsc::unbounded_channel();
    let (ack_tx, mut ack_rx) = mpsc::unbounded_channel();
    let mut tail = OrderedTail::new(tail_tx, blockers.clone());
    let drain = tokio::spawn(crate::rc::outbound::drain_ordered(
        out.clone(),
        tail_rx,
        ack_tx,
    ));

    let end_seq = crate::rc::outbound::EVENT_CAP as u64 + 1;
    let end = term(method::TERMINAL_EXITED, end_seq);
    assert!(matches!(
        send_live_frame(&out, &tail, end.clone()),
        crate::rc::outbound::Sent::DroppedReplayable(_)
    ));
    OrderedTail::reserve_terminal_exit(&blockers);
    tail.enqueue(end, true).expect("tail receiver is alive");
    assert!(terminal_delivery_blocked(&blockers));

    // Free one slot and let a larger seq grab it immediately, with no yield to the tail.
    let first = rx.next_write().await.expect("first queued output");
    assert_eq!(first.frame().seq, Some(1));
    first.commit();
    assert!(matches!(
        send_live_frame(&out, &tail, term(method::TERMINAL_OUTPUT, end_seq + 1)),
        crate::rc::outbound::Sent::DroppedReplayable(_)
    ));

    let mut last = 0;
    for _ in 0..crate::rc::outbound::EVENT_CAP {
        let pending = rx.next_write().await.expect("tail frame must arrive");
        last = pending.frame().seq.unwrap();
        pending.commit();
    }
    assert_eq!(
        last, end_seq,
        "tail frame must remain behind earlier output"
    );

    let stream = tokio::time::timeout(std::time::Duration::from_secs(1), ack_rx.recv())
        .await
        .expect("tail acknowledgement timed out")
        .expect("tail acknowledgement channel closed");
    tail.acknowledge(&stream);
    assert!(!terminal_delivery_blocked(&blockers));
    assert_eq!(
        send_live_frame(&out, &tail, term(method::TERMINAL_OUTPUT, end_seq + 2)),
        crate::rc::outbound::Sent::Queued,
        "only an acknowledged tail frame may unseal the stream"
    );

    drop(tail);
    drain.await.unwrap();
}

#[tokio::test]
async fn a_connection_bound_settlement_waits_in_order_without_using_terminal_slots() {
    let event = |seq: u64| {
        let mut frame = Frame::notification(method::ITEM_DELTA, serde_json::json!({}));
        frame.stream = Some("session-a".into());
        frame.seq = Some(seq);
        frame
    };
    let (out, mut rx) = crate::rc::outbound::channel();
    let (inert_tx, _inert_rx) = mpsc::unbounded_channel();
    let inert_tail = OrderedTail::new(
        inert_tx,
        std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    );
    for seq in 1..=crate::rc::outbound::EVENT_CAP as u64 {
        assert_eq!(
            send_live_frame(&out, &inert_tail, event(seq)),
            crate::rc::outbound::Sent::Queued
        );
    }

    let blockers = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (tail_tx, tail_rx) = mpsc::unbounded_channel();
    let (ack_tx, mut ack_rx) = mpsc::unbounded_channel();
    let mut tail = OrderedTail::new(tail_tx, blockers.clone());
    let drain = tokio::spawn(crate::rc::outbound::drain_ordered(
        out.clone(),
        tail_rx,
        ack_tx,
    ));

    let seq = crate::rc::outbound::EVENT_CAP as u64 + 1;
    let delivery = crate::protocol::ConnectionDelivery::new(
        7,
        crate::protocol::ConnectionFeature::AgentIdentityV1,
    );
    let mut settlement = Frame::notification(
        method::COMMIT_SETTLED,
        serde_json::json!({ "commit_sha": "abc" }),
    );
    settlement.stream = Some("session-a".into());
    settlement.seq = Some(seq);
    settlement.connection_delivery = Some(delivery);
    assert!(OrderedTail::must_order(&settlement));
    let dropped = match send_live_frame(&out, &tail, settlement) {
        crate::rc::outbound::Sent::DroppedReplayable(frame) => frame,
        other => panic!("full event lane should defer settlement, got {other:?}"),
    };
    tail.enqueue(*dropped, false).unwrap();
    assert_eq!(
        OrderedTail::blockers(&blockers),
        0,
        "a session settlement must not consume a terminal admission slot"
    );
    assert!(matches!(
        send_live_frame(&out, &tail, event(seq + 1)),
        crate::rc::outbound::Sent::DroppedReplayable(_)
    ));

    let first = rx.next_write().await.unwrap();
    first.commit();
    let mut last = 0;
    for _ in 0..crate::rc::outbound::EVENT_CAP {
        let pending = rx.next_write().await.unwrap();
        last = pending.frame().seq.unwrap();
        pending.commit();
    }
    assert_eq!(last, seq, "settlement stays behind every earlier event");
    let stream = ack_rx.recv().await.unwrap();
    tail.acknowledge(&stream);
    assert!(tail.is_empty());
    drop(tail);
    drain.await.unwrap();
}

/// Two readers can report their exits in one order while the main pump / ack scheduling runs in
/// the opposite order; a single bool lets the first ack also "unlock" the second, undelivered
/// terminal state. The count reopens only on the last ack.
#[test]
fn terminal_exit_reservations_are_released_one_for_one() {
    let blockers = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut tail = OrderedTail::new(tx, blockers.clone());
    let exit = |stream: &str, seq: u64| {
        let mut f = Frame::notification(
            method::TERMINAL_EXITED,
            serde_json::json!({ "terminal_id": format!("t-{seq}") }),
        );
        f.stream = Some(stream.into());
        f.seq = Some(seq);
        f
    };

    OrderedTail::reserve_terminal_exit(&blockers);
    OrderedTail::reserve_terminal_exit(&blockers);
    tail.enqueue(exit("term:ws-1", 1), true).unwrap();
    tail.enqueue(exit("term:ws-2", 1), true).unwrap();
    assert_eq!(OrderedTail::blockers(&blockers), 2);

    tail.acknowledge("term:ws-1");
    assert!(
        terminal_delivery_blocked(&blockers),
        "one delivered exit must not release another terminal's slot"
    );
    tail.acknowledge("term:ws-2");
    assert!(!terminal_delivery_blocked(&blockers));
}

/// Codex broadcasts NextTurn first and Immediate when the next turn actually starts. The first is
/// only a queued promise and must not clear itself from pending; otherwise later authorization
/// falls back to the old effective baseline.
#[tokio::test]
async fn next_turn_mode_stays_pending_until_the_immediate_fact_arrives() {
    let (tx, _rx) = mpsc::channel(1);
    let mut live = Live {
        generation: 1,
        info: SessionInfo {
            session_id: "s-1".into(),
            workspace_id: "ws-1".into(),
            project_id: None,
            runtime: "codex".into(),
            agent: None,
            branch: None,
            status: SessionStatus::Running,
            last_seq: 0,
            gist: None,
            dangerous: false,
            permission_mode: Some(crate::protocol::PermissionMode::Auto),
            created_at: "now".into(),
            updated_at: "now".into(),
        },
        tx,
        runtime_thread_id: None,
        task: tokio::spawn(async {}),
        danger_arm: 0,
        pending_mode: Some(crate::protocol::PermissionMode::Plan),
        approval_session_modes: HashMap::new(),
        rpc_gate: Arc::new(Mutex::new(())),
        rpc_guard_sensitive: false,
        confirmed_turn_guards: Default::default(),
        inflight_turn_guard: None,
        restart_guard_attempts: Default::default(),
        restart_guard_mode: None,
        ended: false,
    };
    let event = |applied| {
        Frame::notification(
            method::SESSION_PERMISSION_MODE,
            crate::protocol::SessionPermissionMode {
                session_id: "s-1".into(),
                mode: crate::protocol::PermissionMode::Plan,
                applied,
                by: Some("owner".into()),
            },
        )
    };

    let queued = event(crate::protocol::PermissionApply::NextTurn);
    let queued = queued
        .params_as::<crate::protocol::SessionPermissionMode>()
        .expect("wire event parses");
    live.observe_permission_mode(&queued);
    assert_eq!(
        live.info.permission_mode,
        Some(crate::protocol::PermissionMode::Auto),
        "NextTurn must not rewrite the effective mode"
    );
    assert_eq!(
        live.pending_mode,
        Some(crate::protocol::PermissionMode::Plan),
        "NextTurn must remain the authorization baseline"
    );
    assert_eq!(
        live.authorization_baseline(),
        crate::protocol::PermissionMode::Plan
    );

    let applied = event(crate::protocol::PermissionApply::Immediate);
    let applied = applied
        .params_as::<crate::protocol::SessionPermissionMode>()
        .expect("wire event parses");
    live.observe_permission_mode(&applied);
    assert_eq!(
        live.info.permission_mode,
        Some(crate::protocol::PermissionMode::Plan)
    );
    assert_eq!(live.pending_mode, None, "Immediate settles the queued mode");
}

#[test]
fn shutdown_guard_suppresses_mode_frames_and_clamps_every_save_path() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::{PermissionApply, PermissionMode};

                let (tx, _rx) = mpsc::channel(1);
                let live = rpc_test_live("session-a", 2, tx, PermissionMode::Plan);
                let mut entry = rpc_test_roster_entry("session-a", PermissionMode::Plan);
                let shutdown = hard_stop_guard("session-a", 1, "floor");
                entry.guard_attempts.insert(
                    shutdown.token.clone(),
                    roster::GuardAttempt {
                        expected_mode: PermissionMode::Plan,
                        observed: false,
                    },
                );
                let mut roster = Roster::default();
                roster.sessions.insert("session-a".into(), entry);
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);
                let event = |generation, mode, applied| {
                    tagged_test_notification(
                        "session-a",
                        generation,
                        method::SESSION_PERMISSION_MODE,
                        serde_json::to_value(crate::protocol::SessionPermissionMode {
                            session_id: "session-a".into(),
                            mode,
                            applied,
                            by: Some("owner".into()),
                        })
                        .unwrap(),
                    )
                };

                let mut state = daemon.lock().await;
                state
                    .latest_session_generations
                    .insert("session-a".into(), 2);
                assert!(
                    state
                        .project_session_frame(event(
                            1,
                            PermissionMode::Bypass,
                            PermissionApply::Immediate,
                        ))
                        .is_none(),
                    "the generation fence drops stale mode facts first"
                );
                for (mode, applied) in [
                    (PermissionMode::Bypass, PermissionApply::Immediate),
                    (PermissionMode::Auto, PermissionApply::NextTurn),
                    (PermissionMode::Plan, PermissionApply::Immediate),
                ] {
                    assert!(
                        state
                            .project_session_frame(event(2, mode, applied))
                            .is_none(),
                        "every same-generation mode fact is suppressed while S exists"
                    );
                }
                assert_eq!(state.journal.last_seq("session-a"), 0);
                assert_eq!(
                    state.sessions["session-a"].info.permission_mode,
                    Some(PermissionMode::Plan)
                );
                assert_eq!(state.sessions["session-a"].pending_mode, None);

                state
                    .sessions
                    .get_mut("session-a")
                    .unwrap()
                    .info
                    .permission_mode = Some(PermissionMode::Bypass);
                state.sessions.get_mut("session-a").unwrap().pending_mode =
                    Some(PermissionMode::Auto);
                state.persist_session_state("session-a").unwrap();
                assert_eq!(
                    state.sessions["session-a"].info.permission_mode,
                    Some(PermissionMode::Plan)
                );
                assert_eq!(state.sessions["session-a"].pending_mode, None);

                state
                    .sessions
                    .get_mut("session-a")
                    .unwrap()
                    .info
                    .permission_mode = Some(PermissionMode::Default);
                state.sessions.get_mut("session-a").unwrap().pending_mode =
                    Some(PermissionMode::Bypass);
                state
                    .persist_session_state_fail_closed("session-a")
                    .unwrap();
                assert_eq!(
                    state.roster.sessions["session-a"].permission_mode,
                    Some(PermissionMode::Plan)
                );
                assert!(
                    state.roster.sessions["session-a"]
                        .guard_attempts
                        .contains_key(&shutdown.token)
                );
                drop(state);

                let disk = Roster::load();
                assert_eq!(
                    disk.sessions["session-a"].permission_mode,
                    Some(PermissionMode::Plan)
                );
                assert!(
                    disk.sessions["session-a"]
                        .guard_attempts
                        .contains_key(&shutdown.token)
                );
            });
    });
}

/// **The critical beat**: a read-only follow's idle time is already against the cap when someone
/// joins.
///
/// It must survive. If the test lives on the tail itself, "it is about to exit" and "someone is
/// about to join" are two subjects looking at two moments: after the tail reads "no new viewers"
/// and before it actually breaks, a new viewer joins and sees `is_finished()` still false — it
/// hangs on a dying tail and not one frame arrives. With the test in the daemon, the two sit
/// under the same lock; this case pins that interleaving.
#[test]
fn a_viewer_joining_at_the_last_moment_keeps_the_tail_alive() {
    use std::sync::atomic::AtomicU64;
    // `renew()` writes the **real** current instant, so the test uses the real instant too
    // instead of inventing a `now`.
    let now = now_secs();
    let idle = WATCH_IDLE_STOP.as_secs();

    let mk = |last_active: u64| WatchLive {
        info: SessionInfo {
            session_id: "s".into(),
            workspace_id: "ws".into(),
            project_id: None,
            runtime: "claude-code".into(),
            agent: None,
            branch: None,
            status: SessionStatus::Running,
            last_seq: 0,
            gist: None,
            dangerous: false,
            permission_mode: None,
            created_at: String::new(),
            updated_at: String::new(),
        },
        // This case only looks at the test; what the task itself runs does not matter. `spawn`
        // needs a runtime, so it borrows one built on the spot.
        handle: {
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime");
            let _guard = rt.enter();
            tokio::spawn(std::future::ready(()))
        },
        active: std::sync::Arc::new(AtomicU64::new(last_active)),
        viewers: Default::default(),
        generation: 1,
    };

    let mut watches: HashMap<String, WatchLive> = HashMap::new();
    // One tick short of expiry: not swept yet.
    watches.insert("almost".into(), mk(now - idle + 1));
    // Exactly at expiry: swept.
    watches.insert("expired".into(), mk(now - idle));
    let stale = stale_watches(&watches, now);
    assert_eq!(stale, vec!["expired".to_string()]);

    // Someone joins the one that is **exactly at expiry** before the sweep happens
    // (`session.watch` performs this write, under the same lock as the sweep) — so it survives.
    // Through the **production** renewal entry point, not a hand-written store — a hand-written
    // one stays green on the day production code stops writing it this way.
    watches.get("expired").unwrap().renew();
    assert!(
        stale_watches(&watches, now).is_empty(),
        "a viewer joining at the critical moment hangs on a tail about to be swept"
    );
}

/// A confinement refresh **must not** be dropped because "no session is listening right now".
///
/// `watch::Sender::send` returns Err with no receiver, and does not write the new value then.
/// Once every session in a workspace has ended the receivers are gone while the sender stays:
/// every refresh after that (`agit rc grant`, unbinding a directory) spins with no effect, the
/// value stays at the old one, and the next session's `subscribe()` gets exactly that. This value
/// is the whole basis for confinement holding.
#[test]
fn a_confinement_update_lands_even_when_no_session_is_listening() {
    let temp = tempfile::tempdir().unwrap();
    for name in ["old", "new"] {
        std::fs::create_dir(temp.path().join(name)).unwrap();
    }
    let test_roots =
        |name: &str| crate::rc::policy::CanonicalRoots::from_untrusted([temp.path().join(name)]);
    let mut mirror = Mirror::default();
    mirror
        .bind("ws-1", "project-1", &temp.path().join("old"))
        .unwrap();
    let (notes, _notes_rx) = mpsc::channel(1);
    let (settlement, _) = tokio::sync::watch::channel(SettlementState::default());
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
        mirror,
        roster: Roster::default(),
        sessions: HashMap::new(),
        latest_session_generations: HashMap::new(),
        watches: HashMap::new(),
        terminals: HashMap::new(),
        terminal_delivery_blockers: Default::default(),
        term_tx: None,
        online: false,
        connection_id: None,
        secret_filter: Default::default(),
        settlement,
        started_at: std::time::Instant::now(),
        notes,
        grants: crate::rc::grants::Grants::default(),
        watch_generation: 0,
        session_generation: 0,
        confinement: HashMap::new(),
    };

    let rx = daemon.confinement_for("ws-1");
    assert_eq!(rx.borrow().roots, test_roots("old"));
    // The sessions end: the receiver is gone.
    drop(rx);

    daemon.mirror.unbind("ws-1", "project-1");
    daemon
        .mirror
        .bind("ws-1", "project-1", &temp.path().join("new"))
        .unwrap();
    // Through the production refresh entry point; it must **land in the value** even with
    // nobody listening.
    daemon.refresh_confinement();

    // Subscribe directly to the sender in the production map. Calling `confinement_for` again
    // also refreshes the value, which would mask a `refresh_confinement` that has slipped back
    // to `send`.
    let fresh = daemon.confinement["ws-1"].subscribe();
    assert_eq!(
        fresh.borrow().roots,
        test_roots("new"),
        "a new session got stale confinement roots"
    );
}

#[test]
fn the_session_bridge_overwrites_both_local_provenance_fields() {
    let mut frame = Frame::notification(method::ITEM_DELTA, serde_json::json!({}));
    frame.stream = Some("forged-stream".into());
    frame.source_generation = Some(999);

    tag_session_frame(&mut frame, "session-a", 7);

    assert_eq!(frame.stream.as_deref(), Some("session-a"));
    assert_eq!(frame.source_generation, Some(7));
}

#[tokio::test]
async fn stale_generation_is_dropped_before_every_session_projection_side_effect() {
    use crate::protocol::{PermissionApply, PermissionMode};

    let (command_tx, _command_rx) = mpsc::channel(1);
    let mut live = rpc_test_live("session-a", 2, command_tx, PermissionMode::Auto);
    live.approval_session_modes
        .insert("keep".into(), PermissionMode::Plan);
    let daemon = rpc_test_daemon(
        [("session-a".into(), live)].into_iter().collect(),
        Roster::default(),
    );
    let mut state = daemon.lock().await;
    state
        .latest_session_generations
        .insert("session-a".into(), 2);

    let mut frames = vec![
        tagged_test_notification(
            "session-a",
            1,
            method::SESSION_PERMISSION_MODE,
            serde_json::to_value(crate::protocol::SessionPermissionMode {
                session_id: "session-a".into(),
                mode: PermissionMode::Bypass,
                applied: PermissionApply::Immediate,
                by: Some("owner".into()),
            })
            .unwrap(),
        ),
        tagged_test_notification(
            "session-a",
            1,
            method::SESSION_STATUS,
            serde_json::to_value(crate::protocol::SessionStatusChanged {
                session_id: "session-a".into(),
                status: SessionStatus::Ended,
            })
            .unwrap(),
        ),
        tagged_test_notification(
            "session-a",
            1,
            method::APPROVAL_REQUEST,
            serde_json::to_value(test_approval("stale")).unwrap(),
        ),
        tagged_test_notification(
            "session-a",
            1,
            method::TURN_COMPLETED,
            serde_json::json!({"turn_id":"turn-a","outcome":"ok"}),
        ),
        tagged_test_notification(
            "session-a",
            1,
            method::ITEM_DELTA,
            serde_json::json!({"item_id":"item-a","text":"stale"}),
        ),
        tagged_test_notification(
            "session-a",
            1,
            method::COMMIT_SETTLED,
            serde_json::json!({"session_id":"session-a","commit_sha":"old"}),
        ),
    ];
    let mut deliveries = Vec::new();
    for frame in &mut frames {
        let delivery = crate::protocol::ConnectionDelivery::new(
            7,
            crate::protocol::ConnectionFeature::AgentIdentityV1,
        );
        frame.connection_delivery = Some(delivery.clone());
        deliveries.push(delivery);
    }
    for frame in frames {
        assert!(needs_session_projection(&frame));
        assert!(state.project_session_frame(frame).is_none());
    }

    let live = state.sessions.get("session-a").unwrap();
    assert_eq!(live.info.permission_mode, Some(PermissionMode::Auto));
    assert_eq!(live.info.status, SessionStatus::Running);
    assert_eq!(
        live.approval_session_modes,
        [("keep".into(), PermissionMode::Plan)]
            .into_iter()
            .collect()
    );
    assert_eq!(state.journal.last_seq("session-a"), 0);
    assert!(
        deliveries
            .iter()
            .all(|delivery| { delivery.status() == crate::protocol::DeliveryStatus::Stale })
    );
}

#[tokio::test]
async fn current_generation_projects_every_session_fact_and_scrubs_its_tag() {
    use crate::protocol::{PermissionApply, PermissionMode};

    let (command_tx, _command_rx) = mpsc::channel(1);
    let live = rpc_test_live("session-a", 2, command_tx, PermissionMode::Auto);
    let daemon = rpc_test_daemon(
        [("session-a".into(), live)].into_iter().collect(),
        Roster::default(),
    );
    let mut state = daemon.lock().await;
    state
        .latest_session_generations
        .insert("session-a".into(), 2);

    let item = state
        .project_session_frame(tagged_test_notification(
            "session-a",
            2,
            method::ITEM_DELTA,
            serde_json::json!({"item_id":"item-a","text":"fresh"}),
        ))
        .expect("the current generation is accepted");
    assert_eq!(item.seq, Some(1));
    assert_eq!(item.source_generation, None);
    let (replay, _) = state.journal.replay("session-a", 0);
    assert_eq!(replay.len(), 1);
    assert_eq!(replay[0].source_generation, None);

    let permission = tagged_test_notification(
        "session-a",
        2,
        method::SESSION_PERMISSION_MODE,
        serde_json::to_value(crate::protocol::SessionPermissionMode {
            session_id: "session-a".into(),
            mode: PermissionMode::Plan,
            applied: PermissionApply::Immediate,
            by: Some("owner".into()),
        })
        .unwrap(),
    );
    assert!(state.project_session_frame(permission).is_some());
    assert_eq!(
        state.sessions["session-a"].info.permission_mode,
        Some(PermissionMode::Plan)
    );

    let approval = tagged_test_notification(
        "session-a",
        2,
        method::APPROVAL_REQUEST,
        serde_json::to_value(test_approval("fresh")).unwrap(),
    );
    assert!(state.project_session_frame(approval).is_some());
    assert_eq!(
        state.sessions["session-a"]
            .approval_session_modes
            .get("fresh"),
        Some(&PermissionMode::Bypass)
    );

    let completion = tagged_test_notification(
        "session-a",
        2,
        method::TURN_COMPLETED,
        serde_json::json!({"turn_id":"turn-a","outcome":"ok"}),
    );
    assert!(state.project_session_frame(completion).is_some());
    assert!(
        state.sessions["session-a"]
            .approval_session_modes
            .is_empty()
    );

    let status = tagged_test_notification(
        "session-a",
        2,
        method::SESSION_STATUS,
        serde_json::to_value(crate::protocol::SessionStatusChanged {
            session_id: "session-a".into(),
            status: SessionStatus::Ended,
        })
        .unwrap(),
    );
    assert!(state.project_session_frame(status).is_some());
    assert_eq!(
        state.sessions["session-a"].info.status,
        SessionStatus::Ended
    );

    set_connection_features(&state.settlement, 7, true, true);
    let delivery = crate::protocol::ConnectionDelivery::new(
        7,
        crate::protocol::ConnectionFeature::AgentIdentityV1,
    );
    let mut commit = tagged_test_notification(
        "session-a",
        2,
        method::COMMIT_SETTLED,
        serde_json::json!({"session_id":"session-a","commit_sha":"fresh"}),
    );
    commit.connection_delivery = Some(delivery.clone());
    let commit = state
        .project_session_frame(commit)
        .expect("a current connection-bound commit survives both fences");
    assert_eq!(commit.params.unwrap()["through_seq"], 5);
    assert_eq!(commit.seq, Some(6));
    assert_eq!(delivery.status(), crate::protocol::DeliveryStatus::Pending);
}

#[tokio::test]
async fn ended_and_resume_cross_channel_order_obeys_the_materialized_tombstone() {
    use crate::protocol::PermissionMode;

    let (first_tx, _first_rx) = mpsc::channel(1);
    let first = rpc_test_live("session-a", 1, first_tx, PermissionMode::Auto);
    let daemon = rpc_test_daemon(
        [("session-a".into(), first)].into_iter().collect(),
        Roster::default(),
    );
    let mut state = daemon.lock().await;
    state
        .latest_session_generations
        .insert("session-a".into(), 1);

    state.on_session_note(SessionNote::Ended {
        session_id: "session-a".into(),
        generation: 1,
    });
    assert!(!state.sessions.contains_key("session-a"));
    assert_eq!(state.latest_session_generations["session-a"], 1);
    assert!(
        state
            .project_session_frame(tagged_test_notification(
                "session-a",
                1,
                method::ITEM_COMPLETED,
                serde_json::json!({"item_id":"final-old"}),
            ))
            .is_some(),
        "Ended may win its channel race before that generation's final frame"
    );
    assert_eq!(state.journal.last_seq("session-a"), 1);

    let (second_tx, _second_rx) = mpsc::channel(1);
    state.sessions.insert(
        "session-a".into(),
        rpc_test_live("session-a", 2, second_tx, PermissionMode::Plan),
    );
    state
        .latest_session_generations
        .insert("session-a".into(), 2);
    assert!(
        state
            .project_session_frame(tagged_test_notification(
                "session-a",
                1,
                method::ITEM_DELTA,
                serde_json::json!({"text":"late old"}),
            ))
            .is_none(),
        "materializing generation 2 permanently fences generation 1"
    );
    assert_eq!(state.journal.last_seq("session-a"), 1);

    state.on_session_note(SessionNote::Ended {
        session_id: "session-a".into(),
        generation: 2,
    });
    assert!(!state.sessions.contains_key("session-a"));
    assert_eq!(state.latest_session_generations["session-a"], 2);
    assert!(
        state
            .project_session_frame(tagged_test_notification(
                "session-a",
                1,
                method::ITEM_DELTA,
                serde_json::json!({"text":"later old"}),
            ))
            .is_none(),
        "ending generation 2 must not erase its tombstone and revive generation 1"
    );
    assert!(
        state
            .project_session_frame(tagged_test_notification(
                "session-a",
                2,
                method::ITEM_COMPLETED,
                serde_json::json!({"item_id":"final-new"}),
            ))
            .is_some(),
        "generation 2 may still drain its own final frame after Ended"
    );
    assert_eq!(state.journal.last_seq("session-a"), 2);
}

#[tokio::test]
async fn tagged_shape_errors_and_presequenced_frames_cannot_bypass_the_fence() {
    let daemon = rpc_test_daemon(HashMap::new(), Roster::default());
    let mut state = daemon.lock().await;
    state
        .latest_session_generations
        .insert("session-a".into(), 2);

    let mut missing_stream = Frame::notification(method::ITEM_DELTA, serde_json::json!({}));
    missing_stream.source_generation = Some(2);
    let mut request = Frame::request(method::TURN_START, serde_json::json!({}));
    tag_session_frame(&mut request, "session-a", 2);
    let mut presequenced =
        tagged_test_notification("session-a", 2, method::ITEM_DELTA, serde_json::json!({}));
    presequenced.seq = Some(99);
    let stale = tagged_test_notification("session-a", 1, method::ITEM_DELTA, serde_json::json!({}));

    let mut cases = vec![missing_stream, request, presequenced, stale];
    let mut deliveries = Vec::new();
    for frame in &mut cases {
        let delivery = crate::protocol::ConnectionDelivery::new(
            1,
            crate::protocol::ConnectionFeature::AgentIdentityV1,
        );
        frame.connection_delivery = Some(delivery.clone());
        deliveries.push(delivery);
        assert!(
            needs_session_projection(frame),
            "every tagged shape must reach the projection fence"
        );
    }

    let mut shutdown_tail = ShutdownProjectionTail {
        frames: cases.len(),
        notes: 0,
    };
    for frame in cases {
        shutdown_tail.took_frame();
        assert!(state.project_session_frame(frame).is_none());
    }
    assert!(
        shutdown_tail.complete(),
        "dropped frames still consume the fixed shutdown prefix"
    );
    assert_eq!(state.journal.last_seq("session-a"), 0);
    assert!(
        deliveries
            .iter()
            .all(|delivery| { delivery.status() == crate::protocol::DeliveryStatus::Stale })
    );
}

#[tokio::test]
async fn untagged_terminal_watch_rpc_and_replay_paths_remain_compatible() {
    let daemon = rpc_test_daemon(HashMap::new(), Roster::default());
    let mut state = daemon.lock().await;

    for stream in ["term:workspace-a", "watch:session-a"] {
        let mut fresh = Frame::notification(method::ITEM_DELTA, serde_json::json!({}));
        fresh.stream = Some(stream.into());
        assert_eq!(fresh.source_generation, None);
        assert!(needs_session_projection(&fresh));
        let projected = state
            .project_session_frame(fresh)
            .expect("untagged local producers retain their existing path");
        assert_eq!(projected.seq, Some(1));
    }

    let replay = Frame::event(
        "legacy-session",
        1,
        method::ITEM_DELTA,
        serde_json::json!({}),
    );
    let response = Frame::response(
        crate::protocol::RequestId::Num(1),
        serde_json::json!({"ok":true}),
    );
    let request = Frame::request(method::SESSION_LIST, serde_json::json!({}));
    assert!(!needs_session_projection(&replay));
    assert!(!needs_session_projection(&response));
    assert!(!needs_session_projection(&request));
}

#[tokio::test]
async fn failed_launch_does_not_advance_the_materialized_generation_tombstone() {
    let daemon = rpc_test_daemon(HashMap::new(), Roster::default());
    let mut state = daemon.lock().await;
    state
        .latest_session_generations
        .insert("session-a".into(), 1);
    let info = SessionInfo {
        session_id: "session-a".into(),
        workspace_id: "ws-a".into(),
        project_id: None,
        runtime: "unsupported-test-runtime".into(),
        agent: None,
        branch: None,
        status: SessionStatus::Idle,
        last_seq: 0,
        gist: None,
        dangerous: false,
        permission_mode: Some(crate::protocol::PermissionMode::Plan),
        created_at: "now".into(),
        updated_at: "now".into(),
    };
    let spec = LaunchSpec {
        cwd: std::path::PathBuf::from("/"),
        resume_from: None,
        agit_session: None,
        model: None,
        dangerous: false,
        permission_mode: Some(crate::protocol::PermissionMode::Plan),
    };
    let (frames, _frames_rx) = mpsc::channel(4);

    assert!(
        state
            .spawn_session(
                info,
                spec,
                danger::TranscriptDanger::fresh_transcript(),
                &frames,
                None,
                None,
            )
            .await
            .is_err()
    );
    assert_eq!(
        state.session_generation, 3,
        "the failed attempt is consumed"
    );
    assert_eq!(
        state.latest_session_generations["session-a"], 1,
        "only a completed Session::launch may advance the tombstone"
    );
    assert!(!state.sessions.contains_key("session-a"));
    assert!(
        state
            .project_session_frame(tagged_test_notification(
                "session-a",
                1,
                method::ITEM_COMPLETED,
                serde_json::json!({"item_id":"old-final"}),
            ))
            .is_some(),
        "a failed replacement must not fence the last materialized generation"
    );
}
