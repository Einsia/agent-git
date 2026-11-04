use super::*;
use crate::rc::lease::State;

const THREAD: &str = "11111111-2222-4333-8444-555555555555";

/// A daemon running `THREAD` headless as `agit-h`, in `status`; the receiver sees its commands.
fn headless_daemon(status: SessionStatus) -> (Arc<Mutex<Daemon>>, mpsc::Receiver<Command>) {
    let (tx, commands) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
    let mut live = rpc_test_live("agit-h", 1, tx, crate::protocol::PermissionMode::Default);
    live.info.runtime = "claude-code".into();
    live.info.status = status;
    live.runtime_thread_id = Some(THREAD.into());
    let daemon = rpc_test_daemon(HashMap::from([("agit-h".into(), live)]), Roster::default());
    (daemon, commands)
}

fn event(method: &str) -> Frame {
    let mut frame = Frame::notification(method, serde_json::json!({"session_id": "agit-h"}));
    frame.stream = Some("agit-h".into());
    frame
}

/// The test process stands in for the terminal: it started before anything below happens.
fn open(state: &mut Daemon) -> crate::rc::control::Reply {
    state.terminal_opened(THREAD, std::process::id(), true, None)
}

fn stopped(commands: &mut mpsc::Receiver<Command>) -> bool {
    std::iter::from_fn(|| commands.try_recv().ok())
        .any(|command| matches!(command, Command::Shutdown))
}

/// The supervisor's status as it stops the writer, before reaping it.
fn ended_status(state: &mut Daemon) {
    let mut status = Frame::notification(
        method::SESSION_STATUS,
        serde_json::json!({"session_id": "agit-h", "status": "ended"}),
    );
    status.stream = Some("agit-h".into());
    state.project_session_frame(status);
}

fn note(state: &mut Daemon, note: SessionNote) -> crate::rc::lease::View {
    state.on_session_note(note);
    state.leases.get(THREAD).unwrap().view(None)
}

fn released(in_turn: bool) -> SessionNote {
    SessionNote::WriterReleased {
        session_id: "agit-h".into(),
        generation: 1,
        thread: Some(THREAD.into()),
        in_turn,
    }
}

fn not_released() -> SessionNote {
    SessionNote::WriterNotReleased {
        session_id: "agit-h".into(),
        generation: 1,
        thread: Some(THREAD.into()),
        process: None,
    }
}

/// Whether the writer is inside a turn, as its supervisor records when it stops.
fn in_turn(state: &Daemon) -> bool {
    state.sessions.get("agit-h").is_some_and(|live| {
        matches!(
            live.info.status,
            SessionStatus::Running | SessionStatus::AwaitingApproval
        )
    })
}

fn supervisor_ended() -> SessionNote {
    SessionNote::Ended {
        session_id: "agit-h".into(),
        generation: 1,
    }
}

/// The writer's exit as the daemon sees it: its status, the reap, then its supervisor's end.
fn ended(state: &mut Daemon) -> crate::rc::lease::View {
    let in_turn = in_turn(state);
    ended_status(state);
    note(state, released(in_turn));
    note(state, supervisor_ended())
}

fn run(test: impl FnOnce(&mut Daemon, &mut mpsc::Receiver<Command>), status: SessionStatus) {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (daemon, mut commands) = headless_daemon(status);
                let mut state = daemon.lock().await;
                test(&mut state, &mut commands);
            })
    });
}

/// An idle headless writer stops as soon as a terminal opens its conversation, and the
/// terminal holds it afterwards; the web sees the terminal as the holder. No web turn is running,
/// so the terminal is told nothing about one, for a terminal with the plugin or without.
#[test]
fn an_idle_headless_writer_hands_over_at_once() {
    run(
        |state, commands| {
            let crate::rc::control::Reply::TerminalOpened {
                state: lease,
                notice,
            } = open(state)
            else {
                panic!("a terminal announcement is answered with its lease");
            };
            assert_eq!(lease, Some(State::Releasing));
            assert_eq!(notice, None, "an idle writer runs no turn to wait for");
            assert!(stopped(commands), "idle: the headless writer stops at once");
            let releasing = state.lease_view("agit-h").unwrap();
            assert_eq!(
                (
                    releasing.holder.as_str(),
                    releasing.state,
                    releasing.takeable
                ),
                ("headless", State::Releasing, false),
                "until it ends, the web sees the handover and queues"
            );
            let view = ended(state);
            assert_eq!((view.holder.as_str(), view.state), ("tui", State::Held));
            assert_eq!(
                view.reason,
                crate::rc::lease::Reason::HeadlessEnded,
                "a writer that reported its end"
            );
            assert!(
                view.takeable,
                "the web may send through the terminal's plugin"
            );
        },
        SessionStatus::Idle,
    );
    run(
        |state, _| {
            let crate::rc::control::Reply::TerminalOpened { notice, .. } =
                state.terminal_opened(THREAD, std::process::id(), false, None)
            else {
                panic!("a terminal announcement is answered with its lease");
            };
            assert_eq!(
                notice, None,
                "a terminal without the plugin is not sent away either"
            );
        },
        SessionStatus::Idle,
    );
}

