//! The daemon's side of the one-writer lease: a terminal that loads a conversation this daemon
//! runs headless takes it over at the headless turn boundary, and viewers see who holds it.

use super::*;
use crate::rc::lease::{Headless, Leases, Loaded, Opened, Terminal, descends};

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn lease_path() -> Option<std::path::PathBuf> {
    crate::rc::lease::path()
}

/// A recorded terminal is still that process: same pid, same start.
fn terminal_alive(terminal: &Terminal) -> bool {
    #[cfg(unix)]
    {
        crate::rc::native_claude::started_ms(terminal.pid) == Some(terminal.started_ms)
    }
    #[cfg(not(unix))]
    {
        let _ = terminal;
        false
    }
}

/// This device's short code, from its Hub enrollment. A device enrolled with several Hubs has a
/// different id at each, and a banner cannot tell which Hub shows it, so it names none.
fn device_code() -> Option<String> {
    let mut enrolled = crate::rc::cloud::store::origins()
        .ok()?
        .into_iter()
        .filter_map(|hub| crate::rc::cloud::store::load(&hub).ok().flatten());
    let enrollment = enrolled.next()?;
    if enrolled.next().is_some() {
        return None;
    }
    crate::rc::lease::device_code(&enrollment.credential.device.id)
}

impl Daemon {
    pub(super) fn load_leases() -> Leases {
        lease_path()
            .map(|path| Leases::load(&path, terminal_alive, now_ms()))
            .unwrap_or_default()
    }

    fn save_leases(&self) {
        if let Some(path) = lease_path()
            && let Err(error) = self.leases.save(&path)
        {
            eprintln!("agitd: could not save conversation leases: {error:#}");
        }
    }

    /// The live headless Claude Code supervisor writing `thread`, by logical session id.
    fn headless_writer(&self, thread: &str) -> Option<String> {
        self.sessions
            .iter()
            .find(|(_, live)| {
                live.info.runtime == "claude-code"
                    && live.info.native_source.is_none()
                    && !live.shared_executor
                    && !live.ended
                    && !live.writer_released
                    && live.runtime_thread_id.as_deref() == Some(thread)
            })
            .map(|(session_id, _)| session_id.clone())
    }

    /// The Claude Code conversation a session stream writes, when this daemon runs it headless.
    fn headless_thread(&self, session_id: &str) -> Option<String> {
        self.sessions
            .get(session_id)
            .filter(|live| {
                live.info.runtime == "claude-code"
                    && live.info.native_source.is_none()
                    && !live.shared_executor
            })
            .and_then(|live| live.runtime_thread_id.clone())
    }

    fn known_conversation(&self, thread: &str) -> bool {
        self.leases.get(thread).is_some()
            || self
                .roster
                .sessions
                .values()
                .any(|entry| entry.runtime == "claude-code" && entry.thread_id == thread)
    }

    /// A terminal loaded `thread`, continuing the branch `loaded` when its plugin has named it.
    /// Conversations this daemon never ran are none of its business.
    pub(super) fn terminal_opened(
        &mut self,
        thread: &str,
        pid: u32,
        plugin: bool,
        loaded: Option<Loaded>,
    ) -> control::Reply {
        #[cfg(unix)]
        let started = crate::rc::native_claude::started_ms(pid);
        #[cfg(not(unix))]
        let started: Option<i64> = None;
        let headless = self.headless_writer(thread);
        let (Some(started_ms), true) = (
            started,
            headless.is_some() || self.known_conversation(thread),
        ) else {
            return control::Reply::TerminalOpened {
                state: None,
                notice: None,
            };
        };
        let terminal = Terminal {
            pid,
            started_ms,
            plugin,
        };
        // A writer between turns stops at once and has written everything this terminal loaded,
        // so nothing runs that the terminal has to wait for or resume again after.
        let idle_writer = headless
            .as_deref()
            .and_then(|session_id| self.sessions.get(session_id))
            .is_some_and(|live| live.info.status == SessionStatus::Idle);
        let opened = self.leases.opened(
            thread,
            terminal,
            loaded,
            headless
                .as_deref()
                .map(|session_id| Headless { session_id }),
            &descends,
            now_ms(),
        );
        let quiet = idle_writer && matches!(opened, Opened::Release { .. });
        if let Opened::Release { session_id } = &opened {
            if let Some(writer) = self
                .sessions
                .get(session_id)
                .and_then(|live| live.writer_process)
            {
                self.leases.release_writer(thread, writer);
            }
            self.stop_if_idle(session_id);
        }
        self.save_leases();
        let lease = self.leases.get(thread);
        control::Reply::TerminalOpened {
            state: lease.map(|lease| lease.state),
            notice: lease
                .filter(|_| !quiet)
                .and_then(|lease| lease.notice_for(thread, &terminal)),
        }
    }

    /// The headless writer of `thread` reports the last record of the turn it finished. Only a
    /// writer this daemon runs for that conversation is listened to.
    pub(super) fn writer_leaf(&mut self, thread: &str, leaf: &str) {
        if self.headless_writer(thread).is_some() {
            self.leases.writer_reported(thread, leaf, now_ms());
            self.save_leases();
        }
    }

