//! A turn running on a runtime the daemon started is recorded when it starts and removed when it
//! ends. A daemon that dies during it leaves the record behind, and the next start closes the turn:
//! it ends the runtime it left running, when that process is provably the one it started, closes
//! every tool call the transcript left without a result, and archives what was recorded through
//! the usual job. Without it the conversation ends in a call whose result never comes, the runtime
//! cannot continue it, and nothing of the turn is ever archived.

use super::*;
use crate::rc::roster::OpenTurn;
use anyhow::{Context, ensure};
use std::io::{Read, Seek, Write};
use std::path::Path;

/// The result written for a tool call the transcript left open. It names no person and no
/// computer: the resumed runtime reads it, and the archive keeps it.
pub(crate) const INTERRUPTED_OUTPUT: &str = "[agit] interrupted: agit stopped on this computer while this tool was running; its output is not recorded.";

/// How long the runtime a dead daemon left behind gets to exit after each signal.
const ORPHAN_EXIT_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// What became of one recorded turn.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Recovery {
    /// The turn is closed where the transcript ends, its open calls given results; `cut` says
    /// whether the daemon's death cut it (the conversation awaits a reply there), and `archived`
    /// whether a job carries it.
    Closed { cut: bool, archived: bool },
    /// The transcript is no longer this turn's to touch.
    Left,
    /// Not done yet: whether the runtime still writes cannot be told, or a step failed in a way
    /// a later try may not. The record stays and the next start tries again; every step is safe
    /// to repeat.
    Retry,
}

impl Daemon {
    /// Record or clear the open turn a session frame starts or ends.
    pub(super) fn observe_open_turn(&mut self, stream: &str, frame: &Frame) {
        let changed = match frame.method() {
            method::TURN_STARTED => {
                let Some(live) = self.sessions.get(stream) else {
                    return;
                };
                let Some(entry) = self.roster.get(stream) else {
                    return;
                };
                // A runtime the daemon did not start, or shares, outlives the daemon and ends its
                // own turns. Only Claude Code's transcript is closed on the next start.
                if !live.started_process
                    || live.shared_executor
                    || live.info.runtime != "claude-code"
                {
                    return;
                }
                let Some(turn_id) = frame
                    .params
                    .as_ref()
                    .and_then(|params| params["turn_id"].as_str())
                else {
                    return;
                };
                let turn = OpenTurn {
                    turn_id: turn_id.into(),
                    runtime: live.info.runtime.clone(),
                    thread_id: live
                        .runtime_thread_id
                        .clone()
                        .unwrap_or_else(|| entry.thread_id.clone()),
                    cwd: entry.cwd.clone(),
                    writer: live
                        .writer_process
                        .as_ref()
                        .map(|writer| (writer.pid, writer.started_ms)),
                    stopped_by_person: false,
                };
                self.roster.open_turns.insert(stream.into(), turn);
                true
            }
            method::TURN_COMPLETED => self.roster.open_turns.remove(stream).is_some(),
            _ => false,
        };
        if changed && let Err(error) = self.roster.save() {
            eprintln!("agitd: the open turn could not be recorded: {error:#}");
        }
    }

