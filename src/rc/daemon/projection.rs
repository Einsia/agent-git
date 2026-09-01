use super::*;

impl Daemon {
    /// Project one newly emitted session event into the daemon's live/durable
    /// state, stamp it, and retain it for replay.
    ///
    /// Both the ordinary pump and the shutdown tail use this entrance. Keeping
    /// the projection beside sequence allocation means shutdown cannot ship a
    /// permission fact while forgetting to update the roster that resume reads.
    pub(super) fn project_session_frame(&mut self, mut frame: Frame) -> Option<Frame> {
        // Consume provenance before any projection, sequencing, journaling or
        // outbound work. It is never retained or serialized after this point.
        if let Some(source_generation) = frame.source_generation.take() {
            let stream = frame.stream.as_deref();
            let fresh_notification = frame.is_notification()
                && frame.result.is_none()
                && frame.error.is_none()
                && frame.seq.is_none()
                && stream.is_some_and(|stream| !stream.is_empty());
            let generation_matches = stream.is_some_and(|stream| {
                self.latest_session_generations.get(stream) == Some(&source_generation)
            });
            if !fresh_notification || !generation_matches {
                if let Some(delivery) = frame.connection_delivery.as_ref() {
                    delivery.invalidate();
                }
                return None;
            }
        }
        debug_assert!(
            frame.is_notification() && frame.stream.is_some() && frame.seq.is_none(),
            "only a new session event needs projection and numbering"
        );
        let stream = frame.stream.clone().unwrap_or_default();

        // A synthetic hard-stop token is the sole source of truth for the
        // monotonic Plan floor. A delayed supervisor mode frame belongs to the
        // uncertain generation that forced that token; forwarding or applying
        // either Immediate or NextTurn would make the live view looser than the
        // crash-safe restart policy. Generation provenance was checked above,
        // so this is deliberately the next gate before any side effect, seq,
        // journal write, or wire visibility.
        if frame.method() == method::SESSION_PERMISSION_MODE
            && self
                .roster
                .sessions
                .get(&stream)
                .is_some_and(|entry| roster::has_shutdown_guard(&entry.guard_attempts))
        {
            if let Some(delivery) = frame.connection_delivery.as_ref() {
                delivery.invalidate();
            }
            return None;
        }

        // The supervisor owns a session's permission mode — one "do not ask
        // again" can change it, and that path does not pass through the daemon.
        // Follow the broadcast event to keep the daemon's copy current, or
        // `session.resume` / `session.list` report a mode to the web that
        // stopped holding long ago.
        if frame.method() == method::SESSION_PERMISSION_MODE
            && let Ok(p) = frame.params_as::<crate::protocol::SessionPermissionMode>()
            && let Some(live) = self.sessions.get_mut(&stream)
        {
            live.observe_permission_mode(&p);
            if let Err(e) = self.persist_session_state(&stream) {
                eprintln!("agitd: could not persist the session guard: {e:#}");
            }
        }
        // Capture the native session effect before this card is visible to the
        // hub. A later approval.decide may name only the approval id; it must
        // not be able to invent which mode the harness suggestion would apply.
        if frame.method() == method::APPROVAL_REQUEST
            && let Ok(p) = frame.params_as::<crate::protocol::ApprovalRequest>()
            && let Some(live) = self.sessions.get_mut(&stream)
        {
            match p.suggested_permission_mode {
                Some(mode) => {
                    live.approval_session_modes.insert(p.approval_id, mode);
                }
                None => {
                    live.approval_session_modes.remove(&p.approval_id);
                }
            }
        }
        if frame.method() == method::TURN_COMPLETED
            && let Some(live) = self.sessions.get_mut(&stream)
        {
            // The supervisor has expired both halves at this authoritative
            // boundary; daemon-side preflight metadata must not make an old
            // card look armable.
            live.approval_session_modes.clear();
        }
        // `stamped()` promises that `Live.info.status` reflects every event up
        // through its watermark, so project the authoritative status event.
        if frame.method() == method::SESSION_STATUS
            && let Ok(p) = frame.params_as::<crate::protocol::SessionStatusChanged>()
            && let Some(live) = self.sessions.get_mut(&stream)
        {
            live.info.status = p.status;
        }
        // `through_seq` is a journal coordinate. The supervisor cannot derive
        // it from transcript items because status/turn/approval events also
        // consume sequence numbers.
        if frame.method() == method::COMMIT_SETTLED
            && let Some(obj) = frame.params.as_mut().and_then(|p| p.as_object_mut())
        {
            let delivery = frame.connection_delivery.as_ref()?;
            let settlement = *self.settlement.borrow();
            if delivery.feature() != crate::protocol::ConnectionFeature::AgentIdentityV1
                || delivery.epoch() != settlement.epoch
                || !settlement.agent_identity_v1
            {
                delivery.invalidate();
                return None;
            }
            obj.insert(
                "through_seq".into(),
                serde_json::json!(self.journal.last_seq(&stream)),
            );
        }
        Some(self.journal.record(&stream, frame))
    }

