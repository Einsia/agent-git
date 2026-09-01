use super::*;

impl Daemon {
    /// Validate and snapshot one supervisor command while holding the daemon's
    /// state mutex. This function never waits on either the command queue or a
    /// receipt; the returned request owns only its target session's serial
    /// guard across those awaits.
    pub(super) fn prearm_turn_guard_attempt(
        &mut self,
        session_id: &str,
    ) -> Result<Option<crate::rc::harness::TurnGuardAttempt>, RpcError> {
        let Some(expected_mode) = self
            .sessions
            .get(session_id)
            .and_then(|live| live.pending_mode)
        else {
            return Ok(None);
        };
        let token = uuid::Uuid::new_v4().to_string();
        let Some(entry) = self.roster.sessions.get_mut(session_id) else {
            return Err(RpcError::new(
                ErrorCode::Internal,
                "this session has not been recorded yet; refusing to enqueue a guard-sensitive turn",
            ));
        };
        entry.guard_attempts.insert(
            token.clone(),
            roster::GuardAttempt {
                expected_mode,
                observed: false,
            },
        );
        if let Err(error) = self.roster.save_fail_closed() {
            if let Some(entry) = self.roster.sessions.get_mut(session_id) {
                entry.guard_attempts.remove(&token);
            }
            return Err(RpcError::new(
                ErrorCode::Internal,
                format!("could not arm the turn's crash-safe restart guard: {error}"),
            )
            .with_hint("nothing was queued or written to the harness; restore ~/.agit/rc writability and retry"));
        }
        if let Some(live) = self.sessions.get_mut(session_id) {
            live.inflight_turn_guard = Some(token.clone());
        }
        Ok(Some(crate::rc::harness::TurnGuardAttempt {
            token,
            expected_mode,
        }))
    }

    pub(super) fn durable_guard_row_complete(&self, session_id: &str) -> bool {
        self.roster
            .sessions
            .get(session_id)
            .is_some_and(|entry| !entry.thread_id.trim().is_empty())
    }

    pub(super) fn durable_guard_pending_error() -> RpcError {
        RpcError::new(
            ErrorCode::SessionBusy,
            "this session's native restart identity is still being recorded",
        )
        .with_hint("nothing was queued; wait for the harness binding and retry")
    }

    /// A hard-stop guard can only be crash-safe if the complete resume row is
    /// already durable before the native command becomes TAKEN. The production
    /// frame pump waits for a new harness's `Bound` note outside the daemon
    /// mutex; reaching this function therefore means the row is complete and
    /// only its fail-closed refresh can still fail.
    pub(super) fn require_durable_guard_row(&mut self, session_id: &str) -> Result<(), RpcError> {
        if !self.durable_guard_row_complete(session_id) {
            return Err(Self::durable_guard_pending_error());
        }
        self.persist_session_state_fail_closed(session_id)
            .map_err(|error| {
                RpcError::new(
                    ErrorCode::Internal,
                    format!("could not durably refresh this session's restart guard: {error:#}"),
                )
                .with_hint(
                    "nothing was queued; restore ~/.agit/rc writability and retry the instruction",
                )
            })
    }

    pub(super) fn observe_turn_guard(
        &mut self,
        session_id: &str,
        generation: u64,
        confirmation_token: &str,
    ) -> crate::Result<()> {
        if confirmation_token.starts_with(roster::SHUTDOWN_GUARD_PREFIX) {
            anyhow::bail!("a native turn observation cannot consume a shutdown guard token");
        }
        let Some(_) = self
            .sessions
            .get(session_id)
            .filter(|live| live.generation == generation)
        else {
            anyhow::bail!("session generation disappeared before its inferred guard was observed");
        };
        let Some(attempt) = self
            .roster
            .sessions
            .get(session_id)
            .and_then(|entry| entry.guard_attempts.get(confirmation_token))
            .cloned()
        else {
            anyhow::bail!("the pre-dispatch guard token disappeared before native observation");
        };
        let shutdown_floor = self
            .roster
            .sessions
            .get(session_id)
            .is_some_and(|entry| roster::has_shutdown_guard(&entry.guard_attempts));
        let effective_mode = if shutdown_floor {
            crate::protocol::PermissionMode::Plan
        } else {
            attempt.expected_mode
        };
        let live = self
            .sessions
            .get_mut(session_id)
            .expect("generation was checked above");
        live.info.permission_mode = Some(effective_mode);
        live.pending_mode = None;
        // The native observation still counts for danger history even when the
        // synthetic shutdown floor keeps the advertised/restart mode at Plan.
        live.info.dangerous = live.info.dangerous || attempt.expected_mode.is_dangerous();
        let dangerous = live.info.dangerous;
        let Some(entry) = self.roster.sessions.get_mut(session_id) else {
            anyhow::bail!("session has no roster row for an inferred guard transition");
        };
        entry.permission_mode = Some(effective_mode);
        entry.ever_dangerous = entry.ever_dangerous || dangerous;
        entry
            .guard_attempts
            .get_mut(confirmation_token)
            .expect("attempt was read above")
            .observed = true;
        self.roster.save_fail_closed()
    }