    /// Stop a releasing headless writer now if it is between turns; otherwise its turn boundary
    /// does (see `lease_turn_boundary`).
    fn stop_if_idle(&mut self, session_id: &str) {
        if let Some(live) = self.sessions.get(session_id)
            && live.info.status == SessionStatus::Idle
        {
            let _ = live.tx.try_send(Command::Shutdown);
        }
    }

    /// Follow a projected session event: headless output marks what a reopening terminal may have
    /// missed, and the end of a releasing writer's turn stops it.
    pub(super) fn observe_lease_frame(&mut self, session_id: &str, frame: &Frame) {
        let method = frame.method();
        // A turn in a shared terminal trades the lease: web input runs there under the web's
        // lease, and typing in the terminal takes it back.
        if method == method::TURN_STARTED
            && let Some(live) = self.sessions.get(session_id)
            && live.info.runtime == "claude-code"
            && live.shared_executor
            && let Some(thread) = live.runtime_thread_id.clone()
            && let Ok(started) = frame.params_as::<crate::protocol::TurnStarted>()
            && self.leases.turn_started(
                &thread,
                started.source == crate::protocol::TurnSource::Remote,
                now_ms(),
            )
        {
            self.save_leases();
        }
        let Some(thread) = self.headless_thread(session_id) else {
            return;
        };
        // Only the writer's own turns count. Everything it writes to the transcript lies inside a
        // turn that ends with `turn.completed`; items are tailed from the transcript, which the
        // terminal taking over writes too, and a status frame comes from a writer shutting down
        // between turns.
        // A writer stopped inside a turn reports no end to it, only its own end; this runs
        // before that status is projected, so `status` is still the one it ended in.
        let exited = method == method::SESSION_STATUS
            && frame
                .params_as::<crate::protocol::SessionStatusChanged>()
                .is_ok_and(|changed| changed.status == SessionStatus::Ended);
        let ended_in_turn = exited
            && self.sessions.get(session_id).is_some_and(|live| {
                matches!(
                    live.info.status,
                    SessionStatus::Running | SessionStatus::AwaitingApproval
                )
            });
        if ended_in_turn {
            self.leases.record_unobserved_output(&thread, now_ms());
        } else if matches!(
            method,
            method::TURN_STARTED | method::TURN_STEERED | method::TURN_COMPLETED
        ) {
            self.leases.record_output(&thread, now_ms());
        }
        // Its last record is known again once its plugin reports the turn's end. Saved at once:
        // a daemon that stops during the turn must not come back trusting the previous turn's.
        if method == method::TURN_STARTED {
            self.leases.writer_turn_started(&thread);
            self.save_leases();
        }
        if method == method::TURN_COMPLETED
            && self.leases.releasing(&thread)
            && let Some(live) = self.sessions.get(session_id)
        {
            let _ = live.tx.try_send(Command::Shutdown);
        }
    }

    /// The writer of `session_id`'s generation `generation` is reaped, or its cleanup failed: it
    /// is nobody's writer to hand over from any more.
    pub(super) fn writer_released(&mut self, session_id: &str, generation: u64) {
        if let Some(live) = self
            .sessions
            .get_mut(session_id)
            .filter(|live| live.generation == generation)
        {
            live.writer_released = true;
        }
    }

    /// A live row of `session_id` belongs to a generation other than `generation`.
    pub(super) fn newer_generation(&self, session_id: &str, generation: u64) -> bool {
        self.sessions
            .get(session_id)
            .is_some_and(|live| live.generation != generation)
    }

    /// A headless writer's process tree has been reaped: a terminal waiting for it takes the
    /// conversation. Its `ended` status comes earlier, while the process may still be exiting
    /// and appending its last records, and its supervisor's end later, after settlement and
    /// publication; neither hands over. The lease names the writer by session, so a release for
    /// a conversation someone else holds changes nothing.
    pub(super) fn lease_writer_released(&mut self, session_id: &str, thread: &str, in_turn: bool) {
        // A writer stopped inside a turn never reports that turn's end; what it wrote last is
        // as recent as now.
        if in_turn {
            self.leases.record_unobserved_output(thread, now_ms());
        }
        if self
            .leases
            .headless_ended(thread, session_id, &descends, now_ms())
        {
            self.save_leases();
        }
    }

    /// A headless writer's cleanup failed: its process may still write, so the conversation it
    /// was handing over stays with it, and the waiting terminal is told the way out.
    pub(super) fn lease_release_failed(
        &mut self,
        session_id: &str,
        thread: &str,
        process: Option<(u32, i64)>,
    ) {
        let writer = process.map(|(pid, started_ms)| Terminal {
            pid,
            started_ms,
            plugin: false,
        });
        if self
            .leases
            .release_failed(thread, session_id, writer, now_ms())
        {
            self.save_leases();
            match process {
                Some((pid, _)) => eprintln!(
                    "agitd: session {session_id} ended without releasing conversation {thread}; \
                     the terminal waiting for it stays held while process {pid} runs."
                ),
                None => eprintln!(
                    "agitd: session {session_id} ended without releasing conversation {thread}; \
                     the terminal waiting for it stays held. Make sure no `claude -p` for it is \
                     still running, then restart the daemon (`agit rc stop`, `agit rc start`)."
                ),
            }
        }
    }