    /// A session reports its harness id: this completes the test the
    /// second-writer guard applies, and lands the row in the roster.
    ///
    /// This path exists because a codex thread id does not exist yet at launch
    /// (it waits for `thread/started`). Reading the id once at launch leaves
    /// every new codex session with `None`: no guard, no roster row, and
    /// nothing to revive by logical id after a restart.
    pub(super) fn on_session_note(&mut self, note: SessionNote) {
        match note {
            SessionNote::TerminalExited { terminal_id } => {
                // Removing it triggers `Terminal`'s idempotent `Drop` cleanup;
                // the dedicated reaper has already waited on a shell that
                // exited on its own, and the abnormal path reaps the direct
                // children in the background. (A terminal stream never enters
                // the journal's ring anyway — see `Stream::retained`.)
                self.terminals.remove(&terminal_id);
            }
            SessionNote::DangerDisarmed {
                session_id,
                generation,
                arm,
            } => {
                // This does not wash the monotonic bit — the note comes only
                // from a command that was never accepted or from the harness
                // explicitly refusing the target mode, and both prove the
                // unchecked mode never took effect. A timeout or an I/O error
                // sends no note, because an unknown real mode must keep the
                // owner-only fail-closed bit. A generation that does not match
                // says it was armed again afterwards, and that bit is not this
                // note's to flip.
                if self
                    .sessions
                    .get(&session_id)
                    .is_none_or(|l| l.generation != generation)
                {
                    return;
                }
                self.disarm_danger(&session_id, arm);
            }
            SessionNote::RestartGuardReady {
                session_id,
                generation,
                ack,
            } => {
                let result = self
                    .clear_restart_guards_after_ready(&session_id, generation)
                    .map_err(|error| error.to_string());
                let _ = ack.send(result);
            }
            SessionNote::ObserveTurnGuard {
                session_id,
                generation,
                confirmation_token,
                ack,
            } => {
                let result = self
                    .observe_turn_guard(&session_id, generation, &confirmation_token)
                    .map_err(|error| error.to_string());
                let _ = ack.send(result);
            }
            SessionNote::ConfirmTurnRestartGuard {
                session_id,
                generation,
                confirmation_token,
                ack,
            } => {
                let result = self
                    .confirm_turn_guard(&session_id, generation, &confirmation_token)
                    .map_err(|error| error.to_string());
                let _ = ack.send(result);
            }
            SessionNote::FailClosedTurnGuard {
                session_id,
                generation,
                confirmation_token,
                ack,
            } => {
                let result = self
                    .fail_closed_turn_guard(&session_id, generation, confirmation_token)
                    .map_err(|error| error.to_string());
                let _ = ack.send(result);
            }
            SessionNote::Ended {
                session_id,
                generation,
            } => {
                // **Remove only your own generation.**
                //
                // One logical id can be brought back up (a `session.resume`
                // slips a new one in while the old supervisor is still winding
                // down). Without comparing generations, the note the old
                // generation sends on its way out removes the **new** one — the
                // freshly launched harness process is orphaned, and every web
                // operation on it answers "no live session", with no way back.
                // `WatchEnded` compares generations for the same reason.
                let gate = self
                    .sessions
                    .get(&session_id)
                    .filter(|live| live.generation == generation)
                    .map(|live| live.rpc_gate.clone());
                if let Some(gate) = gate {
                    // Never await this per-session gate while holding the
                    // daemon mutex. A command completion owns the gate and
                    // must reacquire this mutex to project/roll back state.
                    if gate.try_lock_owned().is_ok() {
                        self.remove_session_generation(&session_id, generation);
                    } else if let Some(live) = self.sessions.get_mut(&session_id) {
                        live.ended = true;
                        live.info.status = SessionStatus::Ended;
                    }
                }
            }
            SessionNote::WatchEnded { stream, generation } => {
                // **Remove only your own generation.**
                //
                // While this note waits in the queue, a new `session.watch` may
                // already have rebuilt the same key (the tail task exits → the
                // note is queued → someone starts watching again → only then is
                // the note consumed). An unconditional `remove` takes that
                // freshly built tail out of the table while its task still
                // runs: to the viewer the live stream stops with no warning,
                // and the machine keeps a poll nobody references and nobody
                // aborts.
                if self
                    .watches
                    .get(&stream)
                    .is_some_and(|w| w.generation == generation)
                {
                    self.take_watch(&stream);
                }
            }
            SessionNote::Bound {
                session_id,
                generation,
                runtime_thread_id,
                cwd,
                agit_session,
                expected_agent_id,
            } => {
                let Some(live) = self.sessions.get_mut(&session_id) else {
                    return;
                };
                // **Accept only your own generation.** Same reason as `Ended`:
                // an old supervisor can queue this note and then sit for a long
                // time landing the lineage (network + clone); if the same
                // logical id is brought back up in that window, the stale note
                // rewrites the **new** generation's live entry and roster row —
                // overwriting the new session's thread id with the old one, so
                // the second-writer guard and where settlement lands both point
                // at the wrong session.
                if live.generation != generation {
                    return;
                }
                // Always rebuild and save the complete durable row. Thread id
                // alone is not an idempotency key: a clean roster row can gain
                // its first negotiated lineage while keeping the same harness
                // thread. Returning on `thread_id == runtime_thread_id` drops
                // that immutable identity, so the next daemon start sees the
                // row as unclaimed again. Re-saving an identical second Bound
                // note is cheap, idempotent, and also retries a failed first
                // atomic save instead of making the failure permanent.
                live.runtime_thread_id = Some(runtime_thread_id.clone());
                let info = live.info.clone();
                let prior = self.roster.sessions.get(&session_id);
                let guard_attempts = prior
                    .map(|entry| entry.guard_attempts.clone())
                    .unwrap_or_default();
                // **Repository identity only grows.**
                //
                // This note carries the lineage the supervisor came up under
                // this time, and that may legitimately be `None`: with the
                // socket not ACKing `agent_identity_v1` (an old hub, or the ACK
                // not there yet) `resume_lineage` returns `None`, and the
                // session runs as usual, it just does not settle (the
                // `sessions.rs` line "it will run but will not settle"). This
                // handler rebuilds the whole row, so writing `None` down
                // **erases** that row's durable identity on disk.
                //
                // An erased row reads as "never claimed any identity", and that
                // is exactly the row `resume_lineage` runs `lineage_from_params`
                // on: the next resume over a normal socket lets the current hub
                // fill it in with the **new** id of a same-named agent. When the
                // name is reused (the agent deleted and created again), every
                // later turn of this session settles into another repo — the
                // very thing `resume_lineage`'s docs say cannot happen.
                //
                // So: write what is there, and keep what is on disk when there
                // is not. The one path that really changes identity is starting
                // a new session, which is another logical id and another row.
                let agit_session =
                    agit_session.or_else(|| prior.and_then(|e| e.agit_session.clone()));
                let expected_agent_id =
                    expected_agent_id.or_else(|| prior.and_then(|e| e.expected_agent_id.clone()));
                let permission_mode =
                    roster::mode_with_shutdown_floor(&guard_attempts, info.permission_mode);
                if roster::has_shutdown_guard(&guard_attempts) {
                    live.info.permission_mode = Some(crate::protocol::PermissionMode::Plan);
                    live.pending_mode = None;
                }
                self.roster.record(
                    &session_id,
                    roster::Entry {
                        runtime: info.runtime.clone(),
                        thread_id: runtime_thread_id,
                        cwd,
                        workspace_id: info.workspace_id.clone(),
                        project_id: info.project_id.clone(),
                        agit_session,
                        expected_agent_id,
                        permission_mode,
                        guard_attempts,
                        ever_dangerous: info.dangerous,
                        // `record` carries the prior row's old thread id over.
                        prior_threads: vec![],
                    },
                );
                // Only now is the current thread binding on the record, and the
                // poison lifts with it — but **only once this save really
                // reaches disk**.
                let confirmed = self.roster.confirm_binding(&session_id);
                if let Err(e) = self.roster.save() {
                    // Not persisted: disk still holds the old id (claude's
                    // slow-path recovery has just swapped one in), so the new
                    // transcript is still unowned as far as the ledger is
                    // concerned. Letting go in memory first lets any operator,
                    // for as long as this daemon lives, take it over under the
                    // new id and mint a clean identity that picks up everything
                    // this unchecked run read into its context. Put it back and
                    // let the next `Bound` — resent before every turn's
                    // settlement — retry it.
                    if confirmed {
                        self.roster.arm_unconfirmed_binding(&session_id);
                    }
                    eprintln!("agitd: could not persist the session roster: {e:#}");
                }
            }
        }
    }

