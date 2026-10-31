//! Team activity: report the live sessions' metadata to each Hub this device is enrolled with.
//!
//! A report names each live session's id, executor project, runtime, status and last activity,
//! and nothing else: no title, prompt, path or branch. A session is listed only to the Hub its
//! executor project publishes to, as the project's device-local repository records its
//! confirmed destination. A session without a project, or whose project publishes to no Hub, is
//! listed to none, so one Hub never learns of a session bound for another Hub or of a local run
//! outside every Hub project. Whether that repository belongs to an organization is the Hub's to
//! judge: the device holds only its `owner/name`.
//!
//! A terminal Claude Code session registered for native control runs outside the daemon; where
//! it may be listed is decided from its folder, as [`terminal`] describes.
//!
//! A Hub is sent no session at all until it has answered that the device's owner shares
//! activity with an organization on it, and none again once it answers that the owner does not;
//! an answer that does not say reads as not sharing. What a Hub answered is kept only in this
//! process, so a restarted daemon starts every Hub from an empty report. The Hub sets the
//! cadence through `next_report_seconds`.

#[cfg(unix)]
mod terminal;

use super::Daemon;
use crate::hub::activity::{Place, ReportAnswer, ReportedSession};
use crate::protocol::types::{SessionInfo, SessionStatus};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// How often the reporter checks whether a Hub is due.
const TICK: Duration = Duration::from_secs(15);
/// The wait after a failed report: the Hub may be unreachable, or older than the route.
const RETRY: Duration = Duration::from_secs(600);
/// The longest wait a Hub may ask for, so a changed sharing setting is picked up eventually.
const LONGEST: Duration = Duration::from_secs(3600);
/// Sessions one report may list; the Hub refuses more.
const MAX_SESSIONS: usize = 64;

/// The sessions a report may list, most recently active first: live ones run in an executor
/// project.
fn snapshot<'a>(sessions: impl Iterator<Item = &'a SessionInfo>) -> Vec<ReportedSession> {
    let mut sessions: Vec<_> = sessions
        .filter_map(|info| {
            let status = match info.status {
                SessionStatus::Running => "running",
                SessionStatus::Idle => "idle",
                SessionStatus::AwaitingApproval => "awaiting_approval",
                SessionStatus::Detached | SessionStatus::Ended => return None,
            };
            Some(ReportedSession {
                session_id: info.session_id.clone(),
                place: Place::ProjectId(info.project_id.clone()?),
                runtime: info.runtime.clone(),
                status,
                last_activity_at: info.updated_at.clone(),
            })
        })
        .collect();
    sessions.sort_by(|left, right| right.last_activity_at.cmp(&left.last_activity_at));
    sessions
}

/// Pair each session with the Hub its project publishes to, as `hub_of` answers once per
/// project. A session whose project publishes to no Hub is dropped.
fn attribute(
    sessions: Vec<ReportedSession>,
    mut hub_of: impl FnMut(&str) -> Option<String>,
) -> Vec<(String, ReportedSession)> {
    let mut hubs: HashMap<String, Option<String>> = HashMap::new();
    sessions
        .into_iter()
        .filter_map(|session| {
            let Place::ProjectId(project) = &session.place else {
                return None;
            };
            let hub = hubs
                .entry(project.clone())
                .or_insert_with(|| hub_of(project))
                .clone()?;
            Some((hub, session))
        })
        .collect()
}

/// The Hub `project`'s device-local repository is confirmed to publish to. A project whose
/// destination cannot be read publishes to none.
fn publication_hub(project: &str) -> Option<String> {
    crate::rc::local_repository::publication_hub(project)
        .ok()
        .flatten()
}