/// A writer in the middle of a turn finishes it first; the terminal loaded the conversation
/// before that turn's end, so it must resume again before continuing, and its typing waits.
#[test]
fn a_running_headless_turn_finishes_before_the_terminal_takes_over() {
    run(
        |state, commands| {
            let crate::rc::control::Reply::TerminalOpened { notice, .. } = open(state) else {
                panic!("a terminal announcement is answered with its lease");
            };
            assert!(notice.is_some(), "the terminal is told why its input waits");
            assert!(!stopped(commands), "a running turn is not cut off");
            state.project_session_frame(event(method::ITEM_COMPLETED));
            assert!(!stopped(commands));
            state.project_session_frame(event(method::TURN_COMPLETED));
            assert!(stopped(commands), "the writer stops at its turn boundary");
            assert_eq!(ended(state).state, State::Stale);
        },
        SessionStatus::Running,
    );
}

/// The writer's report of its turn's last record decides a terminal that names its branch: one
/// continuing that record holds the conversation although the writer's output came after the
/// terminal started, and one on another branch is stale. A report is forgotten once the writer
/// starts another turn, and the start time judges again.
#[test]
fn the_writers_last_record_decides_a_terminal_that_names_its_branch() {
    for (report_then_start, leaf, expected) in [
        (false, "a2", State::Held),
        (false, "f1", State::Stale),
        (true, "a2", State::Stale),
    ] {
        run(
            |state, _| {
                let directory = tempfile::tempdir().unwrap();
                let transcript = directory.path().join("conversation.jsonl");
                std::fs::write(
                    &transcript,
                    [
                        r#"{"uuid":"a1","parentUuid":null}"#,
                        r#"{"uuid":"a2","parentUuid":"a1"}"#,
                        r#"{"uuid":"f1","parentUuid":"a1"}"#,
                    ]
                    .join("\n"),
                )
                .unwrap();
                state.project_session_frame(event(method::TURN_STARTED));
                state.writer_leaf(THREAD, "a2");
                if report_then_start {
                    state.project_session_frame(event(method::TURN_STARTED));
                }
                state.project_session_frame(event(method::TURN_COMPLETED));
                state.terminal_opened(
                    THREAD,
                    std::process::id(),
                    true,
                    Some(crate::rc::lease::Loaded {
                        leaf: leaf.into(),
                        transcript,
                    }),
                );
                assert_eq!(ended(state).state, expected, "{leaf}, {report_then_start}");
            },
            SessionStatus::Idle,
        );
    }
}

/// A daemon that stops during a headless turn, with no terminal waiting, comes back without the
/// previous turn's last record: a terminal that loaded the conversation up to that record lacks
/// the turn that was running, and is held back.
#[test]
fn a_restart_during_a_headless_turn_forgets_the_previous_turns_last_record() {
    run(
        |state, _| {
            let directory = tempfile::tempdir().unwrap();
            let transcript = directory.path().join("conversation.jsonl");
            std::fs::write(
                &transcript,
                [
                    r#"{"uuid":"a1","parentUuid":null}"#,
                    r#"{"uuid":"a2","parentUuid":"a1"}"#,
                    r#"{"uuid":"u3","parentUuid":"a2"}"#,
                ]
                .join("\n"),
            )
            .unwrap();
            state.project_session_frame(event(method::TURN_STARTED));
            state.writer_leaf(THREAD, "a2");
            state.project_session_frame(event(method::TURN_COMPLETED));
            state.project_session_frame(event(method::TURN_STARTED));

            let mut restarted = Daemon::load_leases();
            let terminal = crate::rc::lease::Terminal {
                pid: std::process::id(),
                started_ms: chrono::Utc::now().timestamp_millis() + 60_000,
                plugin: true,
            };
            assert_eq!(
                restarted.opened(
                    THREAD,
                    terminal,
                    Some(crate::rc::lease::Loaded {
                        leaf: "a2".into(),
                        transcript,
                    }),
                    None,
                    &crate::rc::lease::descends,
                    chrono::Utc::now().timestamp_millis() + 61_000,
                ),
                crate::rc::lease::Opened::Granted(State::Stale)
            );
        },
        SessionStatus::Idle,
    );
}