    /// A live session **that the caller's workspace actually owns**.
    ///
    /// Addressing a session by id alone is not enough: one machine serves
    /// several workspaces at once, and every session-scoped verb takes the id
    /// from the client. Without this check an operator of workspace A can name
    /// a session belonging to workspace B and drive it — the role was real, it
    /// was just spent in the wrong tenant. Path overlap is not a tenant
    /// boundary either, which is why this compares workspace ids and not cwds.
    /// A session's command channel — **and** this frame's right to touch it at
    /// the `need` class.
    ///
    /// This is the only path from dispatch to the `Live::tx` of a session that
    /// is **already alive**, with no sibling accessor beside it: a new session
    /// verb that wants the channel goes through this gate and has to pick a
    /// `Need` for it, so a half guard — the mode switch judged, the message
    /// send not — cannot appear.
    ///
    /// `caller` is taken as `&CallerClaim` and not `Option<&_>`: under the
    /// optional shape `caller == None` **skips** the ownership check and lets
    /// the frame straight through — a missing credential read as a wildcard,
    /// the exact opposite of `require_owner_for_danger`, which treats `None` as
    /// the strictest case. `caller_scope` takes the caller out before dispatch
    /// and rejects `None` there, so the type that would express that failure
    /// state does not exist here.
    pub(super) fn session_channel(
        &self,
        session_id: &str,
        caller: &crate::protocol::CallerClaim,
        need: Need,
    ) -> Result<Driving, RpcError> {
        let live = self
            .sessions
            .get(session_id)
            .ok_or_else(|| no_such_session(session_id))?;
        if live.ended {
            return Err(no_such_session(session_id));
        }
        require_same_workspace(caller, session_id, &live.info.workspace_id)?;
        if need == Need::Drive && !live.restart_guard_attempts.is_empty() {
            return Err(RpcError::new(
                ErrorCode::SessionBusy,
                "this resumed session is still proving its fail-closed Plan restart",
            )
            .with_hint(
                "nothing was queued; retry after the harness reports Ready (interrupt and deny remain available)",
            ));
        }
        if need == Need::Drive {
            require_owner_to_drive(Some(caller), live.info.dangerous)?;
        }
        Ok(Driving {
            tx: live.tx.clone(),
            runtime: live.info.runtime.clone(),
        })
    }