    /// Stop headless writers whose handover deadline passed, so an unanswered approval cannot
    /// keep a reopened terminal waiting forever, and lift failed releases whose recorded writer
    /// process has gone.
    pub(super) fn enforce_lease_deadlines(&mut self) {
        if !self
            .leases
            .lift_gone_writers(terminal_alive, now_ms())
            .is_empty()
        {
            self.save_leases();
        }
        // A handover whose writer's row is gone and whose recorded process has exited will never
        // hear from that writer again: the waiting terminal takes over as stale. A writer that
        // still runs, or can't be judged, keeps the conversation waiting.
        let sessions = &self.sessions;
        let lifted = self.leases.lift_unobserved_releases(
            |session_id| sessions.contains_key(session_id),
            terminal_alive,
            now_ms(),
        );
        if !lifted.is_empty() {
            // A fixed name and a count: which conversation is in the lease file.
            eprintln!(
                "agitd: lease writer_ended_unobserved: {} conversation(s)",
                lifted.len()
            );
            self.save_leases();
        }
        for session_id in self.leases.overdue(now_ms()) {
            if let Some(live) = self.sessions.get(&session_id) {
                let _ = live.tx.try_send(Command::Shutdown);
            }
        }
    }

    /// Web input for a conversation nobody may write right now: while a headless writer hands
    /// it to a terminal (its turn boundary is the handover, so new input would only move it),
    /// and while the terminal holding it loaded an older history (input would continue from
    /// there). The message is not sent, and the refusal says why.
    pub(super) fn lease_refusal(&self, session_id: &str) -> Option<RpcError> {
        let live = self
            .sessions
            .get(session_id)
            .filter(|live| live.info.runtime == "claude-code")?;
        let thread = live.runtime_thread_id.as_deref()?;
        let (message, hint, reason) = match self.leases.get(thread)?.state {
            crate::rc::lease::State::Held => return None,
            crate::rc::lease::State::Releasing => (
                "not sent: this conversation is continuing in a terminal on this machine",
                "the turn already running finishes first; send it from that terminal, or here once \
                 the terminal holds the conversation",
                "lease_releasing",
            ),
            crate::rc::lease::State::ReleaseFailed => (
                "not sent: agit could not confirm that the previous process for this conversation \
                 has stopped",
                "nobody can continue it until that is checked on the device and agit's \
                 background service is restarted",
                "lease_release_failed",
            ),
            crate::rc::lease::State::Stale => (
                "not sent: the terminal holding this conversation continues an older branch of it",
                "the last turn sent from the web is not on that branch, and will not be in the \
                 next turn's context",
                "lease_stale",
            ),
        };
        Some(self.with_lease(
            classified(
                RpcError::new(ErrorCode::SessionBusy, message).with_hint(hint),
                false,
                reason,
            ),
            thread,
        ))
    }

    /// A web resume of a conversation whose last writer's release failed would start another
    /// writer beside a process that may still run; it is refused until the daemon restarts.
    pub(super) fn release_failed_refusal(&self, thread: &str) -> Option<RpcError> {
        self.leases
            .get(thread)
            .is_some_and(|lease| lease.state == crate::rc::lease::State::ReleaseFailed)
            .then(|| {
                self.with_lease(
                    classified(
                        RpcError::new(
                            ErrorCode::SessionBusy,
                            "not sent: agit could not confirm that the previous process for this \
                             conversation has stopped",
                        )
                        .with_hint(
                            "nobody can continue it until that is checked on the device and \
                             agit's background service is restarted",
                        ),
                        false,
                        "lease_release_failed",
                    ),
                    thread,
                )
            })
    }

    /// A refusal to start a second writer names the conversation's holder, so the web can say
    /// where it continues instead of showing an unexplained read-only state.
    pub(super) fn with_lease(&self, mut error: RpcError, thread: &str) -> RpcError {
        if let Some(view) = self
            .leases
            .get(thread)
            .map(|lease| lease.view(device_code().as_deref()))
        {
            let mut data = error.data.take().unwrap_or_else(|| serde_json::json!({}));
            if let Some(fields) = data.as_object_mut() {
                fields.insert("lease".into(), serde_json::json!(view));
            }
            error.data = Some(data);
        }
        error
    }

    /// The lease as the session's viewers see it.
    pub(super) fn lease_view(&self, session_id: &str) -> Option<crate::rc::lease::View> {
        let thread = self
            .sessions
            .get(session_id)
            .and_then(|live| live.runtime_thread_id.clone())
            .or_else(|| {
                self.roster
                    .get(session_id)
                    .filter(|entry| entry.runtime == "claude-code")
                    .map(|entry| entry.thread_id.clone())
            })?;
        self.leases
            .get(&thread)
            .map(|lease| lease.view(device_code().as_deref()))
    }
}
