use super::*;

const THREAD: &str = "11111111-2222-4333-8444-555555555555";

/// A flush sends one `session.list.changed` for a project whose rows changed since the last one,
/// however many changes reached it, and a flush with nothing new sends nothing. The changes are
/// flushed apart here: a managed session's status twice, then a terminal taking its conversation
/// over, which only moves the write lease, then the writer ending, whose status is projected after
/// its live handle is gone. A daemon that sent one event per change would reload a page several
/// times for one burst; one that missed the lease would leave the row's "running elsewhere" mark
/// stale on a page that no longer polls; one that looked only at live writers would leave an
/// ended conversation listed as running.
#[tokio::test]
async fn each_flush_sends_one_list_change_for_a_project_whose_rows_changed() {
    let (tx, _commands) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
    let mut live = rpc_test_live("agit-h", 1, tx, crate::protocol::PermissionMode::Default);
    live.info.runtime = "claude-code".into();
    live.info.project_id = Some("p".into());
    live.runtime_thread_id = Some(THREAD.into());
    let mut roster = Roster::default();
    let mut entry = rpc_test_roster_entry("agit-h", crate::protocol::PermissionMode::Default);
    entry.project_id = Some("p".into());
    roster.sessions.insert("agit-h".into(), entry);
    let daemon = rpc_test_daemon(HashMap::from([("agit-h".into(), live)]), roster);
    let (out, mut rx) = crate::rc::outbound::channel();
    let mut state = daemon.lock().await;
    state.outbound = Some(out.clone());
    let mut status = Frame::notification(
        method::SESSION_STATUS,
        serde_json::json!({"session_id": "agit-h", "status": "running"}),
    );
    status.stream = Some("agit-h".into());
    state.project_session_frame(status.clone());
    state.project_session_frame(status);
    state.send_list_changes();
    state.send_list_changes();
    state.terminal_opened(THREAD, std::process::id(), true, None);
    state.send_list_changes();
    state.sessions.remove("agit-h");
    let mut ended = Frame::notification(
        method::SESSION_STATUS,
        serde_json::json!({"session_id": "agit-h", "status": "ended"}),
    );
    ended.stream = Some("agit-h".into());
    state.project_session_frame(ended);
    state.send_list_changes();
    out.send(Frame::notification("test.sentinel", serde_json::json!({})));

    let project =
        serde_json::json!({"workspace_id": crate::rc::endpoint::WORKSPACE, "project_id": "p"});
    for change in ["the two status changes", "the takeover", "the ended writer"] {
        let write = rx.next_write().await.unwrap();
        assert_eq!(
            write.frame().method(),
            method::SESSION_LIST_CHANGED,
            "{change}"
        );
        assert_eq!(write.frame().params.as_ref(), Some(&project), "{change}");
        write.commit();
    }
    let write = rx.next_write().await.unwrap();
    assert_eq!(
        write.frame().method(),
        "test.sentinel",
        "one event per flush that had a change, and none from a flush with nothing new"
    );
}

/// A rebuilt catalog's reset reaches a reader allowed one session and not its project, through
/// that session's row, as `session.list` would show it to them. A reset that only marked bound
/// projects changed would reach readers of whole projects and leave this reader's list, read
/// before the rebuild, without the session the rebuild found again.
#[tokio::test]
async fn a_catalog_reset_reaches_a_reader_of_one_session() {
    use agit_peer::access::{Access, Policy, Principal, Resource, Rule};
    let reader = Principal {
        issuer: "https://cloud.example".into(),
        account_id: "reader".into(),
    };
    let policy = Policy::new(
        1,
        vec![Rule {
            principal: reader.clone(),
            resource: Resource::Session("codex:found-again".into()),
            access: Access::Read,
        }],
    )
    .unwrap();
    let daemon = rpc_test_daemon(HashMap::new(), Roster::default());
    let mut state = daemon.lock().await;
    state.catalog_changed(crate::rc::runtime_catalog::CatalogChanges::Reset(vec![
        crate::rc::runtime_catalog::ChangedRow {
            session_ref: "codex:found-again".into(),
            native_session_id: "found-again".into(),
            source: None,
            runtime: "codex".into(),
            cwd: "/w/p".into(),
            present: true,
        },
    ]));
    let frames = state.list_changes.take();
    assert!(
        frames.iter().any(|frame| frame
            .list_audience
            .as_ref()
            .is_some_and(|audience| audience.heard_by(&policy, &reader))),
        "the reader of the one session hears of the reset"
    );
}