    /// A live session **of this workspace**, found by harness thread id or by
    /// logical id.
    ///
    /// `session.resume` starts by asking whether it is already supervised —
    /// that is a **data integrity** guard (no second writer on one transcript),
    /// not a window that hands out sessions. It addresses by an id the client
    /// supplies, the same class as `turn.start`, so it judges ownership too: a
    /// `find` over `self.sessions.values()` that does **not** compare
    /// workspaces lets a member of B pass a harness thread id from A and get
    /// A's `SessionInfo` back verbatim.
    pub(super) fn supervised_in(
        &self,
        needle: &str,
        caller: &crate::protocol::CallerClaim,
    ) -> Option<SessionInfo> {
        self.sessions
            .values()
            .find(|l| {
                !l.ended
                    && l.info.workspace_id == caller.workspace_id
                    && (l.runtime_thread_id.as_deref() == Some(needle)
                        || l.info.session_id == needle)
            })
            .map(|l| l.info.clone())
    }

    pub(super) fn ending_in(&self, needle: &str, caller: &crate::protocol::CallerClaim) -> bool {
        self.sessions.values().any(|live| {
            live.ended
                && live.info.workspace_id == caller.workspace_id
                && (live.runtime_thread_id.as_deref() == Some(needle)
                    || live.info.session_id == needle)
        })
    }