    /// Close every turn a daemon that died during it left recorded. Runs before anything can
    /// resume those conversations.
    pub(super) async fn recover_open_turns(&mut self) {
        if self.roster.open_turns.is_empty() {
            return;
        }
        // A record whose conversation the roster no longer has is dropped with nothing to close.
        let orphaned: Vec<String> = self
            .roster
            .open_turns
            .keys()
            .filter(|logical| self.roster.get(logical).is_none())
            .cloned()
            .collect();
        for logical in orphaned {
            self.roster.open_turns.remove(&logical);
        }
        let work: Vec<_> = self
            .roster
            .open_turns
            .iter()
            .filter_map(|(logical, turn)| {
                let entry = self.roster.get(logical)?.clone();
                // A conversation that settles nowhere is still closed; it only has no archive.
                let lineage = entry
                    .agit_session
                    .as_deref()
                    .zip(entry.expected_agent_id.as_deref())
                    .and_then(|(lineage, id)| {
                        crate::rc::lineage::AgitSession::parse(lineage, id).ok()
                    })
                    .map(|mut lineage| {
                        lineage.capture = self.roster.captures.get(logical).cloned();
                        lineage
                    });
                Some((logical.clone(), entry, turn.clone(), lineage))
            })
            .collect();
        let outcomes = tokio::task::spawn_blocking(move || {
            work.into_iter()
                .map(|(logical, entry, turn, lineage)| {
                    let outcome = recover(&logical, &entry, &turn, lineage.as_ref())
                        .unwrap_or_else(|error| {
                            // A failure to read or write may pass; anything else is about this
                            // transcript and stays.
                            let retry = error
                                .chain()
                                .any(|cause| cause.downcast_ref::<std::io::Error>().is_some());
                            crate::rc::diagnostics::event(
                                "open_turn.unrecovered",
                                serde_json::json!({
                                    "session_id": crate::rc::diagnostics::id(&logical),
                                    "error": format!("{error:#}"),
                                    "retry": retry,
                                }),
                            );
                            if retry {
                                Recovery::Retry
                            } else {
                                Recovery::Left
                            }
                        });
                    (logical, turn, outcome)
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        let at = chrono::Utc::now().to_rfc3339();
        for (logical, turn, outcome) in outcomes {
            let (closed, archived) = match outcome {
                Recovery::Closed { archived, .. } => (true, archived),
                Recovery::Left | Recovery::Retry => (false, false),
            };
            crate::rc::diagnostics::event(
                "open_turn.recovered",
                serde_json::json!({
                    "session_id": crate::rc::diagnostics::id(&logical),
                    "closed": closed,
                    "archived": archived,
                    "retry": outcome == Recovery::Retry,
                }),
            );
            match outcome {
                Recovery::Retry => continue,
                Recovery::Closed { cut: false, .. } | Recovery::Left => {}
                Recovery::Closed { cut: true, .. } => {
                    // A turn a stop already recorded keeps that cause.
                    self.roster.interrupted.entry(logical.clone()).or_insert(
                        crate::protocol::Interruption {
                            at: at.clone(),
                            cause: if turn.stopped_by_person {
                                crate::protocol::InterruptionCause::Stopped
                            } else {
                                crate::protocol::InterruptionCause::Crashed
                            },
                        },
                    );
                }
            }
            self.roster.open_turns.remove(&logical);
        }
        if let Err(error) = self.roster.save() {
            eprintln!("agitd: recovered turns could not be recorded: {error:#}");
        }
    }
}

/// Close one recorded turn. Kill and close are separate questions: the runtime is signalled only
/// when its pid still names the process the daemon started, and the transcript is written only
/// once that process is gone and nothing else holds it.
fn recover(
    logical: &str,
    entry: &roster::Entry,
    turn: &OpenTurn,
    lineage: Option<&crate::rc::lineage::AgitSession>,
) -> crate::Result<Recovery> {
    if turn.runtime != "claude-code" {
        // Only Claude Code's transcript is closed here; the others keep today's resume path.
        return Ok(Recovery::Left);
    }
    if let Some((pid, started)) = turn.writer {
        match end_orphan(pid, started) {
            Orphan::Gone => {}
            Orphan::Unknown => return Ok(Recovery::Retry),
        }
    }
    // A person's own terminal may have resumed the conversation since: it is theirs now.
    if super::native_writer_active(&turn.runtime, &turn.thread_id) == Some(true) {
        return Ok(Recovery::Left);
    }
    let cwd = Path::new(&turn.cwd);
    let Some(path) = crate::adapter::claude_code::locate_transcript(&turn.thread_id, Some(cwd))
    else {
        return Ok(Recovery::Left);
    };
    // The turn is closed whether or not a call is left open: a daemon that died before the first
    // call, or after the last result, leaves a turn that awaits a reply nobody will write.
    let Some(Closed { end, cut }) = close_calls(&path, &turn.thread_id, cwd)? else {
        return Ok(Recovery::Left);
    };
    crate::rc::writer_release::record(&turn.runtime, &turn.thread_id, &path)?;
    let archived = match lineage {
        // A turn a person's stop cut is held like their interrupt.
        Some(lineage)
            if !turn.stopped_by_person
                && crate::rc::capture::consented(
                    cwd,
                    &turn.runtime,
                    &turn.thread_id,
                    entry.project_id.is_some(),
                ) =>
        {
            use crate::rc::native_claude::completion::{TurnEnd, turn_end};
            match turn_end(
                &turn.thread_id,
                &path,
                end,
                Some(("recovered", &turn.turn_id)),
                false,
            )? {
                TurnEnd::Closed(at) | TurnEnd::Open(at) => {
                    crate::rc::archive_jobs::Job::capture(
                        logical,
                        &turn.thread_id,
                        &turn.runtime,
                        entry.native_source.clone(),
                        cwd,
                        lineage,
                        &turn.turn_id,
                        &path,
                        at,
                        None,
                    )?
                    .record()?;
                    true
                }
                TurnEnd::Held => false,
            }
        }
        _ => false,
    };
    Ok(Recovery::Closed { cut, archived })
}

enum Orphan {
    /// The process the daemon started is no longer running.
    Gone,
    /// A process with that pid runs, and whether it is that one cannot be told, or it would not
    /// exit.
    Unknown,
}

/// End the runtime a dead daemon left running, if `pid` still names it.
fn end_orphan(pid: u32, started: i64) -> Orphan {
    let ours = || crate::rc::native_claude::started_ms(pid) == Some(started);
    match crate::rc::native_claude::started_ms(pid) {
        Some(now) if now == started => {}
        // Another process has the pid now; the one the daemon started is gone.
        Some(_) => return Orphan::Gone,
        None if process_exists(pid) => return Orphan::Unknown,
        None => return Orphan::Gone,
    }
    for signal in [libc::SIGTERM, libc::SIGKILL] {
        // The runtime leads its own process group, so the group carries its tool processes too.
        unsafe {
            libc::killpg(pid as i32, signal);
        }
        let deadline = std::time::Instant::now() + ORPHAN_EXIT_WAIT;
        while std::time::Instant::now() < deadline {
            if !ours() {
                return Orphan::Gone;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    if ours() {
        Orphan::Unknown
    } else {
        Orphan::Gone
    }
}

fn process_exists(pid: u32) -> bool {
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Where a closed transcript ends, and whether the conversation awaits a reply there.
pub(super) struct Closed {
    pub end: u64,
    pub cut: bool,
}

/// Close the tool calls the transcript at `path` left open, and return where it now ends; None
/// when it is not this conversation's transcript. Only for a transcript whose writer is proven
/// gone. Running it again on a transcript it closed writes nothing.
///
/// A record the dead writer was still writing is cut away first: it was never finished, so it is
/// neither scanned nor archived, and nothing written later joins onto it.
pub(super) fn close_calls(
    path: &Path,
    thread_id: &str,
    cwd: &Path,
) -> crate::Result<Option<Closed>> {
    let mut file = {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true);
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
        options.open(path)?
    };
    let size = file.metadata()?.len();
    ensure!(
        file.metadata()?.is_file(),
        "native transcript is not a regular file"
    );
    ensure!(
        size <= crate::adapter::native_snapshot::MAX_CAPTURE_BYTES as u64,
        "native transcript exceeds its capture budget"
    );
    let mut bytes = Vec::with_capacity(size as usize);
    (&mut file).take(size).read_to_end(&mut bytes)?;
    let complete = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |end| end + 1);
    let text = std::str::from_utf8(&bytes[..complete]).context("native transcript is not UTF-8")?;
    if !belongs_to(text, thread_id, cwd) {
        return Ok(None);
    }
    let closing = crate::domain::install::closing_records(
        text,
        "claude-code",
        thread_id,
        cwd,
        INTERRUPTED_OUTPUT,
    )?;
    let cut = crate::adapter::claude_code::awaits_reply(&format!("{text}{closing}"));
    if closing.is_empty() && complete as u64 == size {
        return Ok(Some(Closed { end: size, cut }));
    }
    // Written only while the file is as read: anything appended since belongs to a writer.
    ensure!(
        file.metadata()?.len() == size,
        "native transcript changed while it was being closed"
    );
    file.set_len(complete as u64)?;
    file.seek(std::io::SeekFrom::End(0))?;
    file.write_all(closing.as_bytes())?;
    file.sync_all()?;
    Ok(Some(Closed {
        end: (complete + closing.len()) as u64,
        cut,
    }))
}

/// Whether every record names this conversation, and the conversation began in this folder. A
/// path found from a stale record may by now hold another conversation's file, or one moved or
/// linked there. Only the first folder counts: Claude Code records the folder the agent is in, so
/// a conversation that changed directory names several.
fn belongs_to(text: &str, thread_id: &str, cwd: &Path) -> bool {
    let mut named = false;
    let mut began = None;
    for record in text
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
        if let Some(session) = record["sessionId"].as_str() {
            if session != thread_id {
                return false;
            }
            named = true;
        }
        if began.is_none()
            && let Some(recorded) = record["cwd"].as_str()
        {
            began = Some(Path::new(recorded).canonicalize().ok());
        }
    }
    named && began.is_some_and(|began| began.is_some() && began == cwd.canonicalize().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A start after a daemon died closes the turns it left open, and only as far as is safe.
    /// The orphaned runtime whose pid and start time still match is ended; a pid that now names
    /// another process is left alone, and its turn is closed because that writer is gone. A record
    /// the dead writer left half-written is cut, not scanned or joined onto. A marker whose path
    /// now holds another conversation's transcript writes nothing, while one whose agent moved to
    /// another folder mid-turn is still this conversation's. A turn cut before its first call, or
    /// after its last result, has nothing to close and is still ended as cut. Each closed
    /// transcript has no open call left, so Claude Code can continue it, and only `{at, cause}`
    /// reaches a subscriber. A step that fails to write after the transcript is closed keeps every
    /// record for the next start, which finishes the turns without closing them twice. A recovery
    /// that trusted the pid would end someone else's process; one that trusted the path would
    /// write into another conversation; one that kept the half line would archive a cut-off
    /// record; one that gave up on a turn with no open call, or on a failed write, would leave a
    /// turn nobody ever ends or archives.
    #[test]
    fn a_start_closes_the_turns_a_dead_daemon_left_open() {
        if crate::rc::in_isolated_test(
            "rc::daemon::open_turns::tests::a_start_closes_the_turns_a_dead_daemon_left_open",
        ) {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let claude = home.path().join("claude");
        // Process-wide, as this test runs alone in its process: recovery works on another thread,
        // which a thread's own home override doesn't reach.
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", &claude);
            std::env::set_var("AGIT_HOME", home.path().join("agit"));
        }
        let line = |value: serde_json::Value| format!("{value}\n");
        // The call is made from a folder the agent moved to, as Claude Code records it.
        let transcript = |thread: &str, cwd: &Path, tail: &str| {
            let path = crate::adapter::claude_code::session_path(thread, cwd).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let moved_to = cwd.join("src");
            let prompt = line(serde_json::json!({"type": "user", "sessionId": thread,
                "cwd": cwd, "uuid": "u1", "message": {"role": "user", "content": "go"}}));
            let call = line(serde_json::json!({"type": "assistant", "sessionId": thread,
                "cwd": moved_to, "uuid": "a1", "parentUuid": "u1", "message": {
                    "model": "claude-haiku-5-5", "role": "assistant", "stop_reason": "tool_use",
                    "content": [{"type": "tool_use", "id": "toolu_1", "name": "Bash",
                                 "input": {"command": "sleep 60"}}]}}));
            let result = line(serde_json::json!({"type": "user", "sessionId": thread,
                "cwd": moved_to, "uuid": "r1", "parentUuid": "a1", "message": {"role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "toolu_1",
                                 "content": "done"}]}}));
            let text = match tail {
                "before-call" => prompt,
                "after-result" => prompt + &call + &result,
                tail => prompt + &call + tail,
            };
            std::fs::write(&path, text).unwrap();
            path
        };
        let spawn = || {
            use std::os::unix::process::CommandExt;
            std::process::Command::new("sleep")
                .arg("60")
                .process_group(0)
                .spawn()
                .unwrap()
        };
        let mut reused = spawn();
        let mut orphan = spawn();
        let started = |pid: u32| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Some(ms) = crate::rc::native_claude::started_ms(pid) {
                    break ms;
                }
                assert!(std::time::Instant::now() < deadline, "start time of {pid}");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        let cases = [
            (
                "agit-reused",
                "thread-reused",
                Some((reused.id(), started(reused.id()) + 1)),
            ),
            (
                "agit-orphan",
                "thread-orphan",
                Some((orphan.id(), started(orphan.id()))),
            ),
            ("agit-moved", "thread-moved", None),
            ("agit-before-call", "thread-before-call", None),
            ("agit-after-result", "thread-after-result", None),
        ];
        let mut roster = Roster::default();
        let mut paths = std::collections::BTreeMap::new();
        for (logical, thread, writer) in cases {
            let cwd = home.path().join(logical);
            std::fs::create_dir_all(&cwd).unwrap();
            let cwd = cwd.canonicalize().unwrap();
            // The moved conversation's file now holds another conversation's records.
            let named = if logical == "agit-moved" {
                "someone-else"
            } else {
                thread
            };
            let half = match logical {
                "agit-orphan" => "{\"type\":\"assist",
                "agit-before-call" => "before-call",
                "agit-after-result" => "after-result",
                _ => "",
            };
            let path = transcript(named, &cwd, half);
            let path = if logical == "agit-moved" {
                let moved = crate::adapter::claude_code::session_path(thread, &cwd).unwrap();
                std::fs::rename(&path, &moved).unwrap();
                moved
            } else {
                path
            };
            paths.insert(logical, (path.clone(), std::fs::read(&path).unwrap()));
            let mut entry = super::super::tests::rpc_test_roster_entry(
                logical,
                crate::protocol::PermissionMode::Default,
            );
            entry.runtime = "claude-code".into();
            entry.thread_id = thread.into();
            entry.cwd = cwd.to_string_lossy().into_owned();
            roster.sessions.insert(logical.into(), entry);
            roster.open_turns.insert(
                logical.into(),
                OpenTurn {
                    turn_id: uuid::Uuid::new_v4().to_string(),
                    runtime: "claude-code".into(),
                    thread_id: thread.into(),
                    cwd: cwd.to_string_lossy().into_owned(),
                    writer,
                    stopped_by_person: false,
                },
            );
        }
        // The process a dead daemon left behind belongs to init, which reaps it as it exits; here
        // the test is its parent and stands in for init.
        let reaper = std::thread::spawn(move || orphan.wait());
        crate::rc::with_agit_home(&home.path().join("agit"), || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let daemon = super::super::tests::rpc_test_daemon(HashMap::new(), roster);
                    let mut state = daemon.lock().await;
                    // The release mark can't be written: the transcripts are closed, and every
                    // turn whose transcript is its own stays recorded for the next start.
                    let released = crate::rc::rc_dir().unwrap().join("released");
                    std::fs::write(&released, b"").unwrap();
                    state.recover_open_turns().await;
                    let kept: Vec<_> = Roster::load().open_turns.keys().cloned().collect();
                    assert_eq!(
                        kept,
                        [
                            "agit-after-result",
                            "agit-before-call",
                            "agit-orphan",
                            "agit-reused"
                        ]
                    );
                    let first: Vec<_> = ["agit-orphan", "agit-reused"]
                        .map(|logical| std::fs::read(&paths[logical].0).unwrap())
                        .into();
                    std::fs::remove_file(&released).unwrap();
                    state.recover_open_turns().await;
                    assert_eq!(
                        ["agit-orphan", "agit-reused"]
                            .map(|logical| std::fs::read(&paths[logical].0).unwrap())
                            .to_vec(),
                        first,
                        "a closed transcript is not closed again"
                    );

                    assert!(
                        reused.try_wait().unwrap().is_none(),
                        "a reused pid is left alone"
                    );
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    while !reaper.is_finished() {
                        assert!(std::time::Instant::now() < deadline, "the orphan is ended");
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    let saved = Roster::load();
                    assert!(saved.open_turns.is_empty());
                    let interrupted: Vec<_> = saved.interrupted.keys().cloned().collect();
                    assert_eq!(
                        interrupted,
                        [
                            "agit-after-result",
                            "agit-before-call",
                            "agit-orphan",
                            "agit-reused"
                        ]
                    );
                    for logical in ["agit-after-result", "agit-before-call"] {
                        let (path, before) = &paths[logical];
                        assert_eq!(&std::fs::read(path).unwrap(), before, "nothing to close");
                        assert_eq!(
                            saved.interrupted[logical].cause,
                            crate::protocol::InterruptionCause::Crashed
                        );
                    }
                    for logical in ["agit-orphan", "agit-reused"] {
                        assert_eq!(
                            saved.interrupted[logical].cause,
                            crate::protocol::InterruptionCause::Crashed
                        );
                        let (path, _) = &paths[logical];
                        let text = std::fs::read_to_string(path).unwrap();
                        assert!(text.ends_with('\n'), "{logical}");
                        // Whole records are written with their keys in order, so only the cut one starts so.
                        assert!(!text.contains("{\"type\":\"assist"), "{logical}");
                        assert!(
                            crate::adapter::get("claude-code")
                                .unwrap()
                                .open_tool_calls(&text)
                                .is_empty(),
                            "{logical}: {text}"
                        );
                        assert!(text.contains(INTERRUPTED_OUTPUT), "{logical}");
                    }
                    let (moved, before) = &paths["agit-moved"];
                    assert_eq!(&std::fs::read(moved).unwrap(), before, "nothing is written");

                    let (frames, _) = mpsc::channel(1);
                    let mut subscribe = Frame::request(
                        method::SESSION_SUBSCRIBE,
                        serde_json::json!({"session_id": "agit-orphan"}),
                    );
                    subscribe.caller = Some(super::super::tests::claim("viewer", "ws-a"));
                    let data = state
                        .dispatch(&subscribe, &frames)
                        .await
                        .unwrap_err()
                        .data
                        .unwrap();
                    let interrupted = data["interrupted"].as_object().unwrap();
                    assert_eq!(
                        interrupted.keys().map(String::as_str).collect::<Vec<_>>(),
                        ["at", "cause"]
                    );
                    assert_eq!(interrupted["cause"], "crashed");
                })
        });
        let _ = reused.kill();
        let _ = reused.wait();
    }
}