/// A process standing in for another terminal or a writer, with its start time.
fn other_process() -> (std::process::Child, crate::rc::lease::Terminal) {
    let child = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    let started_ms = crate::rc::native_claude::started_ms(child.id()).unwrap();
    let terminal = crate::rc::lease::Terminal {
        pid: child.id(),
        started_ms,
        plugin: true,
    };
    (child, terminal)
}

/// Sim run 5e: the writer is reaped and hands over, but its supervisor goes on settling, and
/// its row stays until it ends. A terminal opening in between waits on no writer: that writer
/// is gone, and no second release would ever come.
#[test]
fn a_terminal_opening_while_the_released_writer_settles_waits_on_nothing() {
    run(
        |state, commands| {
            open(state);
            assert!(stopped(commands));
            ended_status(state);
            assert_eq!(note(state, released(false)).state, State::Held);
            assert!(state.sessions.contains_key("agit-h"), "still settling");
            std::thread::sleep(std::time::Duration::from_millis(50));

            let (mut child, other) = other_process();
            state.terminal_opened(THREAD, other.pid, true, None);
            let lease = state.leases.get(THREAD).unwrap();
            assert!(
                matches!(lease.holder, crate::rc::lease::Holder::Tui(terminal) if terminal.pid == other.pid),
                "{lease:?}"
            );
            assert_eq!(lease.state, State::Held);
            assert!(!stopped(commands));
            note(state, supervisor_ended());
            let _ = child.kill();
            let _ = child.wait();
        },
        SessionStatus::Idle,
    );
}

/// The release note never comes, and the writer's row is retired: at the deadline sweep the
/// waiting terminal takes over as stale once the writer process recorded for the handover is
/// gone, and not while it runs.
#[test]
fn a_handover_whose_writer_vanished_resolves_stale_at_the_deadline_sweep() {
    run(
        |state, commands| {
            let (mut writer, recorded) = other_process();
            state.sessions.get_mut("agit-h").unwrap().writer_process = Some(recorded);
            open(state);
            assert!(stopped(commands));
            assert_eq!(state.leases.get(THREAD).unwrap().writer, Some(recorded));
            ended_status(state);
            note(state, supervisor_ended());
            assert!(!state.sessions.contains_key("agit-h"), "the row is retired");
            state.leases.lease_mut(THREAD).unwrap().deadline_ms = Some(0);

            state.enforce_lease_deadlines();
            assert_eq!(
                state.leases.get(THREAD).unwrap().state,
                State::Releasing,
                "the writer process still runs"
            );
            let _ = writer.kill();
            let _ = writer.wait();
            state.enforce_lease_deadlines();
            let lease = state.leases.get(THREAD).unwrap();
            assert_eq!(lease.state, State::Stale);
            assert_eq!(
                lease.reason,
                crate::rc::lease::Reason::HeadlessEndedUnobserved
            );
            assert!(
                matches!(lease.holder, crate::rc::lease::Holder::Tui(terminal) if terminal.pid == std::process::id())
            );
        },
        SessionStatus::Idle,
    );
}

/// While a headless writer hands over, web input is refused as not sent and names the lease;
/// it is never delivered into the turn that is ending. A stale terminal refuses it too: it would
/// continue the conversation from the older history that terminal loaded.
#[test]
fn web_input_is_refused_with_the_lease_until_a_terminal_holds_the_conversation() {
    run(
        |state, _| {
            open(state);
            let data = state
                .lease_refusal("agit-h")
                .expect("a releasing writer refuses input")
                .data
                .unwrap();
            assert_eq!(data["reason"], "lease_releasing");
            assert_eq!(data["retryable"], false);
            assert_eq!(data["lease"]["state"], "releasing");

            state.project_session_frame(event(method::TURN_COMPLETED));
            ended(state);
            // The web now reaches the conversation through the terminal's shared session.
            let (tx, _commands) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
            let mut shared =
                rpc_test_live("agit-t", 1, tx, crate::protocol::PermissionMode::Default);
            shared.info.runtime = "claude-code".into();
            shared.shared_executor = true;
            shared.runtime_thread_id = Some(THREAD.into());
            state.sessions.insert("agit-t".into(), shared);
            let data = state.lease_refusal("agit-t").unwrap().data.unwrap();
            assert_eq!(data["reason"], "lease_stale");
            assert_eq!(data["lease"]["state"], "stale");
        },
        SessionStatus::Running,
    );
}