    /// An open terminal, **already confirmed to belong to the caller's
    /// workspace**.
    ///
    /// Same shape and same reason as [`Daemon::session_channel`]: every
    /// terminal verb addresses by an id alone, so the test sits in the one
    /// function that can hand out a `Terminal`.
    pub(super) fn terminal_owned_by(
        &self,
        terminal_id: &str,
        caller: &crate::protocol::CallerClaim,
    ) -> Result<&Terminal, RpcError> {
        let t = self
            .terminals
            .get(terminal_id)
            .filter(|t| t.workspace_id == caller.workspace_id)
            .ok_or_else(|| RpcError::new(ErrorCode::SessionNotFound, "that terminal is gone"))?;
        Ok(&t.term)
    }

    /// Stamp a `SessionInfo` bound for a viewer with the seq watermark of this
    /// moment.
    ///
    /// A response carries no seq of its own — it answers one request and does
    /// not enter the journal (entering would punch a hole in the stream). The
    /// web still takes it as a baseline: yesterday's `session.status(ended)`
    /// and the `idle` that today's `session.resume` returns cannot be ordered
    /// from the responses alone, so the stale one wins forever — the composer
    /// locks up, and **no event comes to unlock it**: the supervisor's
    /// `set_status` emits an event only when the status **changes**, and a
    /// freshly started session is idle from the start.
    ///
    /// `last_seq` supplies exactly that dimension: **this status already
    /// reflects every event with seq ≤ last_seq**, and those events may not
    /// reopen the question; every frame emitted after it carries a larger seq
    /// and overrides it as usual.
    ///
    /// Reading it must happen inside the same lock as taking `info` — numbering
    /// frames in the main pump wants that same lock, and `dispatch` holds it
    /// throughout. That same-lock invariant is the whole reason the web can
    /// make that comparison.
    pub(super) fn stamped(&self, mut info: SessionInfo) -> SessionInfo {
        info.last_seq = self.journal.last_seq(&info.session_id);
        info
    }

    pub(super) fn remove_session_generation(&mut self, session_id: &str, generation: u64) {
        if self
            .sessions
            .get(session_id)
            .is_some_and(|live| live.generation == generation)
        {
            // Keep the roster: it is the restart identity and durable danger
            // ledger. Only the live handle and replay ring end here.
            self.sessions.remove(session_id);
            self.journal.forget(session_id);
        }
    }

    /// Retire a generation whose post-launch bootstrap command could not reach
    /// its supervisor without waiting for that task under the daemon mutex.
    ///
    /// A supervisor always ends by sending `SessionNote::Ended`. That send may
    /// itself be parked behind a full notes channel whose consumer needs this
    /// mutex, so joining here would deadlock the entire daemon. Detach a tiny
    /// reaper instead and pair the earlier `journal.resume()` immediately with
    /// `forget`; the eventual generation-fenced Ended note is then a no-op.
    pub(super) fn detach_failed_session_generation(&mut self, session_id: &str, generation: u64) {
        let matches = self
            .sessions
            .get(session_id)
            .is_some_and(|live| live.generation == generation);
        if !matches {
            return;
        }
        let live = self
            .sessions
            .remove(session_id)
            .expect("failed generation was checked above");
        self.journal.forget(session_id);
        let Live { task, tx, .. } = live;
        drop(tx);
        tokio::spawn(async move {
            let _ = task.await;
        });
    }

    /// This workspace's confinement **right now**, as a subscription a session
    /// can keep watching.
    ///
    /// One `watch` channel per workspace, kept once it is built — sessions hold
    /// the `Receiver`, and dropping the `Sender` closes the channel under them
    /// at once.
    pub(super) fn confinement_for(
        &mut self,
        workspace_id: &str,
    ) -> tokio::sync::watch::Receiver<crate::rc::Confinement> {
        let now = crate::rc::Confinement {
            roots: self.mirror.roots(workspace_id),
            operator_heads: self.grants.for_workspace(workspace_id),
        };
        match self.confinement.get(workspace_id) {
            Some(tx) => {
                // **`send_replace`, not `send`.**
                //
                // `watch::Sender::send` returns Err when there is **no
                // receiver**, and it **writes no new value** then. Once this
                // workspace's last session ends every receiver is gone while
                // the sender stays in the table: every refresh after that
                // (`agit rc grant`, unbinding a directory) spins for nothing
                // and the value stays at the old one. The next session to come
                // up and `subscribe()` gets exactly that **stale** confinement —
                // a directory that was just unbound is still inside it, or a
                // command that was just granted is not. And this value is the
                // whole basis for staying confined.
                tx.send_replace(now);
                tx.subscribe()
            }
            None => {
                let (tx, rx) = tokio::sync::watch::channel(now);
                self.confinement.insert(workspace_id.to_string(), tx);
                rx
            }
        }
    }