    pub(super) fn clear_restart_guards_after_ready(
        &mut self,
        session_id: &str,
        generation: u64,
    ) -> crate::Result<()> {
        let Some((snapshot, launch_mode)) = self
            .sessions
            .get(session_id)
            .filter(|live| live.generation == generation)
            .map(|live| (live.restart_guard_attempts.clone(), live.restart_guard_mode))
        else {
            anyhow::bail!("session generation disappeared before its restart guard was ready");
        };
        if snapshot.is_empty() {
            return Ok(());
        }
        let Some(launch_mode) = launch_mode else {
            anyhow::bail!("restart guard has no authoritative native launch mode");
        };

        let Some(entry) = self.roster.sessions.get_mut(session_id) else {
            anyhow::bail!("session has no roster row for its restart guard");
        };
        let previous_mode = entry.permission_mode;
        let mut removed = Vec::new();
        for token in &snapshot {
            if let Some(attempt) = entry.guard_attempts.remove(token) {
                removed.push((token.clone(), attempt));
            }
        }
        // This generation was launched with the fail-closed restart mode. Its
        // native Ready event is the first evidence that Plan is now the real
        // baseline rather than only an override for an older ambiguous write.
        entry.permission_mode = Some(launch_mode);
        if let Err(error) = self.roster.save_fail_closed() {
            let entry = self
                .roster
                .sessions
                .get_mut(session_id)
                .expect("entry was borrowed above");
            entry.permission_mode = previous_mode;
            entry.guard_attempts.extend(removed);
            return Err(error);
        }

        let live = self
            .sessions
            .get_mut(session_id)
            .expect("generation was checked above");
        live.info.permission_mode = Some(launch_mode);
        live.restart_guard_attempts.clear();
        live.restart_guard_mode = None;
        Ok(())
    }

    pub(super) fn confirm_turn_guard(
        &mut self,
        session_id: &str,
        generation: u64,
        confirmation_token: &str,
    ) -> crate::Result<()> {
        if confirmation_token.starts_with(roster::SHUTDOWN_GUARD_PREFIX) {
            anyhow::bail!("a native turn confirmation cannot consume a shutdown guard token");
        }
        let Some(_) = self
            .sessions
            .get(session_id)
            .filter(|live| live.generation == generation)
        else {
            anyhow::bail!("session generation disappeared before turn confirmation");
        };
        let already_confirmed = self
            .sessions
            .get(session_id)
            .and_then(|live| live.confirmed_turn_guards.get(confirmation_token))
            .copied();
        let Some(entry) = self.roster.sessions.get_mut(session_id) else {
            anyhow::bail!("session has no roster row for turn confirmation");
        };
        let Some(attempt) = entry.guard_attempts.get(confirmation_token).cloned() else {
            return match already_confirmed {
                Some(_) => Ok(()),
                None => anyhow::bail!(
                    "the turn confirmation token is neither armed nor already confirmed"
                ),
            };
        };
        if !attempt.observed {
            anyhow::bail!("late exact response overtook notification-mode projection");
        }
        entry.guard_attempts.remove(confirmation_token);
        if let Err(error) = self.roster.save_fail_closed() {
            let entry = self
                .roster
                .sessions
                .get_mut(session_id)
                .expect("entry was borrowed above");
            entry
                .guard_attempts
                .insert(confirmation_token.to_string(), attempt);
            return Err(error);
        }
        if let Some(live) = self.sessions.get_mut(session_id)
            && live.inflight_turn_guard.as_deref() == Some(confirmation_token)
        {
            live.confirmed_turn_guards
                .insert(confirmation_token.to_string(), attempt.expected_mode);
        }
        Ok(())
    }