/// The sessions of `live` attributed to `hub`, compared in the normalized form a confirmed
/// destination records, leaving out those named by a Hub repository unless `accepts_agent_id`.
/// A Hub address that does not normalize is attributed none.
fn published_to(
    hub: &str,
    live: &[(String, ReportedSession)],
    accepts_agent_id: bool,
) -> Vec<ReportedSession> {
    let Ok(hub) = crate::hub::identity::normalize_hub(hub) else {
        return Vec::new();
    };
    live.iter()
        .filter(|(destination, session)| {
            *destination == hub
                && (accepts_agent_id || matches!(session.place, Place::ProjectId(_)))
        })
        .map(|(_, session)| session.clone())
        .take(MAX_SESSIONS)
        .collect()
}

/// `(hub, device id)` for every Hub the store lists: those this device has cloud access on with,
/// or has asked for it. A device whose cloud access to a Hub is off reports nothing to that Hub.
fn enrolled_devices() -> crate::Result<Vec<(String, String)>> {
    let mut devices = Vec::new();
    for hub in crate::rc::cloud::store::origins()? {
        if let Some(enrollment) = crate::rc::cloud::store::load(&hub)? {
            devices.push((hub, enrollment.credential.device.id));
        }
    }
    Ok(devices)
}

/// What the reporter has learned from one Hub's answers.
struct Known {
    /// The device the answers were about. A new enrollment on the Hub has a new device, whose
    /// owner may be someone else, so it starts over from an empty report.
    device: String,
    due: Instant,
    /// What the Hub's latest answer said: whether the owner shares, and whether it accepts a
    /// session named by `agent_id`. A report that gets no answer leaves both as they were.
    sharing: bool,
    accepts_agent_id: bool,
}

/// The reporter's memory, by Hub.
#[derive(Default)]
struct Hubs(HashMap<String, Known>);

impl Hubs {
    /// Forget the Hubs `devices` no longer lists, and return the enrollments due at `now`.
    fn due(&mut self, devices: Vec<(String, String)>, now: Instant) -> Vec<(String, String)> {
        self.0.retain(|hub, known| {
            devices
                .iter()
                .any(|(enrolled, device)| enrolled == hub && *device == known.device)
        });
        devices
            .into_iter()
            .filter(|(hub, _)| self.0.get(hub).is_none_or(|known| known.due <= now))
            .collect()
    }

    /// The latest answer from `hub` about `device`, if there is one.
    fn known(&self, hub: &str, device: &str) -> Option<&Known> {
        self.0.get(hub).filter(|known| known.device == device)
    }

    /// Whether the latest answer from `hub` about `device` said the owner shares.
    fn shares(&self, hub: &str, device: &str) -> bool {
        self.known(hub, device).is_some_and(|known| known.sharing)
    }

    /// Send one report for each enrollment in `ready`, through `send`. A report lists the
    /// sessions of `live` attributed to its Hub, and only if that Hub's latest answer about that
    /// device said the owner shares; every other report lists no session. A session named by a
    /// Hub repository is listed only if that answer also said the Hub accepts one.
    async fn report<F>(
        &mut self,
        ready: Vec<(String, String)>,
        live: &[(String, ReportedSession)],
        send: F,
    ) where
        F: Fn(&str, &str, &[ReportedSession]) -> crate::Result<ReportAnswer>
            + Clone
            + Send
            + 'static,
    {
        for (hub, device) in ready {
            let (sharing, accepts_agent_id) =
                self.known(&hub, &device).map_or((false, false), |known| {
                    (known.sharing, known.accepts_agent_id)
                });
            let sessions = if sharing {
                published_to(&hub, live, accepts_agent_id)
            } else {
                Vec::new()
            };
            let request = {
                let (send, hub, device) = (send.clone(), hub.clone(), device.clone());
                move || send(hub.as_str(), device.as_str(), sessions.as_slice())
            };
            let answer = tokio::task::spawn_blocking(request)
                .await
                .ok()
                .and_then(Result::ok);
            let (wait, sharing, accepts_agent_id) = match answer {
                Some(answer) => (
                    answer.next_report_seconds,
                    answer.sharing,
                    answer.accepts_agent_id,
                ),
                None => (RETRY.as_secs(), sharing, accepts_agent_id),
            };
            let known = Known {
                device,
                due: Instant::now() + Duration::from_secs(wait).clamp(TICK, LONGEST),
                sharing,
                accepts_agent_id,
            };
            self.0.insert(hub, known);
        }
    }
}