    /// Reap read-only tails that have been quiet for too long.
    ///
    /// **The test lives in the daemon, not in the tail.** If the tail judged
    /// for itself, "it is about to exit" and "someone is about to join" are two
    /// independent parties looking at two different moments: after the tail
    /// reads "no new viewer" and before it actually breaks, a new viewer joins
    /// and sees `is_finished()` still false — so they hang off a dying tail and
    /// not one frame ever comes. Here, "add a viewer" and "close up shop" are
    /// under the same lock, and that gap does not exist.
    ///
    /// The backstop itself stays necessary: a viewer can leave by closing the
    /// tab, `session.unwatch` then never arrives, and every session anyone has
    /// watched leaves a poll behind that never stops.
    pub(super) fn reap_idle_watches(&mut self) {
        for k in stale_watches(&self.watches, now_secs()) {
            if let Some(w) = self.take_watch(&k) {
                w.handle.abort();
            }
        }
    }

    /// Drop a read-only tail and release its replay ring in the journal.
    ///
    /// A watch stream retains the same bounded ring as an ordinary session, but
    /// it has three exits with no natural `WatchEnded` (an explicit unwatch,
    /// idle reaping, and finding the old task already finished on reopen).
    /// `forget` funnels into the single remove entry point, so none of them
    /// leaves a whole ring behind forever. `Journal::forget` only clears frames
    /// and never rewinds seq, so reopening the same id keeps the watermark
    /// monotonic.
    pub(super) fn take_watch(&mut self, stream: &str) -> Option<WatchLive> {
        let watch = self.watches.remove(stream);
        if watch.is_some() {
            self.journal.forget(stream);
        }
        watch
    }

    /// Re-read the grant list from disk; broadcast only when it really changed.
    ///
    /// Compare before sending because `watch::Sender::send` wakes every
    /// receiver and this heartbeat never stops. Unchanged means nothing
    /// happens.
    pub(super) fn reload_grants(&mut self) {
        let next = crate::rc::grants::Grants::load();
        if next.heads != self.grants.heads {
            self.grants = next;
            self.refresh_confinement();
        }
    }

    /// Recompute every workspace's confinement and push it to the running
    /// sessions.
    ///
    /// Call it whenever the mirror changes (a reconnect where the hub states
    /// new workspace definitions, a `project.bind` / `project.unbind`).
    /// Without the call, a directory that was just unbound is still a pass in
    /// the operator's hand until that session ends.
    pub(super) fn refresh_confinement(&mut self) {
        for (ws, tx) in &self.confinement {
            // As above: with no receiver `send` writes no value, and this is
            // exactly where the value must be updated.
            tx.send_replace(crate::rc::Confinement {
                roots: self.mirror.roots(ws),
                operator_heads: self.grants.for_workspace(ws),
            });
        }
    }
}

#[cfg(test)]
mod bound_lineage_tests {
    use super::*;

    const A1: &str = "00000000-0000-0000-0000-000000000001";
    const A2: &str = "00000000-0000-0000-0000-000000000002";

    fn claimed_row() -> roster::Entry {
        roster::Entry {
            runtime: "claude-code".into(),
            thread_id: "native-1".into(),
            cwd: "/tmp".into(),
            workspace_id: "ws1".into(),
            project_id: Some("project-1".into()),
            agit_session: Some("acme/api@s/1".into()),
            expected_agent_id: Some(A1.into()),
            permission_mode: Some(crate::protocol::PermissionMode::Default),
            guard_attempts: Default::default(),
            prior_threads: vec![],
            ever_dangerous: false,
        }
    }