/// The supervisor reports the writer ended before it reaps the process, which may still be
/// exiting and appending its last records then. Only the reap hands the conversation over; it
/// comes before settlement and publication, so the supervisor's own end is not waited for.
#[test]
fn the_terminal_stays_held_until_the_writer_is_reaped() {
    for status in [SessionStatus::Idle, SessionStatus::Running] {
        run(
            |state, commands| {
                open(state);
                if status == SessionStatus::Running {
                    state.project_session_frame(event(method::TURN_COMPLETED));
                }
                assert!(stopped(commands));
                ended_status(state);
                let waiting = state.leases.get(THREAD).unwrap().view(None);
                assert_eq!(
                    (waiting.holder.as_str(), waiting.state, waiting.takeable),
                    ("headless", State::Releasing, false),
                    "an ended status before the reap does not open the conversation"
                );
                assert!(state.lease_refusal("agit-h").is_some());
                let lease = note(state, released(false));
                assert_eq!(lease.holder, "tui");
                let epoch = lease.epoch;
                assert_eq!(note(state, supervisor_ended()).epoch, epoch);
            },
            status,
        );
    }
}

/// The periodic reconciliation may retire a finished supervisor's row before the daemon reads
/// that supervisor's queued release; the release still hands the conversation over.
#[test]
fn a_release_read_after_the_row_is_retired_still_hands_over() {
    run(
        |state, commands| {
            open(state);
            assert!(stopped(commands));
            ended_status(state);
            note(state, supervisor_ended());
            assert!(
                !state.sessions.contains_key("agit-h"),
                "the row is retired first"
            );
            let lease = note(state, released(false));
            assert_eq!((lease.holder.as_str(), lease.state), ("tui", State::Held));
        },
        SessionStatus::Idle,
    );
}

/// A writer whose cleanup failed reports it: the conversation stays with it, even after its
/// supervisor ends, rather than going to a terminal while the process may still write. The
/// lease says so, and the waiting terminal is told the way out instead of waiting forever. The
/// supervisor's end alone says nothing about the reap and changes nothing.
#[test]
fn a_writer_never_reaped_keeps_the_conversation_and_says_so() {
    run(
        |state, commands| {
            open(state);
            assert!(stopped(commands));
            ended_status(state);
            let lease = note(state, supervisor_ended());
            assert_eq!(
                (lease.holder.as_str(), lease.state),
                ("headless", State::Releasing)
            );
            let lease = note(state, not_released());
            assert_eq!(
                (lease.holder.as_str(), lease.state, lease.takeable),
                ("headless", State::ReleaseFailed, false)
            );
            let crate::rc::control::Reply::TerminalOpened {
                state: answered,
                notice,
            } = open(state)
            else {
                panic!("a terminal announcement is answered with its lease");
            };
            assert_eq!(answered, Some(State::ReleaseFailed));
            let notice = notice.expect("the waiting terminal is told why it cannot take over");
            assert!(notice.contains("agit rc stop"), "{notice}");
            assert!(
                notice.contains(&format!("claude --resume {THREAD}")),
                "{notice}"
            );
        },
        SessionStatus::Idle,
    );
}

/// After a failed release the old process may still write. A different terminal process that
/// opens the conversation, and a web resume of it, are refused the conversation as well.
#[cfg(unix)]
#[test]
fn a_failed_release_binds_a_terminal_that_opens_the_conversation_later() {
    run(
        |state, commands| {
            open(state);
            assert!(stopped(commands));
            ended_status(state);
            note(state, supervisor_ended());
            note(state, not_released());
            let other = std::os::unix::process::parent_id();
            let crate::rc::control::Reply::TerminalOpened {
                state: answered,
                notice,
            } = state.terminal_opened(THREAD, other, true, None)
            else {
                panic!("a terminal announcement is answered with its lease");
            };
            assert_eq!(answered, Some(State::ReleaseFailed));
            assert!(notice.is_some_and(|notice| notice.contains("agit rc stop")));
            let lease = state.leases.get(THREAD).unwrap().view(None);
            assert_eq!((lease.holder.as_str(), lease.takeable), ("headless", false));
            let refusal = state.release_failed_refusal(THREAD).unwrap();
            assert_eq!(refusal.data.unwrap()["reason"], "lease_release_failed");
        },
        SessionStatus::Idle,
    );
}