fn send_report(
    hub: &str,
    device: &str,
    sessions: &[ReportedSession],
) -> crate::Result<ReportAnswer> {
    crate::hub::Client::for_stored_hub_with_timeout(hub, Duration::from_secs(10))
        .report_device_activity(device, sessions)
}

/// Every session a report may list, with the Hub it may be listed to, most recently active
/// first: the daemon's own sessions, and on Unix the terminal sessions registered for native
/// control.
async fn live(daemon: &Arc<Mutex<Daemon>>) -> Vec<(String, ReportedSession)> {
    #[cfg(unix)]
    let terminals =
        tokio::task::spawn_blocking(crate::rc::native_claude::Client::terminal_activity)
            .await
            .unwrap_or_default();
    let (sessions, read) = {
        let state = daemon.lock().await;
        let sessions = snapshot(state.sessions.values().map(|session| &session.info));
        #[cfg(unix)]
        let read = {
            let roots = state.mirror.bound_projects();
            let rows: HashMap<String, Vec<terminal::Row>> = terminals
                .iter()
                .map(|t| (t.session.clone(), terminal::rows(&state.roster, &t.session)))
                .collect();
            move || {
                let rows_of = |t: &crate::rc::native_claude::TerminalActivity| {
                    rows.get(&t.session).cloned().unwrap_or_default()
                };
                terminal::attribute(
                    terminals,
                    &roots,
                    |t, project| terminal::read(t, project, rows_of(t)),
                    |t| terminal::folder(t, rows_of(t)),
                )
            }
        };
        #[cfg(not(unix))]
        let read = Vec::new;
        (sessions, read)
    };
    tokio::task::spawn_blocking(move || merged(attribute(sessions, publication_hub), read()))
        .await
        .unwrap_or_default()
}

/// The attributed daemon sessions and terminal sessions in one list, most recently active first,
/// so the per-Hub cap keeps the sessions that moved last whichever kind they are.
fn merged(
    mut sessions: Vec<(String, ReportedSession)>,
    terminals: Vec<(String, ReportedSession)>,
) -> Vec<(String, ReportedSession)> {
    sessions.extend(terminals);
    sessions.sort_by(|(_, left), (_, right)| right.last_activity_at.cmp(&left.last_activity_at));
    sessions
}