    pub(super) fn fail_closed_turn_guard(
        &mut self,
        session_id: &str,
        generation: u64,
        confirmation_token: String,
    ) -> crate::Result<()> {
        if confirmation_token.starts_with(roster::SHUTDOWN_GUARD_PREFIX) {
            anyhow::bail!("a native turn contradiction cannot reuse a shutdown guard token");
        }
        let Some(live) = self
            .sessions
            .get(session_id)
            .filter(|live| live.generation == generation)
        else {
            anyhow::bail!("session generation disappeared before fail-closed confirmation");
        };
        let dangerous = live.info.dangerous;
        let confirmed = live.confirmed_turn_guards.get(&confirmation_token).copied();
        let Some(entry) = self.roster.sessions.get_mut(session_id) else {
            anyhow::bail!("session has no roster row for fail-closed confirmation");
        };
        entry.ever_dangerous = entry.ever_dangerous || dangerous;
        if let std::collections::btree_map::Entry::Vacant(slot) =
            entry.guard_attempts.entry(confirmation_token)
        {
            let Some(expected_mode) = confirmed else {
                anyhow::bail!("contradicted turn guard token is neither armed nor confirmed");
            };
            slot.insert(roster::GuardAttempt {
                expected_mode,
                observed: true,
            });
        }
        self.roster.save_fail_closed()
    }

    /// Write the session's durable state (guard + danger bit) into the roster.
    ///
    /// One function so the two facts cannot drift: the roster is what a restart
    /// reads back, and a session that comes back with a *weaker* guard than it
    /// had is a silent privilege grant.
    pub(super) fn persist_session_state(&mut self, session_id: &str) -> crate::Result<()> {
        let shutdown_floor = self
            .roster
            .sessions
            .get(session_id)
            .is_some_and(|entry| roster::has_shutdown_guard(&entry.guard_attempts));
        let Some(live) = self.sessions.get_mut(session_id) else {
            return Ok(());
        };
        if shutdown_floor {
            live.info.permission_mode = Some(crate::protocol::PermissionMode::Plan);
            live.pending_mode = None;
        }
        let (mode, dangerous) = (live.info.permission_mode, live.info.dangerous);
        if let Some(e) = self.roster.sessions.get_mut(session_id) {
            e.permission_mode = roster::mode_with_shutdown_floor(&e.guard_attempts, mode);
            e.ever_dangerous = e.ever_dangerous || dangerous;
            return self.roster.save();
        }
        Ok(())
    }

    /// Durable completion barrier for a known guard transition.
    ///
    /// Unlike ordinary best-effort event projection, absence of a roster row
    /// is an error: the RPC may not acknowledge a mode that restart cannot
    /// reproduce. `save_fail_closed` gives a synced fallback when the primary
    /// atomic replacement fails.
    pub(super) fn persist_session_state_fail_closed(
        &mut self,
        session_id: &str,
    ) -> crate::Result<()> {
        let shutdown_floor = self
            .roster
            .sessions
            .get(session_id)
            .is_some_and(|entry| roster::has_shutdown_guard(&entry.guard_attempts));
        let Some(live) = self.sessions.get_mut(session_id) else {
            anyhow::bail!("session disappeared before its guard could be persisted");
        };
        if shutdown_floor {
            live.info.permission_mode = Some(crate::protocol::PermissionMode::Plan);
            live.pending_mode = None;
        }
        let (mode, dangerous) = (live.info.permission_mode, live.info.dangerous);
        let Some(entry) = self.roster.sessions.get_mut(session_id) else {
            anyhow::bail!(
                "session has not been recorded yet; waiting for the harness binding before acknowledging its guard"
            );
        };
        anyhow::ensure!(
            !entry.thread_id.trim().is_empty(),
            "session start is recorded, but its native thread id is still pending; waiting for the harness binding before acknowledging its guard"
        );
        entry.permission_mode = roster::mode_with_shutdown_floor(&entry.guard_attempts, mode);
        entry.ever_dangerous = entry.ever_dangerous || dangerous;
        self.roster.save_fail_closed()
    }

    /// Flip back the bit pre-written for a guard change **proven not to have
    /// taken effect**.
    ///
    /// Three kinds of evidence: the execution side withdrew the instruction, the
    /// enqueue failed on the spot (both prove it never ran), or the harness took
    /// the instruction and then explicitly refused the target mode. A timeout, an
    /// I/O error or an incomplete echo must never come through here.
    ///
    /// `arm` is that arming's number; a mismatch changes nothing — the same
    /// session may have been armed again while the notification was in flight,
    /// and **that** arming may really have taken effect.
    pub(super) fn disarm_danger(&mut self, session_id: &str, arm: u64) {
        let Some(live) = self.sessions.get_mut(session_id) else {
            return;
        };
        if live.danger_arm != arm {
            return;
        }
        live.info.dangerous = false;
        if let Some(e) = self.roster.sessions.get_mut(session_id) {
            e.ever_dangerous = false;
        }
        if let Err(e) = self.persist_session_state(session_id) {
            // Failing to flip it back only means "this session stays
            // owner-only", far safer than flipping it the wrong way.
            eprintln!("agitd: could not clear a withdrawn guard change: {e:#}");
        }
    }