    /// A `Bound` note **without lineage** may not erase the repository identity
    /// this row already claimed.
    ///
    /// The supervisor reports "what lineage it came up under this time", and
    /// that may legitimately be `None`: with the socket not ACKing
    /// `agent_identity_v1` (an old hub, or the ACK lost / not there yet)
    /// `resume_lineage` returns `None`, and the session runs as usual, it just
    /// does not settle. This handler **rebuilds the whole row**, and
    /// `Roster::record` keeps only `prior_threads` / `ever_dangerous` /
    /// `guard_attempts` — so the row on disk loses `agit_session` and
    /// `expected_agent_id` together, reading as "never claimed any identity".
    ///
    /// And that is exactly the row `resume_lineage` runs `lineage_from_params`
    /// on: the next resume over a normal socket lets the current hub fill it in
    /// with the new agent id of the **same name**. When the name is reused (the
    /// agent deleted and created again), every later turn of this session
    /// settles into another repo — the very thing `resume_lineage`'s docs say
    /// cannot happen.
    #[test]
    fn a_bound_note_without_lineage_never_erases_a_rows_repository_identity() {
        let home = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(home.path(), || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let (cmd_tx, _cmd_rx) = mpsc::channel(1);
                    let info = SessionInfo {
                        session_id: "agit-S".into(),
                        workspace_id: "ws1".into(),
                        project_id: Some("project-1".into()),
                        runtime: "claude-code".into(),
                        agent: None,
                        branch: None,
                        status: SessionStatus::Running,
                        last_seq: 0,
                        gist: None,
                        dangerous: false,
                        permission_mode: Some(crate::protocol::PermissionMode::Default),
                        created_at: "now".into(),
                        updated_at: "now".into(),
                    };
                    let mut roster = Roster::default();
                    roster.sessions.insert("agit-S".into(), claimed_row());
                    let live = Live {
                        generation: 3,
                        info,
                        tx: cmd_tx,
                        runtime_thread_id: Some("native-1".into()),
                        task: tokio::spawn(async {}),
                        danger_arm: 0,
                        pending_mode: None,
                        approval_session_modes: HashMap::new(),
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
                    let mut daemon = Daemon {
                        deferred: vec![],
                        deferred_slot: None,
                        replay_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(
                            REPLAY_SLOTS,
                        )),
                        outbound: None,
                        opts: Options {
                            hub: "https://hub.invalid".into(),
                            token: "test".into(),
                            connection_id: None,
                        },
                        journal: Journal::new(),
                        mirror: Mirror::default(),
                        roster,
                        sessions: [("agit-S".to_string(), live)].into_iter().collect(),
                        latest_session_generations: HashMap::new(),
                        watches: HashMap::new(),
                        terminals: HashMap::new(),
                        terminal_delivery_blockers: std::sync::Arc::new(
                            std::sync::atomic::AtomicUsize::new(0),
                        ),
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

                    // Resume over a socket that never ACKed the identity
                    // feature: the supervisor comes up carrying
                    // `agit_session: None`, and **resends this note before every
                    // turn's settlement**.
                    daemon.on_session_note(SessionNote::Bound {
                        session_id: "agit-S".into(),
                        generation: 3,
                        runtime_thread_id: "native-2".into(),
                        cwd: "/tmp".into(),
                        agit_session: None,
                        expected_agent_id: None,
                    });

                    // The row that reached disk still claims the original
                    // identity — a restart reads back exactly this.
                    let on_disk = Roster::load();
                    let entry = on_disk
                        .get("agit-S")
                        .expect("the row is still there")
                        .clone();
                    assert_eq!(entry.thread_id, "native-2", "thread id updates as usual");
                    assert_eq!(entry.agit_session.as_deref(), Some("acme/api@s/1"));
                    assert_eq!(entry.expected_agent_id.as_deref(), Some(A1));

                    // The consequence this really blocks: `acme/api` is deleted
                    // and created again (same name, new id), and the next
                    // resume over a normal socket must not let the current hub
                    // fill the new id in.
                    let adopted =
                        resume_lineage(true, &entry, Some("acme/api"), Some(A2), Some("s/2"))
                            .expect("a claimed row does not error on these params");
                    assert_eq!(
                        adopted.as_ref().map(|l| l.agent_id().to_string()),
                        Some(A1.to_string()),
                        "resume keeps this row's identity, not a same-named agent's new id"
                    );
                    assert_eq!(
                        adopted.as_ref().map(|l| l.branch().to_string()),
                        Some("s/1".to_string())
                    );
                });
        });
    }
}