pub(super) async fn run(daemon: Arc<Mutex<Daemon>>) {
    let mut hubs = Hubs::default();
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let Ok(Ok(devices)) = tokio::task::spawn_blocking(enrolled_devices).await else {
            continue;
        };
        let ready = hubs.due(devices, Instant::now());
        if ready.is_empty() {
            continue;
        }
        // No project's destination is read in a round where no Hub may be sent a session.
        let live = if ready.iter().any(|(hub, device)| hubs.shares(hub, device)) {
            live(&daemon).await
        } else {
            Vec::new()
        };
        hubs.report(ready, &live, send_report).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::activity::report_body;
    use serde_json::{Value, json};

    type Shared<T> = Arc<std::sync::Mutex<T>>;

    /// Pins what a report body may carry, per Hub. A Hub is sent only the sessions whose project
    /// publishes to it: a session on the other Hub's project, a session without a project and a
    /// session whose project publishes to no Hub reach no body, nor do terminal sessions whose
    /// folder resolves ambiguously or whose project excludes them ([`terminals`]). A terminal
    /// session waiting for approval is listed under the daemon's id with its status fields
    /// alone; the body is compared whole, so any extra key fails it. A terminal session in a
    /// folder bound through the CLI alone is listed by its repository's id, and only to a Hub
    /// whose latest answer accepts that: an answer without `accepts_agent_id` keeps every such
    /// session out, since an older Hub refuses the whole report for it. And it is sent them only while
    /// it says the owner shares: the first report lists none, an answer without `sharing` keeps
    /// it that way, `sharing: true` lets the next report list them, `sharing: false` empties
    /// them again, and one Hub's answer does not open another. An implementation that sends
    /// every live session to every sharing Hub, lets a project-less or unpublished session
    /// through, sends the snapshot first and waits for a refusal, reads a missing field as
    /// sharing, keeps one flag for every Hub, or never forgets an earlier `true` puts a session
    /// into a body asserted to lack it.
    #[test]
    fn a_report_lists_only_its_hubs_sessions_and_only_while_that_hub_says_the_owner_shares() {
        const A: &str = "https://a.example";
        const B: &str = "https://b.example";
        let sent: Shared<Vec<(String, Value)>> = Default::default();
        let answers: Shared<HashMap<String, &'static str>> = Default::default();
        let send = {
            let (sent, answers) = (sent.clone(), answers.clone());
            move |hub: &str, _: &str, listed: &[ReportedSession]| -> crate::Result<ReportAnswer> {
                let body = report_body(listed);
                sent.lock().unwrap().push((hub.to_owned(), body));
                Ok(serde_json::from_str(answers.lock().unwrap()[hub])?)
            }
        };
        let info = |session: &str, project: Option<&str>| -> SessionInfo {
            serde_json::from_value(json!({
                "session_id": session,
                "workspace_id": "local-owner",
                "project_id": project,
                "runtime": "claude",
                "status": "running",
                "last_seq": 0,
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z"
            }))
            .unwrap()
        };
        let sessions = [
            info("on-a", Some("project-a")),
            info("on-b", Some("project-b")),
            info("projectless", None),
            info("unpublished", Some("project-c")),
        ];
        let live = merged(
            attribute(snapshot(sessions.iter()), |project| match project {
                "project-a" => Some(A.to_owned()),
                "project-b" => Some(B.to_owned()),
                _ => None,
            }),
            terminals(A, B),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut hubs = Hubs::default();
        let mut round = |hub: &str, answer: &'static str| {
            answers.lock().unwrap().insert(hub.to_owned(), answer);
            let ready = vec![(hub.to_owned(), "device".to_owned())];
            runtime.block_on(hubs.report(ready, &live, send.clone()));
        };

        round(A, r#"{"next_report_seconds":600}"#);
        round(A, r#"{"next_report_seconds":60,"sharing":true}"#);
        round(
            B,
            r#"{"next_report_seconds":60,"sharing":true,"accepts_agent_id":true}"#,
        );
        round(
            A,
            r#"{"next_report_seconds":60,"sharing":true,"accepts_agent_id":true}"#,
        );
        round(B, r#"{"next_report_seconds":600,"sharing":false}"#);
        round(A, r#"{"next_report_seconds":600,"sharing":false}"#);
        round(A, r#"{"next_report_seconds":600,"sharing":false}"#);

        let empty = json!({"sessions": []});
        let listed = |session: &str, project: &str| {
            json!({"sessions": [{
                "session_id": session,
                "project_id": project,
                "runtime": "claude",
                "status": "running",
                "last_activity_at": "2026-01-01T00:00:00Z"
            }]})
        };
        let expected = vec![
            // A Hub that has not answered yet.
            (A.to_owned(), empty.clone()),
            // An answer without `sharing`.
            (A.to_owned(), empty.clone()),
            // Another Hub, while the first one shares.
            (B.to_owned(), empty.clone()),
            // After `sharing: true` without `accepts_agent_id`, the Hub's own sessions named by
            // a project and nothing else: none named by a repository.
            (
                A.to_owned(),
                with_terminals(listed("on-a", "project-a"), vec![approval()]),
            ),
            // After an answer that also accepts them, the Hub's own sessions named by a
            // repository too.
            (
                B.to_owned(),
                with_terminals(
                    listed("on-b", "project-b"),
                    vec![in_folder("folder-b", "agent-d")],
                ),
            ),
            (
                A.to_owned(),
                with_terminals(
                    listed("on-a", "project-a"),
                    vec![in_folder("folder-a", "agent-c"), approval()],
                ),
            ),
            // After `sharing: false`.
            (A.to_owned(), empty),
        ];
        assert_eq!(*sent.lock().unwrap(), expected);
        let sent = serde_json::to_string(&*sent.lock().unwrap()).unwrap();
        assert!(
            !sent.contains("claude-session"),
            "a terminal session is never named by Claude Code's own id"
        );
        assert!(
            !["tool", "command", "/work", "acme/"]
                .iter()
                .any(|key| sent.contains(key)),
            "no tool, command, folder or repository name reaches a body"
        );
    }

    /// The terminal sessions of the report test, attributed. In Workspace projects: one that
    /// waits for approval in a project publishing to `a`, which is listed, and three that would
    /// be listed beside it were their binding read loosely: one in a folder bound inside another
    /// bound folder, one whose own spelling of its folder lies in another project, and one its
    /// project excludes. In folders no Workspace project binds: one whose CLI binding uploads it
    /// to a repository on each Hub, which are listed by that repository's id, and four that
    /// are not: one whose binding is not certain to upload it, one a remote-control row places
    /// in a project, one whose spelling of its folder lies in a Workspace project, and one whose
    /// binding uploads as another account than the one that owns the device's enrollment.
    #[cfg(unix)]
    fn terminals(a: &str, b: &str) -> Vec<(String, ReportedSession)> {
        use crate::commands::project::Capture;
        use crate::hub::identity::RemoteIdentity;
        use std::path::PathBuf;
        let roots: Vec<(String, PathBuf)> = [
            ("project-a", "/work/a"),
            ("project-nested", "/work/a/nested"),
            ("project-b", "/work/b"),
        ]
        .into_iter()
        .map(|(project, root)| (project.to_owned(), PathBuf::from(root)))
        .collect();
        let terminal = |id: &str, cwd: &str, spelled: &str, status, at: &str| {
            crate::rc::native_claude::TerminalActivity {
                report_id: id.to_owned(),
                session: format!("claude-session-{id}"),
                cwd: cwd.into(),
                spelled: spelled.into(),
                status,
                last_activity_at: at.to_owned(),
            }
        };
        let (moved, later) = ("2026-01-02T00:00:00Z", "2026-01-03T00:00:00Z");
        let terminals = vec![
            terminal(
                "approval",
                "/work/a/src",
                "/work/a/src",
                "awaiting_approval",
                moved,
            ),
            terminal(
                "nested",
                "/work/a/nested",
                "/work/a/nested",
                "running",
                moved,
            ),
            terminal("symlinked", "/work/a/src", "/work/b/link", "running", moved),
            terminal("excluded", "/work/a/src", "/work/a/src", "running", moved),
            terminal("folder-a", "/work/c", "/work/c", "running", later),
            terminal("folder-b", "/work/d", "/work/d", "running", later),
            terminal("folder-uncertain", "/work/c", "/work/c", "running", later),
            terminal("folder-claimed", "/work/c", "/work/c", "running", later),
            terminal("folder-other-owner", "/work/c", "/work/c", "running", later),
            terminal(
                "folder-spelled",
                "/work/c",
                "/work/a/link",
                "running",
                later,
            ),
        ];
        let identity = |hub: &str, agent: &str| RemoteIdentity {
            hub: hub.to_owned(),
            agent_id: agent.to_owned(),
        };
        terminal::attribute(
            terminals,
            &roots,
            |terminal, _| terminal::Publication {
                destination: Some(terminal::Destination {
                    identity: identity(a, "agent-a"),
                    repositories: ["acme/a".to_owned(), "local/a".to_owned()],
                }),
                capture: if terminal.report_id == "excluded" {
                    Capture::Withheld
                } else {
                    Capture::Uploads(identity(a, "agent-a"))
                },
                rows: Vec::new(),
                claim: None,
            },
            |terminal| terminal::Folder {
                upload: match terminal.report_id.as_str() {
                    "folder-uncertain" => None,
                    "folder-b" => Some(identity(b, "agent-d")),
                    _ => Some(identity(a, "agent-c")),
                },
                account: Some("owner".to_owned()),
                owner: Some(
                    if terminal.report_id == "folder-other-owner" {
                        "someone-else"
                    } else {
                        "owner"
                    }
                    .to_owned(),
                ),
                rows: if terminal.report_id == "folder-claimed" {
                    vec![terminal::Row {
                        project: Some("project-a".to_owned()),
                        observed: true,
                    }]
                } else {
                    Vec::new()
                },
            },
        )
    }

    #[cfg(not(unix))]
    fn terminals(_: &str, _: &str) -> Vec<(String, ReportedSession)> {
        Vec::new()
    }

    /// `body` with the terminal sessions `listed` before its daemon session, as they moved
    /// later. Without native control, on Windows, there are none.
    fn with_terminals(mut body: Value, listed: Vec<Value>) -> Value {
        if cfg!(unix) {
            let sessions = body["sessions"].as_array_mut().unwrap();
            for (at, session) in listed.into_iter().enumerate() {
                sessions.insert(at, session);
            }
        }
        body
    }

    /// The terminal session waiting for approval, as a body lists it: its status fields and
    /// nothing more, so no tool, command or path behind the approval reaches it.
    fn approval() -> Value {
        json!({
            "session_id": "approval",
            "project_id": "project-a",
            "runtime": "claude-code",
            "status": "awaiting_approval",
            "last_activity_at": "2026-01-02T00:00:00Z"
        })
    }

    /// A terminal session in a folder bound through the CLI alone, as a body lists it: named by
    /// its repository's opaque id, with no project id, and nothing more.
    fn in_folder(session: &str, agent: &str) -> Value {
        json!({
            "session_id": session,
            "agent_id": agent,
            "runtime": "claude-code",
            "status": "running",
            "last_activity_at": "2026-01-03T00:00:00Z"
        })
    }

    /// Pins that a session's last activity follows its events into the report snapshot: after a
    /// status event, the session that started earliest is listed first, with that status and a
    /// time later than the other session's. An implementation that projects only the status,
    /// or reports the time the session was opened, lists it last with its opening time, and
    /// the per-Hub cap then drops the session that just moved.
    #[test]
    fn a_status_event_moves_the_sessions_last_activity_in_the_report() {
        use crate::protocol::{Frame, PermissionMode, method};
        let home = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(home.path(), || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let live = |session: &str, at: &str| {
                        let (tx, _) = tokio::sync::mpsc::channel(1);
                        let mut live = super::super::tests::rpc_test_live(
                            session,
                            1,
                            tx,
                            PermissionMode::Default,
                        );
                        live.info.project_id = Some("project".into());
                        live.info.updated_at = at.into();
                        (session.to_owned(), live)
                    };
                    let daemon = super::super::tests::rpc_test_daemon(
                        [
                            live("opened-first", "2026-01-01T00:00:00Z"),
                            live("opened-later", "2026-01-02T00:00:00Z"),
                        ]
                        .into_iter()
                        .collect(),
                        Default::default(),
                    );
                    let mut state = daemon.lock().await;
                    let mut frame = Frame::notification(
                        method::SESSION_STATUS,
                        json!({"session_id": "opened-first", "status": "awaiting_approval"}),
                    );
                    frame.stream = Some("opened-first".into());
                    state.project_session_frame(frame);

                    let listed = snapshot(state.sessions.values().map(|live| &live.info));
                    let order: Vec<_> = listed.iter().map(|s| s.session_id.as_str()).collect();
                    assert_eq!(order, ["opened-first", "opened-later"]);
                    assert_eq!(listed[0].status, "awaiting_approval");
                    assert!(listed[0].last_activity_at > listed[1].last_activity_at);
                });
        });
    }
}