    /// **Persist the danger bit first, then hand the loosening to the harness.**
    ///
    /// The consequence of reversing the order is not "slower": when
    /// `~/.agit/rc/sessions.json` cannot be written (ENOSPC, or an earlier
    /// `sudo agit rc start` left it owned by root), `set_permission_mode` has
    /// already acknowledged, the CLI is already running unsupervised, and the
    /// `save()` failure is swallowed by a `let _ =`. After agitd restarts, the
    /// roster reads back `ever_dangerous: false`, so **any** operator in the
    /// workspace can resume this conversation — claude `--resume` reloads into
    /// the context everything that unsupervised run read.
    ///
    /// A bit the whole design calls monotonic, erased by one ignored write error.
    pub(super) fn arm_danger_before_loosening(
        &mut self,
        session_id: &str,
    ) -> Result<Option<u64>, RpcError> {
        // **The rollback restores the old value, not `false`.**
        //
        // On a session that has already been dangerous, forcing it to `false` on
        // a write failure lets this "failed" guard change wash the monotonic bit
        // away — worse than no fix at all.
        let was = self
            .sessions
            .get(session_id)
            .is_some_and(|l| l.info.dangerous);
        if let Some(live) = self.sessions.get_mut(session_id) {
            live.info.dangerous = true;
        }
        // **A missing roster row is not a successful persist.**
        //
        // A new session has no such row until the harness reports its native id
        // (`SessionNote::Bound` is what writes it). `persist_session_state`
        // returns `Ok(())` for "no row" — so "wrote nothing" passes as a
        // successful write, the loosening goes through anyway, and after a
        // restart the roster holds no danger record for this session at all.
        // This path has to confirm that row itself.
        let recorded = self.roster.get(session_id).is_some();
        // **The roster row's old value is kept too.** `persist_session_state`
        // marks the in-memory row dangerous **before** the write; rolling back
        // only `live` on a write failure leaves that true in the roster's
        // in-memory copy, and **any** later successful save then makes a bypass
        // that never took effect permanent — that session is inexplicably
        // owner-only from then on.
        let roster_was = self
            .roster
            .get(session_id)
            .map(|e| (e.ever_dangerous, e.permission_mode));
        let outcome = if recorded {
            self.persist_session_state(session_id)
        } else {
            Err(anyhow::anyhow!(
                "this session has not been recorded yet — its harness has not reported an id"
            ))
        };
        if let Err(e) = outcome {
            // No persist, no loosening: better this guard change fails than a
            // session left "running unsupervised while the ledger says clean".
            // Both in-memory states roll back together, as one transaction.
            if let Some(live) = self.sessions.get_mut(session_id) {
                live.info.dangerous = was;
            }
            if let Some((d, m)) = roster_was
                && let Some(e) = self.roster.sessions.get_mut(session_id)
            {
                e.ever_dangerous = d;
                e.permission_mode = m;
            }
            return Err(RpcError::new(
                ErrorCode::Internal,
                format!("could not record that this session is going unsupervised: {e}"),
            )
            .with_hint(
                "the guard change was refused rather than left unrecorded — try again in a moment, or check that ~/.agit/rc is writable",
            ));
        }

        // The state is durable. **Whether this arming is a new one** has to reach
        // the execution side: only a bit that was "clean before and just flipped
        // to dangerous here" may be flipped back when the instruction is proven
        // never to have run. On a session that was already dangerous that bit is
        // not this guard change's to touch — a monotonic bit means exactly
        // "nobody may wash it away".
        //
        // prepare holds this session's own `rpc_gate`, so two arming instructions
        // for one session are never in flight together; the daemon's global lock
        // covers only this short state update and does not span the queue and
        // receipt waits. The number still lines up **successive** armings: if
        // another arming happens while a withdrawal notification is in flight,
        // the numbers do not match and the bit stays.
        // **Every arming advances the number, even when it flips nothing.**
        //
        // Incrementing only on "newly flipped true" leaves a laundering path: #1
        // flips it true (number 1) and is then withdrawn, its notification in
        // flight; #2 sees it already dangerous, so `armed: None` and **no
        // advance**, and #2 really takes effect. When #1's notification arrives
        // the number is still 1 — it matches, so a session that has just really
        // started running unsupervised is marked clean again. What the number
        // recognizes is "another guard change has happened since", not "whether
        // the bit was flipped".
        let live = self.sessions.get_mut(session_id).expect("just persisted");
        live.danger_arm += 1;
        let arm = live.danger_arm;
        // Only the instruction that flipped it from false to true **this time**
        // may flip it back when it is proven not to have run. On a session that
        // was already dangerous that bit is not its to touch.
        Ok((!roster_was.map(|(d, _)| d).unwrap_or(false)).then_some(arm))
    }
}