/// A failed cleanup of a headless writer nobody was waiting for still leaves the failure on
/// record: a terminal that opens the conversation later, and a web resume of it, are refused.
#[test]
fn a_failed_cleanup_with_nobody_waiting_binds_the_next_opener() {
    run(
        |state, _| {
            ended_status(state);
            state.on_session_note(supervisor_ended());
            assert!(state.leases.get(THREAD).is_none(), "nobody was waiting");
            assert_eq!(note(state, not_released()).state, State::ReleaseFailed);
            let crate::rc::control::Reply::TerminalOpened {
                state: answered,
                notice,
            } = open(state)
            else {
                panic!("a terminal announcement is answered with its lease");
            };
            assert_eq!(answered, Some(State::ReleaseFailed));
            assert!(notice.is_some());
            assert!(state.release_failed_refusal(THREAD).is_some());
        },
        SessionStatus::Idle,
    );
}

/// A writer that never reported its conversation still reaches the lease it holds: a failed
/// cleanup is never left looking like an ordinary handover in progress, and a release still
/// hands over.
#[test]
fn a_reap_outcome_without_a_thread_finds_the_lease_by_its_holder() {
    run(
        |state, commands| {
            open(state);
            assert!(stopped(commands));
            ended_status(state);
            let lease = note(
                state,
                SessionNote::WriterNotReleased {
                    session_id: "agit-h".into(),
                    generation: 1,
                    thread: None,
                    process: Some((std::process::id(), 0)),
                },
            );
            assert_eq!(lease.state, State::ReleaseFailed);
            assert_eq!(
                state
                    .leases
                    .get(THREAD)
                    .unwrap()
                    .writer
                    .map(|writer| writer.pid),
                Some(std::process::id())
            );
        },
        SessionStatus::Idle,
    );
    run(
        |state, commands| {
            open(state);
            assert!(stopped(commands));
            ended_status(state);
            let lease = note(
                state,
                SessionNote::WriterReleased {
                    session_id: "agit-h".into(),
                    generation: 1,
                    thread: None,
                    in_turn: false,
                },
            );
            assert_eq!(lease.holder, "tui");
        },
        SessionStatus::Idle,
    );
}

/// The terminal that resumes an idle conversation writes its own records to the transcript the
/// headless writer tails; those are not the writer's output and leave the terminal current.
/// A writer that ends inside a turn may have written up to its end, so the terminal is stale.
#[test]
fn only_the_writer_s_own_turns_make_the_terminal_stale() {
    run(
        |state, commands| {
            open(state);
            assert!(stopped(commands));
            state.project_session_frame(event(method::ITEM_COMPLETED));
            assert_eq!(ended(state).state, State::Held);
        },
        SessionStatus::Idle,
    );
    run(
        |state, _| {
            open(state);
            assert_eq!(ended(state).state, State::Stale);
        },
        SessionStatus::Running,
    );
}

/// A pending approval belongs to the running turn: the writer is not stopped while it waits,
/// so the approval can still be answered, and the handover follows that turn's end.
#[test]
fn a_pending_approval_holds_the_handover_until_its_turn_ends() {
    run(
        |state, commands| {
            open(state);
            assert!(
                !stopped(commands),
                "an open approval is not abandoned by the handover"
            );
            assert!(state.leases.releasing(THREAD));
            state.project_session_frame(event(method::APPROVAL_RESOLVED));
            assert!(
                !stopped(commands),
                "answering the approval does not end the turn"
            );
            state.project_session_frame(event(method::TURN_COMPLETED));
            assert!(stopped(commands));
            assert_eq!(ended(state).holder, "tui");
        },
        SessionStatus::AwaitingApproval,
    );
}

/// The race between Claude Code loading the history and the terminal announcing itself: a
/// headless turn ending in between leaves the terminal stale, although the writer is idle when
/// the announcement arrives.
#[test]
fn a_turn_ending_between_terminal_start_and_announcement_leaves_it_stale() {
    run(
        |state, commands| {
            state.project_session_frame(event(method::TURN_COMPLETED));
            open(state);
            assert!(stopped(commands));
            assert_eq!(ended(state).state, State::Stale);
        },
        SessionStatus::Idle,
    );
}

/// A terminal opening a conversation this daemon never ran is not recorded.
#[test]
fn an_unknown_conversation_gets_no_lease() {
    run(
        |state, commands| {
            let reply = state.terminal_opened(
                "99999999-2222-4333-8444-555555555555",
                std::process::id(),
                true,
                None,
            );
            assert!(matches!(
                reply,
                crate::rc::control::Reply::TerminalOpened {
                    state: None,
                    notice: None
                }
            ));
            assert!(!stopped(commands));
        },
        SessionStatus::Idle,
    );
}
