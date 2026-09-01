//! Persistent map: logical session id → how to reach that conversation again.
//!
//! The web addresses a session by its logical `agit-…` id for the rest of its
//! life, but the daemon's in-memory registry dies with the daemon. Without this
//! file a daemon restart makes every session row on the web page dead: a click
//! sends `session.resume(agit-…)` and the daemon, knowing only harness-native
//! ids, answers "no local session under this workspace's folders" — a wrong and
//! misleading error. The roster is the durable half of that mapping.
//!
//! `~/.agit/rc/sessions.json`. Small, append-mostly, rewritten atomically.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Namespace for a daemon hard-stop decision whose executor was still inside
/// a guard-sensitive native operation when the shutdown deadline expired.
///
/// Keep this in the existing `guard_attempts` map: every binary that understands
/// that field already restarts at Plan for an unknown token, while newer
/// binaries can identify the synthetic token and keep it as a monotonic floor
/// until a Plan recovery generation reaches Ready. The hard-stop writer also
/// stores direct `permission_mode = Plan` for still-older readers.
pub(crate) const SHUTDOWN_GUARD_PREFIX: &str = "shutdown/v1/";

pub(crate) fn has_shutdown_guard(guard_attempts: &BTreeMap<String, GuardAttempt>) -> bool {
    guard_attempts
        .keys()
        .any(|token| token.starts_with(SHUTDOWN_GUARD_PREFIX))
}

pub(crate) fn mode_with_shutdown_floor(
    guard_attempts: &BTreeMap<String, GuardAttempt>,
    mode: Option<crate::protocol::PermissionMode>,
) -> Option<crate::protocol::PermissionMode> {
    if has_shutdown_guard(guard_attempts) {
        Some(crate::protocol::PermissionMode::Plan)
    } else {
        mode
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardAttempt {
    pub expected_mode: crate::protocol::PermissionMode,
    #[serde(default)]
    pub observed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub runtime: String,
    /// The harness-native session/thread id (`claude --resume` takes this).
    pub thread_id: String,
    pub cwd: String,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// `owner/name@branch` — where this session settles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agit_session: Option<String>,
    /// Immutable identity paired with `agit_session`. Legacy rows have no ID
    /// and therefore cannot be silently adopted for RC settlement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_agent_id: Option<String>,
    /// The permission mode it was last **actually running** under. Resuming
    /// restores it: a conversation deliberately put in `plan` must not come
    /// back able to write just because the daemon restarted.
    ///
    /// Only ever written from a mode the driver confirmed — a change that is
    /// still queued for the next turn has not happened yet, and persisting it
    /// would make a restart apply a guard change nobody ever saw take effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<crate::protocol::PermissionMode>,
    /// Guard-sensitive turn writes armed durably before entering the session
    /// queue. Any surviving token forces a Plan restart; exact native evidence
    /// removes only its own token, so an old receipt cannot clear a newer
    /// uncertainty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub guard_attempts: BTreeMap<String, GuardAttempt>,
    /// **Every** harness thread id this session has used.
    ///
    /// Claude Code's slow-path recovery swaps in a new session id at
    /// `system/init` (the transcript file changes with it). If `record`
    /// overwrote `thread_id` outright, the old one would be left unclaimed — and
    /// `~/.claude/projects/<slug>/<old id>.jsonl` is still on disk, so
    /// `local_sessions` keeps listing it as "adoptable". Resume that orphan id,
    /// `logical_for_thread` finds nothing, and it **mints a new logical id**
    /// with `ever_dangerous` starting at false — a conversation that once ran
    /// with checks off comes back under a clean identity, drivable by any
    /// operator.
    #[serde(default)]
    pub prior_threads: Vec<String>,
    /// This session has, at some point, run with permission checks off.
    ///
    /// **Monotonic and separate from `permission_mode` on purpose.** Deriving
    /// "is it dangerous" from the current mode loses the history: a session
    /// that ran as `bypass` and was later tightened back to `default` would
    /// come back from a restart looking innocent, and the hub would go back to
    /// letting operators drive it — after it had already been handed an
    /// unsupervised shell. Danger is a property of what a session has done,
    /// not of what it is doing now.
    #[serde(default)]
    pub ever_dangerous: bool,
}

impl Entry {
    pub fn restart_permission_mode(&self) -> Option<crate::protocol::PermissionMode> {
        if self.guard_attempts.is_empty() {
            self.permission_mode
        } else {
            Some(crate::protocol::PermissionMode::Plan)
        }
    }
}

/// Canonical launch inputs covered by one `session.start` idempotency key.
///
/// `by` is retained so the original turn keeps its presentation attribution,
/// but it is not launch identity: an account rename between retries must not
/// turn the same durable UUID into a conflict. Every field compared by
/// [`StartSpec::same_launch_as`] can change what process is launched or what
/// first turn it receives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartSpec {
    pub workspace_id: String,
    pub project_id: String,
    pub runtime: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agit_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    pub permission_mode: crate::protocol::PermissionMode,
}

impl StartSpec {
    pub(crate) fn same_launch_as(&self, other: &Self) -> bool {
        self.workspace_id == other.workspace_id
            && self.project_id == other.project_id
            && self.runtime == other.runtime
            && self.cwd == other.cwd
            && self.agit_session == other.agit_session
            && self.expected_agent_id == other.expected_agent_id
            && self.prompt == other.prompt
            && self.permission_mode == other.permission_mode
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StartState {
    /// Published before the harness launch begins. Seeing this after a crash is
    /// intentionally ambiguous and must never trigger another launch.
    Pending {
        session: crate::protocol::SessionInfo,
    },
    /// Exact response replayed for every duplicate of the same start intent.
    Completed {
        result: crate::protocol::SessionStartResult,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartIntent {
    pub spec: StartSpec,
    pub state: StartState,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StartClaim {
    Reserved,
    Pending(crate::protocol::SessionInfo),
    Completed(crate::protocol::SessionStartResult),
    Conflict,
    HistoryLost,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Roster {
    #[serde(default)]
    pub sessions: BTreeMap<String, Entry>,
    /// This machine's session history **has been lost** (the ledger failed to
    /// parse, or an I/O error that was not "file does not exist").
    ///
    /// An empty ledger and a lost one must be treated differently. Empty means
    /// "this machine has never had a session"; lost means "it has, and I do not
    /// know which" — and what the ledger held is exactly that monotonic
    /// `ever_dangerous` bit. Treating lost as empty washes the safety fact out:
    /// a session that was handed an unapproved shell gets a clean identity the
    /// first time an operator adopts it by native thread id.
    ///
    /// **It goes into the file.** Marking it `#[serde(skip)]` sounds right — "it
    /// describes the circumstances of this load, not the content of the ledger"
    /// — and is a hole: once the corrupt file is moved aside, the next session
    /// registration or permission-mode save writes out a new ledger that **looks
    /// intact**, one restart and the fact is gone, and every old dangerous
    /// session that was not re-recorded can be adopted by an operator by native
    /// thread id again. Having lost history is part of the ledger's content, and
    /// the part a single write must not erase.
    ///
    /// It does not lock this machine into owner-only forever: sessions recorded
    /// **after** the loss carry an accurate bit and are judged normally; only
    /// ids the ledger cannot find (the lost batch) are treated as dangerous.
    #[serde(default)]
    pub history_lost: bool,
    /// A corrupt roster may also have lost idempotency intents. Unlike session
    /// danger history, an unknown key cannot be safely rebuilt: launching it
    /// might duplicate a process that already escaped before the crash.
    #[serde(default)]
    pub start_history_lost: bool,
    #[serde(default)]
    pub starts: BTreeMap<String, StartIntent>,
    /// Logical sessions that **are (or were) running in a dangerous mode while
    /// their current harness thread id has not been confirmed persisted**.
    ///
    /// An empty `thread_id` covers only the "born without an id" half (Codex
    /// waits for a native Ready). The other half is that the id **changes**:
    /// Claude Code's slow-path recovery swaps in a new id at `system/init`, and
    /// the transcript file changes with it. At that moment the ledger still
    /// holds the old id, while the new transcript — carrying the whole context
    /// of this unchecked run — is already on disk. If agitd crashes before
    /// `SessionNote::Bound` persists, or that `save()` itself fails, nothing on
    /// disk points at the new id any more — and one `session.resume` on it
    /// later, `logical_for_thread` finds nothing and mints a clean identity with
    /// `ever_dangerous == false`, drivable by any operator.
    ///
    /// So a dangerous-mode session records itself here **before launch**, and is
    /// struck off only once `Bound` has persisted. While it is there,
    /// `dangerous_start_unaccounted` treats every thread the ledger cannot
    /// recognize on the same territory as dangerous.
    #[serde(default)]
    pub unconfirmed_dangerous_bindings: BTreeSet<String>,
    /// True only while this running daemon has a durable fail-closed snapshot
    /// that still takes precedence on restart.
    ///
    /// This is process-local bookkeeping, not ledger content. While it is set,
    /// every save refreshes the fallback first, then the primary, and only then
    /// durably removes the fallback. That order makes every crash point choose
    /// a current snapshot instead of rolling a later update back.
    #[serde(skip)]
    pub(crate) fail_closed_fallback_active: std::cell::Cell<bool>,
}

const FILE: &str = "sessions.json";
const FAIL_CLOSED_FILE: &str = "sessions.fail-closed.json";

#[cfg(test)]
thread_local! {
    /// Failure injection follows the synchronous/current-thread test that armed
    /// it. An unrelated cargo test may also save a roster, but must not consume
    /// another test's fault or redirect that test's expected write ordering.
    static FAIL_PRIMARY_SAVES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FAIL_FALLBACK_SAVES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FAIL_FALLBACK_REMOVALS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Let this many primary-ledger saves through before failing per
    /// `FAIL_PRIMARY_SAVES`.
    ///
    /// Some holes have exactly this shape: one save succeeds and the very next
    /// one fails. A keyed `session.start` makes its reservation durable, and
    /// then the danger-bit save before `spawn_session` launches fails — the
    /// reservation lives, no process exists. An injection that fails from the
    /// start on can never produce this case, leaving that rollback path to
    /// eyeball review.
    static SKIP_PRIMARY_SAVES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn consume_injected_failure(
    counter: &'static std::thread::LocalKey<std::cell::Cell<usize>>,
) -> bool {
    counter.with(|counter| {
        let left = counter.get();
        if left == 0 {
            false
        } else {
            counter.set(left - 1);
            true
        }
    })
}

#[cfg(test)]
fn consume_injected_primary_failure() -> bool {
    // The skip budget is spent first: **which save** the injected failure lands on is the whole
    // point of this kind of test.
    let skipped = SKIP_PRIMARY_SAVES.with(|counter| {
        let left = counter.get();
        if left == 0 {
            false
        } else {
            counter.set(left - 1);
            true
        }
    });
    !skipped && consume_injected_failure(&FAIL_PRIMARY_SAVES)
}

#[cfg(test)]
pub(crate) fn fail_next_saves(primary: usize, fallback: usize) {
    SKIP_PRIMARY_SAVES.with(|counter| counter.set(0));
    FAIL_PRIMARY_SAVES.with(|counter| counter.set(primary));
    FAIL_FALLBACK_SAVES.with(|counter| counter.set(fallback));
}

/// Fail the **`nth`** primary-ledger save (1 = the next one); every save before it succeeds.
#[cfg(test)]
pub(crate) fn fail_primary_save_number(nth: usize) {
    assert!(nth >= 1, "saves are counted from 1");
    SKIP_PRIMARY_SAVES.with(|counter| counter.set(nth - 1));
    FAIL_PRIMARY_SAVES.with(|counter| counter.set(1));
    FAIL_FALLBACK_SAVES.with(|counter| counter.set(0));
}

#[cfg(test)]
pub(crate) fn pending_injected_saves() -> (usize, usize) {
    (
        FAIL_PRIMARY_SAVES.with(std::cell::Cell::get),
        FAIL_FALLBACK_SAVES.with(std::cell::Cell::get),
    )
}

#[cfg(test)]
pub(crate) fn fail_next_fallback_removals(count: usize) {
    FAIL_FALLBACK_REMOVALS.with(|counter| counter.set(count));
}

impl Roster {
    fn unusable() -> Roster {
        Roster {
            sessions: BTreeMap::new(),
            history_lost: true,
            start_history_lost: true,
            starts: BTreeMap::new(),
            unconfirmed_dangerous_bindings: BTreeSet::new(),
            fail_closed_fallback_active: std::cell::Cell::new(false),
        }
    }

    fn load_primary() -> Roster {
        // The roster is a **ledger**, not a cache — the monotonic danger bit lives nowhere else.
        match super::load_ledger(FILE) {
            super::Ledger::Loaded(r) => r,
            super::Ledger::Missing => Roster::default(),
            super::Ledger::Unusable => Self::unusable(),
        }
    }

    fn read_fail_closed_snapshot() -> crate::Result<Option<Roster>> {
        let path = super::rc_dir()?.join(FAIL_CLOSED_FILE);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                super::sync_rc_dir()?;
                return Ok(None);
            }
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "could not read fail-closed roster snapshot {}: {error}",
                    path.display()
                ));
            }
        };
        serde_json::from_slice(&bytes).map(Some).map_err(|error| {
            anyhow::anyhow!(
                "fail-closed roster snapshot {} is unreadable: {error}",
                path.display()
            )
        })
    }

    /// Load the roster for a daemon launch.
    ///
    /// A fail-closed snapshot is authoritative over the older primary. The
    /// daemon may run only after that snapshot has been atomically promoted and
    /// its directory entry durably removed; otherwise later ordinary saves
    /// could be rolled back by the stale fallback on another restart.
    pub fn try_load() -> crate::Result<Roster> {
        let Some(snapshot) = Self::read_fail_closed_snapshot()? else {
            return Ok(Self::load_primary());
        };
        snapshot.save_primary()?;
        #[cfg(test)]
        if consume_injected_failure(&FAIL_FALLBACK_REMOVALS) {
            anyhow::bail!("injected fail-closed fallback removal failure");
        }
        super::remove_json(FAIL_CLOSED_FILE)?;
        Ok(snapshot)
    }

    pub fn load() -> Roster {
        match Self::try_load() {
            Ok(roster) => roster,
            Err(error) => {
                eprintln!("agitd: refusing unsafe roster recovery: {error:#}");
                Self::unusable()
            }
        }
    }

    /// **Treat as dangerous** when the ledger has no row for it and this machine
    /// has lost history.
    ///
    /// This is the only defensible degradation: refusing to come up costs too
    /// much (the daemon never starts), and treating it as empty washes the
    /// safety fact out. Treating it as dangerous only stops anyone but the owner
    /// from driving it — a recoverable inconvenience in place of an
    /// unrecoverable pass.
    ///
    /// What can be found is judged normally: a session re-recorded after the
    /// loss carries an accurate bit. So one corruption does not degrade this
    /// machine into owner-only forever, while the lost batch — precisely the one
    /// that may hide unapproved history — stays closed.
    ///
    /// **It answers only for "this row", so it does not leave this module.** The
    /// danger bit asks "did this harness transcript run unchecked", and one
    /// transcript can have several rows (when two workspaces each bind the same
    /// folder, each side mints its own logical id). The only question available
    /// from outside is
    /// [`transcript_ever_dangerous`](Self::transcript_ever_dangerous) — expose
    /// this branch and the next call site picks the shorter, looser question,
    /// which has no symptom at all.
    fn ever_dangerous(&self, logical_id: &str) -> bool {
        match self.get(logical_id) {
            Some(e) => e.ever_dangerous,
            None => self.history_lost,
        }
    }

    fn save_primary(&self) -> crate::Result<()> {
        #[cfg(test)]
        if consume_injected_primary_failure() {
            anyhow::bail!("injected primary roster save failure");
        }
        super::save_json(FILE, self)
    }

    fn save_fallback(&self) -> crate::Result<()> {
        #[cfg(test)]
        if consume_injected_failure(&FAIL_FALLBACK_SAVES) {
            anyhow::bail!("injected fail-closed fallback save failure");
        }
        super::save_json(FAIL_CLOSED_FILE, self)
    }

    /// Save while a fallback is authoritative.
    ///
    /// The fallback is refreshed first. If the process crashes after that, it
    /// is already the newest snapshot. The primary is then atomically replaced
    /// and synced before the fallback directory entry is durably deleted.
    fn save_and_promote_active_fallback(&self) -> crate::Result<()> {
        self.save_fallback()?;
        self.save_primary()?;
        #[cfg(test)]
        if consume_injected_failure(&FAIL_FALLBACK_REMOVALS) {
            anyhow::bail!("injected fail-closed fallback removal failure");
        }
        super::remove_json(FAIL_CLOSED_FILE)?;
        self.fail_closed_fallback_active.set(false);
        Ok(())
    }

    pub fn save(&self) -> crate::Result<()> {
        if self.fail_closed_fallback_active.get() {
            self.save_and_promote_active_fallback()
        } else {
            self.save_primary()
        }
    }

    /// Persist a guard-tightening snapshot before cancelling an executor whose
    /// exact permission outcome is unknown.
    ///
    /// The normal roster remains preferred. If its atomic replacement fails,
    /// the complete new roster is written to a separate fsynced fallback. On
    /// restart [`try_load`](Self::try_load) gives that fallback precedence and
    /// refuses to run unless it can promote and durably remove it.
    pub fn save_fail_closed(&self) -> crate::Result<()> {
        if self.fail_closed_fallback_active.get() {
            return self.save_and_promote_active_fallback();
        }
        match self.save_primary() {
            Ok(()) => Ok(()),
            Err(primary) => {
                self.save_fallback()
                    .map_err(|fallback| {
                        anyhow::anyhow!(
                            "primary roster save failed ({primary:#}); fail-closed fallback also failed ({fallback:#})"
                        )
                    })
                    .map(|()| self.fail_closed_fallback_active.set(true))
            }
        }
    }

    /// Record one row. **The old thread id is not lost** — it moves into `prior_threads`.
    pub fn record(&mut self, logical_id: &str, mut entry: Entry) {
        if let Some(old) = self.sessions.get(logical_id) {
            entry.prior_threads = old.prior_threads.clone();
            if old.thread_id != entry.thread_id && !old.thread_id.is_empty() {
                entry.prior_threads.push(old.thread_id.clone());
            }
            // Re-recording never clears the monotonic bit.
            entry.ever_dangerous = entry.ever_dangerous || old.ever_dangerous;
            entry.guard_attempts.extend(old.guard_attempts.clone());
        }
        self.sessions.insert(logical_id.to_string(), entry);
    }

    pub fn get(&self, logical_id: &str) -> Option<&Entry> {
        self.sessions.get(logical_id)
    }

    /// Reserve one launch key in memory. The caller must atomically [`save`]
    /// this new `Pending` row before invoking the harness; on save failure it
    /// removes the reservation with [`forget_start`]. The daemon serializes
    /// calls under its global mutex, so this is also the concurrency gate.
    pub fn claim_start(
        &mut self,
        start_id: &str,
        spec: StartSpec,
        session: crate::protocol::SessionInfo,
    ) -> StartClaim {
        match self.starts.get(start_id) {
            Some(intent) if !intent.spec.same_launch_as(&spec) => StartClaim::Conflict,
            Some(StartIntent {
                state: StartState::Pending { session },
                ..
            }) => StartClaim::Pending(session.clone()),
            Some(StartIntent {
                state: StartState::Completed { result },
                ..
            }) => StartClaim::Completed(result.clone()),
            None if self.start_history_lost => StartClaim::HistoryLost,
            None => {
                self.starts.insert(
                    start_id.to_string(),
                    StartIntent {
                        spec,
                        state: StartState::Pending { session },
                    },
                );
                StartClaim::Reserved
            }
        }
    }

    pub fn complete_start(
        &mut self,
        start_id: &str,
        result: crate::protocol::SessionStartResult,
    ) -> crate::Result<()> {
        let intent = self.starts.get_mut(start_id).ok_or_else(|| {
            anyhow::anyhow!("start intent {start_id} disappeared before completion")
        })?;
        match &intent.state {
            StartState::Pending { session } if session.session_id == result.session.session_id => {
                intent.state = StartState::Completed { result };
                Ok(())
            }
            StartState::Completed { result: old } if old == &result => Ok(()),
            _ => {
                anyhow::bail!("start intent {start_id} completed with a different logical session")
            }
        }
    }

    pub fn forget_start(&mut self, start_id: &str) {
        self.starts.remove(start_id);
    }

    /// The logical id already registered for a harness thread, if any — one
    /// thread must never get a second logical identity, or the web shows two
    /// rows for one conversation.
    /// `workspace_id` is **part of the test, not a filter applied afterwards**.
    ///
    /// Two workspaces can each bind the same folder, so one thread id can have
    /// two rows in the roster. With an outer `.filter()`: this returns the
    /// **first** row in key order, that row may belong to the other workspace,
    /// so it is filtered out and read as "no prior identity" — B has a row of
    /// its own and is minted a new id anyway, washing `ever_dangerous` out with
    /// it. Which is exactly what this code exists to prevent.
    pub fn logical_for_thread(
        &self,
        runtime: &str,
        thread_id: &str,
        workspace_id: &str,
    ) -> Option<String> {
        self.sessions
            .iter()
            .find(|(_, e)| {
                e.runtime == runtime
                    && e.workspace_id == workspace_id
                    && (e.thread_id == thread_id
                        // Accept old ids: after Claude Code rotates its session id, the
                        // old transcript is still on disk and `local_sessions` keeps
                        // listing it as adoptable. Without it, clicking it **mints a new
                        // logical identity** and washes the monotonic danger bit out.
                        || e.prior_threads.iter().any(|t| t == thread_id))
            })
            .map(|(k, _)| k.clone())
    }

    /// Mark a dangerous session as "current thread binding not yet confirmed".
    /// **Must be persisted before the harness launches**; returns whether this
    /// call is what recorded it (the caller rolls back on that).
    pub fn arm_unconfirmed_binding(&mut self, logical_id: &str) -> bool {
        self.unconfirmed_dangerous_bindings
            .insert(logical_id.to_string())
    }

    /// Struck off only **after** the `SessionNote::Bound` `save()` succeeds.
    /// Returns whether it was recorded — on a persist failure the caller puts it
    /// back with this: the poisoned row on disk is still there, and clearing
    /// memory first lets any operator, for as long as this daemon lives, adopt
    /// that dangerous transcript under a clean identity by its new thread id.
    pub fn confirm_binding(&mut self, logical_id: &str) -> bool {
        self.unconfirmed_dangerous_bindings.remove(logical_id)
    }

    /// Whether any row in the ledger records **this harness transcript** as
    /// having run dangerously on this territory — under whichever workspace it
    /// happens to be filed.
    ///
    /// **The monotonic danger bit follows the transcript, not the row.** Looking
    /// up a logical identity
    /// ([`logical_for_thread`](Self::logical_for_thread)) with a workspace is
    /// right: that step asks "what id does this conversation have in this
    /// workspace", and when two workspaces each bind the same folder each side
    /// resolves to its own row. But "has it been dangerous" must not follow that
    /// test: the poisoned row is filed under ws-a while ws-b binds the same
    /// folder, so the transcript sits in ws-b's `local_sessions` listing and is
    /// just as adoptable by thread id. Honour only your own row, and one
    /// adoption by a ws-b operator mints a clean `dangerous: false` identity
    /// that inherits everything the unchecked run read into context;
    /// `SessionNote::Bound` then persists that "clean" as a row of its own, and
    /// from ws-b it is clean forever.
    ///
    /// The test matches
    /// [`dangerous_start_unaccounted`](Self::dangerous_start_unaccounted) (same
    /// workspace **or** same cwd). That one already crosses the workspace
    /// boundary for the case where the ledger **cannot tell** which transcript
    /// it is; when the ledger **can** tell there is even less reason to be
    /// looser — that is the case it is surest about.
    ///
    /// Old thread ids count too (`prior_threads`): after Claude Code rotates its
    /// session id the old transcript is still on disk, `local_sessions` keeps
    /// listing it as adoptable, and it carries the same context.
    fn thread_ever_dangerous_here(
        &self,
        runtime: &str,
        thread_id: &str,
        workspace_id: &str,
        cwd: &str,
    ) -> bool {
        self.sessions.values().any(|e| {
            e.ever_dangerous
                && e.runtime == runtime
                && (e.workspace_id == workspace_id || e.cwd == cwd)
                && (e.thread_id == thread_id || e.prior_threads.iter().any(|t| t == thread_id))
        })
    }

    /// Whether this territory holds a session that **has run dangerously while
    /// its current harness thread id is unknown to the ledger**. If it does,
    /// every thread the ledger cannot recognize must be treated as dangerous.
    ///
    /// Two kinds of "unknown": an empty `thread_id` (a dangerous start that
    /// crashed before the native id was born), and anything hanging in
    /// [`unconfirmed_dangerous_bindings`](Self::unconfirmed_dangerous_bindings)
    /// (an id was born, but the harness may already have rotated it and the new
    /// one never reached disk). Both mean a transcript that "ran unchecked and
    /// the ledger does not know it" is lying on disk: adopt or watch it by
    /// thread id without treating it as dangerous, and the operator mints a
    /// clean new logical identity out of it, inheriting everything the unchecked
    /// run read into context.
    ///
    /// **The territory cannot be drawn by workspace alone.** Two workspaces can
    /// each bind the same folder (the comments on `watch_stream_id` and
    /// `resume_session` are written for this): the poisoned row is filed under
    /// ws-a while ws-b also binds `/srv/app`, and a ws-b operator sees that
    /// orphan transcript in its own listing. So same workspace **or** same cwd
    /// both count — let either side through and the monotonic danger bit is
    /// washed clean on the other.
    fn dangerous_start_unaccounted(&self, runtime: &str, workspace_id: &str, cwd: &str) -> bool {
        self.sessions.iter().any(|(logical_id, e)| {
            e.ever_dangerous
                && (e.thread_id.is_empty()
                    || self.unconfirmed_dangerous_bindings.contains(logical_id))
                && e.runtime == runtime
                && (e.workspace_id == workspace_id || e.cwd == cwd)
        })
    }

    /// **"Did this harness transcript run unchecked" — the ledger's only answer.**
    ///
    /// Each helper above answers one angle, and none of them leaves this module:
    /// picking one from outside means passing a narrower question off as this
    /// one. This hole keeps coming back in one shape — a path asks "has **my
    /// row** been dangerous" while what it hands `--resume` is a transcript that
    /// several rows can point at:
    ///
    /// * `session.watch` gets a harness-native thread id, which never resolves
    ///   against the logical ledger;
    /// * the adoption half of `session.resume` **mints** a logical id on the
    ///   spot for a transcript the ledger does not know, and that id is clean to
    ///   the ledger forever;
    /// * the logical-id half of `session.resume` does find its own row, but when
    ///   two workspaces each bind the same folder that row is only **one** of
    ///   the transcript's identities — ws-a's row poisoned and ws-b's clean is
    ///   the **designed** normal that the workspace test in `logical_for_thread`
    ///   produces.
    ///
    /// So the test hangs off the transcript's coordinates (runtime + thread id +
    /// where it lies), not off a row. The absence of a logical id in the
    /// parameters is deliberate: a caller that cannot produce transcript
    /// coordinates cannot ask this question.
    ///
    /// When the transcript cannot be recognized, **treat it as dangerous**: a
    /// `history_lost` machine is uniformly dangerous (see
    /// [`ever_dangerous`](Self::ever_dangerous)), and while this territory holds
    /// a session that "ran dangerously with a current thread id the ledger does
    /// not know", every unrecognized thread is dangerous too (see
    /// [`dangerous_start_unaccounted`](Self::dangerous_start_unaccounted)).
    ///
    /// [`thread_ever_dangerous_here`](Self::thread_ever_dangerous_here) comes
    /// first and is an `||`: it must not be asked only when the logical-identity
    /// lookup misses — once ws-b has been washed into a clean row (the product
    /// of this very hole), `logical_for_thread` finds that row, ws-a's poisoned
    /// row is never read again, and one whitewash holds forever.
    pub fn transcript_ever_dangerous(
        &self,
        runtime: &str,
        thread_id: &str,
        workspace_id: &str,
        cwd: &str,
    ) -> bool {
        self.thread_ever_dangerous_here(runtime, thread_id, workspace_id, cwd)
            || self
                .logical_for_thread(runtime, thread_id, workspace_id)
                .map(|logical| self.ever_dangerous(&logical))
                .unwrap_or_else(|| {
                    self.ever_dangerous(thread_id)
                        || self.dangerous_start_unaccounted(runtime, workspace_id, cwd)
                })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn failure_injection_is_owned_by_the_arming_test_thread() {
        super::fail_next_saves(2, 3);
        super::fail_next_fallback_removals(1);

        std::thread::spawn(|| {
            assert_eq!(super::pending_injected_saves(), (0, 0));
            assert!(!super::consume_injected_failure(
                &super::FAIL_FALLBACK_REMOVALS
            ));
        })
        .join()
        .unwrap();

        assert_eq!(super::pending_injected_saves(), (2, 3));
        assert!(super::consume_injected_failure(
            &super::FAIL_FALLBACK_REMOVALS
        ));
        assert!(!super::consume_injected_failure(
            &super::FAIL_FALLBACK_REMOVALS
        ));
        super::fail_next_saves(0, 0);
    }

    /// `fail_primary_save_number` must hit **only that one save**.
    ///
    /// It pins the case where one save succeeds and the very next one fails; a
    /// helper that fires early, or fires again, leaves the regression tests
    /// built on it proving something else while still looking green.
    #[test]
    fn the_nth_primary_save_is_the_only_one_that_fails() {
        let home = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(home.path(), || {
            let r = Roster::default();
            super::fail_primary_save_number(2);
            r.save().expect("the first save succeeds normally");
            let error = r.save().expect_err("the second save is the one named");
            assert!(error.to_string().contains("injected primary roster save"));
            r.save().expect("the third save succeeds normally again");
            super::fail_next_saves(0, 0);
        });
    }

    /// When two workspaces each bind the same folder, each resolves to its own
    /// row.
    ///
    /// This pins the test **inside** the lookup: with an outer `.filter()` the
    /// lookup returns the first row in key order (possibly the other
    /// workspace's), and filtering it out reads as "no prior identity" — B has a
    /// row of its own and is minted a new id anyway, washing `ever_dangerous`
    /// out with it. Which is exactly what this code exists to prevent: a
    /// conversation that was handed an unapproved shell coming back under a
    /// clean identity.
    #[test]
    fn two_workspaces_sharing_a_folder_each_resolve_to_their_own_row() {
        let mut r = Roster::default();
        let base = Entry {
            runtime: "claude-code".into(),
            thread_id: "t1".into(),
            cwd: "/srv/app".into(),
            workspace_id: "ws-a".into(),
            project_id: None,
            agit_session: None,
            expected_agent_id: None,
            permission_mode: None,
            guard_attempts: Default::default(),
            ever_dangerous: false,
            prior_threads: vec![],
        };
        // `agit-a` sorts first by key, so without the workspace test it is always found first.
        r.record("agit-a", base.clone());
        r.record(
            "agit-b",
            Entry {
                workspace_id: "ws-b".into(),
                // B's row has been dangerous — exactly the bit that must not be washed out.
                ever_dangerous: true,
                ..base
            },
        );

        assert_eq!(
            r.logical_for_thread("claude-code", "t1", "ws-a").as_deref(),
            Some("agit-a")
        );
        assert_eq!(
            r.logical_for_thread("claude-code", "t1", "ws-b").as_deref(),
            Some("agit-b"),
            "B resolving to A's row (then dropping it) washes B's own danger bit out"
        );
        assert!(r.ever_dangerous("agit-b"));
    }

    /// A dangerous row with an empty `thread_id` (a dangerous start that crashed
    /// before `Bound` persisted) poisons its own runtime, and both its workspace
    /// **and** its folder (a second workspace can bind that same folder); the
    /// moment a real id is bound it stops.
    #[test]
    fn an_unbound_dangerous_start_poisons_its_runtime_workspace_and_directory() {
        let mut r = Roster::default();
        let unbound = Entry {
            runtime: "codex".into(),
            thread_id: String::new(),
            cwd: "/srv/app".into(),
            workspace_id: "ws-a".into(),
            project_id: None,
            agit_session: None,
            expected_agent_id: None,
            permission_mode: None,
            guard_attempts: Default::default(),
            ever_dangerous: true,
            prior_threads: vec![],
        };
        r.record("agit-crashed", unbound.clone());
        // A clean unbound row poisons nothing: only a danger bit in play is worth narrowing
        // adoption to owner-only.
        r.record(
            "agit-clean",
            Entry {
                workspace_id: "ws-b".into(),
                ever_dangerous: false,
                ..unbound.clone()
            },
        );

        assert!(r.dangerous_start_unaccounted("codex", "ws-a", "/other"));
        assert!(!r.dangerous_start_unaccounted("codex", "ws-b", "/other"));
        assert!(!r.dangerous_start_unaccounted("claude-code", "ws-a", "/other"));
        // A second workspace binds the same folder: the orphan transcript is in ws-b's listing,
        // and with the territory drawn by workspace alone one adoption by a ws-b operator washes
        // the monotonic danger bit out.
        assert!(
            r.dangerous_start_unaccounted("codex", "ws-b", "/srv/app"),
            "the other workspace on a shared folder also sees this orphan transcript"
        );

        // Once `Bound` fills in the real id, the session is accounted for by thread id on its
        // own row, and the "unbound dangerous" state disappears.
        r.record(
            "agit-crashed",
            Entry {
                thread_id: "native-9".into(),
                ..unbound
            },
        );
        assert!(!r.dangerous_start_unaccounted("codex", "ws-a", "/srv/app"));
        assert!(r.ever_dangerous("agit-crashed"));
    }

    /// A dangerous transcript that is **fully accounted for** (real thread id,
    /// binding confirmed) is still dangerous from a second workspace bound to
    /// the same folder.
    ///
    /// Nothing else covers this case: `dangerous_start_unaccounted` matches only
    /// rows with an empty `thread_id` or an unconfirmed binding, and the
    /// logical-identity lookup carries a workspace test. So once a ws-a bypass
    /// session has persisted `Bound`, ws-b cannot tell that it was dangerous —
    /// yet the transcript lies in a folder ws-b binds too, and ws-b's listing
    /// shows it. One adoption by a ws-b operator by thread id mints a clean
    /// `dangerous: false` identity that inherits everything the unchecked run
    /// read into context; `Bound` then persists that "clean" as a row of its own
    /// and the whitewash holds forever.
    ///
    /// When the ledger **can** tell which transcript it is, it must not be
    /// looser than when it cannot.
    #[test]
    fn a_confirmed_dangerous_thread_is_still_dangerous_from_a_second_workspace_on_its_cwd() {
        let mut r = Roster::default();
        r.record(
            "agit-a",
            Entry {
                runtime: "claude-code".into(),
                thread_id: "t1".into(),
                cwd: "/srv/app".into(),
                workspace_id: "ws-a".into(),
                project_id: None,
                agit_session: None,
                expected_agent_id: None,
                permission_mode: Some(crate::protocol::PermissionMode::Bypass),
                guard_attempts: Default::default(),
                ever_dangerous: true,
                // Claude Code rotated the session id: the old transcript is still on
                // disk, carrying the same context.
                prior_threads: vec!["t0".into()],
            },
        );
        // precondition: this row is neither kind of "unknown", so the unbound test cannot
        // match it.
        assert!(r.unconfirmed_dangerous_bindings.is_empty());
        assert!(!r.dangerous_start_unaccounted("claude-code", "ws-b", "/srv/app"));

        assert!(r.thread_ever_dangerous_here("claude-code", "t1", "ws-a", "/srv/app"));
        assert!(
            r.thread_ever_dangerous_here("claude-code", "t1", "ws-b", "/srv/app"),
            "ws-b binds this folder too, so the dangerous transcript is adoptable from its listing"
        );
        assert!(
            r.thread_ever_dangerous_here("claude-code", "t0", "ws-b", "/srv/app"),
            "a rotated-away thread id points at the same context"
        );

        // Outside the territory it is someone else's tenancy: another folder, another runtime,
        // another thread is not implicated, or this test would lock unrelated adoptions into
        // owner-only too.
        assert!(!r.thread_ever_dangerous_here("claude-code", "t1", "ws-b", "/srv/other"));
        assert!(!r.thread_ever_dangerous_here("codex", "t1", "ws-b", "/srv/app"));
        assert!(!r.thread_ever_dangerous_here("claude-code", "t9", "ws-a", "/srv/app"));
    }

    /// The half where the harness **has rotated** the thread id: the ledger's id
    /// is non-empty but is no longer the name of the transcript on disk. Until
    /// the binding is confirmed it must poison this territory just the same.
    #[test]
    fn an_unconfirmed_binding_poisons_until_bound_is_durable() {
        let mut r = Roster::default();
        r.record(
            "agit-x",
            Entry {
                runtime: "claude-code".into(),
                // Non-empty: a genuine id, the one in use until slow-path recovery.
                thread_id: "t-old".into(),
                cwd: "/srv/app".into(),
                workspace_id: "ws-a".into(),
                project_id: None,
                agit_session: None,
                expected_agent_id: None,
                permission_mode: None,
                guard_attempts: Default::default(),
                ever_dangerous: true,
                prior_threads: vec![],
            },
        );
        assert!(
            !r.dangerous_start_unaccounted("claude-code", "ws-a", "/srv/app"),
            "a confirmed binding poisons no one: the ledger knows every id it has"
        );

        assert!(r.arm_unconfirmed_binding("agit-x"));
        assert!(
            !r.arm_unconfirmed_binding("agit-x"),
            "re-arming is not a new record"
        );
        assert!(
            r.dangerous_start_unaccounted("claude-code", "ws-a", "/srv/app"),
            "t-old may already be rotated away and the ledger does not know the new one"
        );
        // The old id itself is judged normally: it is right there on this row.
        assert!(r.ever_dangerous("agit-x"));

        assert!(r.confirm_binding("agit-x"));
        assert!(!r.confirm_binding("agit-x"));
        assert!(!r.dangerous_start_unaccounted("claude-code", "ws-a", "/srv/app"));
    }

    /// The poison bit goes **into the file**. It describes exactly that this
    /// machine crashed, and a restart that cannot read it back is the same as it
    /// never having happened — and a restart is the only moment this hole is
    /// exploitable.
    #[test]
    fn an_unconfirmed_binding_survives_a_reload() {
        let mut r = Roster::default();
        r.record(
            "agit-x",
            Entry {
                runtime: "claude-code".into(),
                thread_id: "t-old".into(),
                cwd: "/srv/app".into(),
                workspace_id: "ws-a".into(),
                project_id: None,
                agit_session: None,
                expected_agent_id: None,
                permission_mode: None,
                guard_attempts: Default::default(),
                ever_dangerous: true,
                prior_threads: vec![],
            },
        );
        r.arm_unconfirmed_binding("agit-x");
        let reloaded: Roster = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert!(reloaded.dangerous_start_unaccounted("claude-code", "ws-a", "/srv/app"));
    }

    /// Once the ledger has been corrupt, **a restart must not forget that fact**.
    ///
    /// If the bit does not go into the file: the corrupt file is moved aside,
    /// the next registration writes out a new ledger that looks intact, one
    /// restart and "history was lost" is gone, and every old dangerous session
    /// that was not re-recorded can be adopted by an operator by native thread
    /// id again — a conversation that was handed an unapproved shell comes back
    /// under a clean identity.
    #[test]
    fn a_lost_history_survives_the_next_write_and_the_next_start() {
        let home = tempfile::tempdir().expect("temp AGIT_HOME");
        super::super::with_agit_home(home.path(), || {
            let path = super::super::rc_dir().unwrap().join(FILE);
            std::fs::write(&path, "{ this roster was torn").expect("write corrupt roster");

            // Exercise the production loader: an unusable ledger, unlike a missing one, must
            // create the monotonic history-loss evidence.
            let mut after = Roster::load();
            assert!(after.history_lost);
            assert!(after.start_history_lost);
            assert!(path.with_extension("json.corrupt").exists());

            // Sessions recorded after the loss are judged normally — one corruption must not
            // degrade this machine forever.
            after.record(
                "s-new",
                Entry {
                    runtime: "claude-code".into(),
                    thread_id: "t-new".into(),
                    cwd: "/tmp".into(),
                    workspace_id: "ws-1".into(),
                    project_id: None,
                    agit_session: None,
                    expected_agent_id: None,
                    permission_mode: None,
                    guard_attempts: Default::default(),
                    prior_threads: vec![],
                    ever_dangerous: false,
                },
            );
            assert!(!after.ever_dangerous("s-new"));
            // And the ids the ledger cannot find (the lost batch) stay closed.
            assert!(after.ever_dangerous("s-from-before-the-loss"));

            // Model the next launch through the production save and load; one legitimate write
            // must not wash the loss evidence out.
            after.save().expect("save recovered roster");
            let back = Roster::load();
            assert!(
                back.history_lost,
                "a write must not clear lost history; old dangerous sessions would be adoptable"
            );
            assert!(back.start_history_lost);
            assert!(back.ever_dangerous("s-from-before-the-loss"));
            assert!(!back.ever_dangerous("s-new"));
        });
    }

    #[test]
    fn fail_closed_recovery_must_promote_and_durably_remove_before_launch() {
        let home = tempfile::tempdir().expect("temp AGIT_HOME");
        super::super::with_agit_home(home.path(), || {
            let fallback = super::super::rc_dir().unwrap().join(FAIL_CLOSED_FILE);
            let snapshot = Roster {
                history_lost: true,
                ..Roster::default()
            };

            fail_next_saves(1, 0);
            snapshot
                .save_fail_closed()
                .expect("a primary failure falls back durably");
            assert!(fallback.exists());

            fail_next_saves(1, 0);
            assert!(
                Roster::try_load().is_err(),
                "a daemon must not run when fallback promotion fails"
            );
            assert!(
                fallback.exists(),
                "failed promotion must preserve the authoritative snapshot"
            );

            fail_next_fallback_removals(1);
            assert!(
                Roster::try_load().is_err(),
                "a daemon must not run while the fallback entry may survive"
            );
            assert!(fallback.exists(), "failed durable deletion is retriable");

            let recovered = Roster::try_load().expect("a later launch can finish recovery");
            assert!(recovered.history_lost);
            assert!(
                !fallback.exists(),
                "the daemon may run only after the fallback is durably gone"
            );
        });
    }

    #[test]
    fn an_active_fallback_is_refreshed_before_any_later_primary_save() {
        let home = tempfile::tempdir().expect("temp AGIT_HOME");
        super::super::with_agit_home(home.path(), || {
            let mut roster = Roster::default();
            fail_next_saves(1, 0);
            roster
                .save_fail_closed()
                .expect("create authoritative fallback");
            assert!(roster.fail_closed_fallback_active.get());

            // Model a later projection while shutdown is draining. Promotion
            // still fails, but the authoritative file must advance first.
            roster.history_lost = true;
            fail_next_saves(1, 0);
            assert!(roster.save().is_err());
            let on_disk = Roster::read_fail_closed_snapshot()
                .expect("fallback remains readable")
                .expect("fallback remains authoritative");
            assert!(
                on_disk.history_lost,
                "a stale fallback would roll this later update back on restart"
            );

            let recovered = Roster::try_load().expect("next launch promotes the newest fallback");
            assert!(recovered.history_lost);
        });
    }

    #[test]
    fn corrupt_or_unreadable_fail_closed_snapshot_refuses_launch() {
        let home = tempfile::tempdir().expect("temp AGIT_HOME");
        super::super::with_agit_home(home.path(), || {
            let fallback = super::super::rc_dir().unwrap().join(FAIL_CLOSED_FILE);
            std::fs::write(&fallback, b"{ torn fail-closed roster")
                .expect("write corrupt fallback");
            assert!(Roster::try_load().is_err());
            assert!(
                fallback.exists(),
                "corrupt fail-closed evidence must not be renamed away"
            );

            std::fs::remove_file(&fallback).unwrap();
            std::fs::create_dir(&fallback).expect("make the fallback path unreadable as a file");
            assert!(
                Roster::try_load().is_err(),
                "a non-readable fallback shape must also fail startup"
            );
        });
    }
    use super::*;

    #[test]
    fn a_thread_maps_back_to_its_logical_id() {
        let mut r = Roster::default();
        r.record(
            "agit-abc",
            Entry {
                runtime: "claude-code".into(),
                thread_id: "0192-uuid".into(),
                cwd: "/w".into(),
                workspace_id: "ws1".into(),
                project_id: None,
                agit_session: Some("alice/payments@s-1".into()),
                expected_agent_id: Some("00000000-0000-0000-0000-000000000001".into()),
                permission_mode: None,
                guard_attempts: Default::default(),
                ever_dangerous: false,
                prior_threads: vec![],
            },
        );
        assert_eq!(
            r.logical_for_thread("claude-code", "0192-uuid", "ws1")
                .as_deref(),
            Some("agit-abc")
        );
        assert!(r.logical_for_thread("codex", "0192-uuid", "ws1").is_none());
        assert_eq!(r.get("agit-abc").unwrap().cwd, "/w");
    }

    #[test]
    fn the_same_thread_can_gain_its_first_immutable_lineage() {
        let mut r = Roster::default();
        let unclaimed = Entry {
            runtime: "codex".into(),
            thread_id: "thread-1".into(),
            cwd: "/w".into(),
            workspace_id: "ws-1".into(),
            project_id: Some("project-1".into()),
            agit_session: None,
            expected_agent_id: None,
            permission_mode: None,
            guard_attempts: Default::default(),
            ever_dangerous: false,
            prior_threads: vec![],
        };
        r.record("agit-1", unclaimed.clone());
        r.record(
            "agit-1",
            Entry {
                agit_session: Some("alice/payments@s/1".into()),
                expected_agent_id: Some("00000000-0000-0000-0000-000000000001".into()),
                ..unclaimed
            },
        );

        let saved = r.get("agit-1").unwrap();
        assert_eq!(saved.thread_id, "thread-1");
        assert_eq!(saved.agit_session.as_deref(), Some("alice/payments@s/1"));
        assert_eq!(
            saved.expected_agent_id.as_deref(),
            Some("00000000-0000-0000-0000-000000000001")
        );
    }

    /// Claude Code's slow-path recovery swaps in a new session id while the old
    /// transcript is still on disk — `local_sessions` keeps listing it as
    /// "adoptable".
    ///
    /// Without accepting the old id, clicking it finds nothing in
    /// `logical_for_thread`, so it **mints a new logical identity** with
    /// `ever_dangerous` starting at false: a conversation that once ran with
    /// checks off comes back under a clean identity, drivable by any operator.
    #[test]
    fn an_old_thread_id_still_resolves_to_the_same_conversation_after_a_rotation() {
        let mut r = Roster::default();
        let base = Entry {
            runtime: "claude-code".into(),
            thread_id: "t1".into(),
            cwd: "/srv/app".into(),
            workspace_id: "ws-a".into(),
            project_id: None,
            agit_session: None,
            expected_agent_id: None,
            permission_mode: None,
            guard_attempts: Default::default(),
            ever_dangerous: true,
            prior_threads: vec![],
        };
        r.record("agit-X", base.clone());
        // Claude Code rotated the id.
        r.record(
            "agit-X",
            Entry {
                thread_id: "t2".into(),
                // On a re-record the caller passes the value as of now, which may
                // not carry the monotonic bit.
                ever_dangerous: false,
                ..base
            },
        );

        assert_eq!(
            r.logical_for_thread("claude-code", "t2", "ws-a").as_deref(),
            Some("agit-X")
        );
        assert_eq!(
            r.logical_for_thread("claude-code", "t1", "ws-a").as_deref(),
            Some("agit-X"),
            "an old id must still resolve to the same conversation, not a clean new identity"
        );
        assert!(
            r.get("agit-X").unwrap().ever_dangerous,
            "a re-record must not clear the monotonic bit"
        );
    }

    fn start_info() -> crate::protocol::SessionInfo {
        crate::protocol::SessionInfo {
            session_id: "agit-started".into(),
            workspace_id: "ws-1".into(),
            project_id: Some("project-1".into()),
            runtime: "codex".into(),
            agent: Some("alice/payments".into()),
            branch: Some("s/one".into()),
            status: crate::protocol::SessionStatus::Idle,
            last_seq: 0,
            gist: None,
            dangerous: false,
            permission_mode: Some(crate::protocol::PermissionMode::Default),
            created_at: "2026-08-22T00:00:00Z".into(),
            updated_at: "2026-08-22T00:00:00Z".into(),
        }
    }

    fn start_spec() -> StartSpec {
        StartSpec {
            workspace_id: "ws-1".into(),
            project_id: "project-1".into(),
            runtime: "codex".into(),
            cwd: "/srv/payments".into(),
            agit_session: Some("alice/payments@s/one".into()),
            expected_agent_id: Some("00000000-0000-0000-0000-000000000001".into()),
            prompt: Some("fix it".into()),
            by: Some("alice".into()),
            permission_mode: crate::protocol::PermissionMode::Default,
        }
    }

    #[test]
    fn a_start_key_reserves_once_and_replays_the_exact_persisted_result() {
        let id = "018f47cb-60ff-7e31-aec9-02d2e39d3114";
        let mut roster = Roster::default();
        assert_eq!(
            roster.claim_start(id, start_spec(), start_info()),
            StartClaim::Reserved
        );
        let mut duplicate_candidate = start_info();
        duplicate_candidate.session_id = "agit-must-not-launch".into();
        assert_eq!(
            roster.claim_start(id, start_spec(), duplicate_candidate.clone()),
            StartClaim::Pending(start_info()),
            "a concurrent duplicate must not reserve a second launch"
        );

        let mut changed = start_spec();
        changed.prompt = Some("different launch".into());
        assert_eq!(
            roster.claim_start(id, changed, start_info()),
            StartClaim::Conflict
        );

        let home = tempfile::tempdir().unwrap();
        let mut restarted = crate::rc::with_agit_home(home.path(), || {
            roster.save().unwrap();
            Roster::load()
        });
        assert_eq!(
            restarted.claim_start(id, start_spec(), duplicate_candidate.clone()),
            StartClaim::Pending(start_info()),
            "a pre-launch intent survives a daemon restart and forbids relaunch"
        );

        let result = crate::protocol::SessionStartResult {
            start_id: Some(id.into()),
            session: start_info(),
        };
        restarted.complete_start(id, result.clone()).unwrap();
        assert_eq!(
            restarted.claim_start(id, start_spec(), duplicate_candidate.clone()),
            StartClaim::Completed(result.clone())
        );

        let mut completed = crate::rc::with_agit_home(home.path(), || {
            restarted.save().unwrap();
            Roster::load()
        });
        assert_eq!(
            completed.claim_start(id, start_spec(), duplicate_candidate),
            StartClaim::Completed(result),
            "restart must replay the same logical session instead of launching"
        );
    }

    #[test]
    fn lost_start_history_refuses_every_unknown_key() {
        let mut roster = Roster {
            start_history_lost: true,
            ..Roster::default()
        };
        assert_eq!(
            roster.claim_start(
                "018f47cb-60ff-7e31-aec9-02d2e39d3114",
                start_spec(),
                start_info()
            ),
            StartClaim::HistoryLost
        );
        assert!(roster.starts.is_empty());
    }

    #[test]
    fn every_launch_behavior_dimension_is_immutable_for_one_key() {
        let id = "018f47cb-60ff-7e31-aec9-02d2e39d3114";
        let original = start_spec();
        let mut roster = Roster::default();
        assert_eq!(
            roster.claim_start(id, original.clone(), start_info()),
            StartClaim::Reserved
        );

        let mut changes = Vec::new();
        let mut changed = original.clone();
        changed.workspace_id = "ws-2".into();
        changes.push(("workspace_id", changed));
        let mut changed = original.clone();
        changed.project_id = "project-2".into();
        changes.push(("project_id", changed));
        let mut changed = original.clone();
        changed.runtime = "claude-code".into();
        changes.push(("runtime", changed));
        let mut changed = original.clone();
        changed.cwd = "/srv/other".into();
        changes.push(("cwd", changed));
        let mut changed = original.clone();
        changed.agit_session = Some("alice/payments@s/two".into());
        changes.push(("lineage", changed));
        let mut changed = original.clone();
        changed.expected_agent_id = Some("00000000-0000-0000-0000-000000000002".into());
        changes.push(("expected_agent_id", changed));
        let mut changed = original.clone();
        changed.prompt = Some("different prompt".into());
        changes.push(("prompt", changed));
        let mut changed = original;
        changed.permission_mode = crate::protocol::PermissionMode::Bypass;
        changes.push(("permission_mode", changed));

        for (field, changed) in changes {
            assert_eq!(
                roster.claim_start(id, changed, start_info()),
                StartClaim::Conflict,
                "reusing the key changed {field}"
            );
        }
    }

    #[test]
    fn display_attribution_does_not_change_a_start_keys_launch_identity() {
        let id = "018f47cb-60ff-7e31-aec9-02d2e39d3114";
        let original = start_spec();
        let mut roster = Roster::default();
        assert_eq!(
            roster.claim_start(id, original.clone(), start_info()),
            StartClaim::Reserved
        );

        let mut renamed = original;
        renamed.by = Some("alice-renamed".into());
        assert_eq!(
            roster.claim_start(id, renamed, start_info()),
            StartClaim::Pending(start_info()),
            "a display-name change must replay the original launch instead of conflicting"
        );
    }

    #[tokio::test]
    async fn concurrent_duplicates_have_exactly_one_reservation_winner() {
        let id = "018f47cb-60ff-7e31-aec9-02d2e39d3114";
        let roster = std::sync::Arc::new(tokio::sync::Mutex::new(Roster::default()));
        let mut tasks = vec![];
        for _ in 0..16 {
            let roster = roster.clone();
            tasks.push(tokio::spawn(async move {
                roster
                    .lock()
                    .await
                    .claim_start(id, start_spec(), start_info())
            }));
        }
        let mut reserved = 0;
        let mut pending = 0;
        for task in tasks {
            match task.await.unwrap() {
                StartClaim::Reserved => reserved += 1,
                StartClaim::Pending(_) => pending += 1,
                other => panic!("unexpected concurrent claim: {other:?}"),
            }
        }
        assert_eq!(reserved, 1, "more than one caller could reach launch");
        assert_eq!(pending, 15);
    }

    #[test]
    fn every_unresolved_turn_guard_forces_plan_until_the_last_token_is_removed() {
        let mut entry = Entry {
            runtime: "codex".into(),
            thread_id: "thread-1".into(),
            cwd: "/tmp/project".into(),
            workspace_id: "ws-1".into(),
            project_id: None,
            agit_session: None,
            expected_agent_id: None,
            permission_mode: Some(crate::protocol::PermissionMode::Bypass),
            guard_attempts: BTreeMap::from([
                (
                    "old".into(),
                    GuardAttempt {
                        expected_mode: crate::protocol::PermissionMode::Bypass,
                        observed: true,
                    },
                ),
                (
                    "new".into(),
                    GuardAttempt {
                        expected_mode: crate::protocol::PermissionMode::Auto,
                        observed: false,
                    },
                ),
            ]),
            prior_threads: vec![],
            ever_dangerous: true,
        };

        assert_eq!(
            entry.restart_permission_mode(),
            Some(crate::protocol::PermissionMode::Plan)
        );
        entry.guard_attempts.remove("old");
        assert_eq!(
            entry.restart_permission_mode(),
            Some(crate::protocol::PermissionMode::Plan),
            "an older confirmation cannot clear a newer uncertainty"
        );
        entry.guard_attempts.remove("new");
        assert_eq!(
            entry.restart_permission_mode(),
            Some(crate::protocol::PermissionMode::Bypass)
        );
    }

    #[test]
    fn shutdown_guard_round_trip_is_a_named_live_mode_floor() {
        let token = format!("{}stable-token", SHUTDOWN_GUARD_PREFIX);
        let mut entry = Entry {
            runtime: "codex".into(),
            thread_id: "thread-1".into(),
            cwd: "/tmp/project".into(),
            workspace_id: "ws-1".into(),
            project_id: None,
            agit_session: None,
            expected_agent_id: None,
            permission_mode: Some(crate::protocol::PermissionMode::Bypass),
            guard_attempts: BTreeMap::from([(
                token.clone(),
                GuardAttempt {
                    expected_mode: crate::protocol::PermissionMode::Plan,
                    observed: false,
                },
            )]),
            prior_threads: vec![],
            ever_dangerous: true,
        };

        let encoded = serde_json::to_vec(&entry).unwrap();
        let decoded: Entry = serde_json::from_slice(&encoded).unwrap();
        assert!(has_shutdown_guard(&decoded.guard_attempts));
        assert_eq!(
            mode_with_shutdown_floor(
                &decoded.guard_attempts,
                Some(crate::protocol::PermissionMode::Bypass)
            ),
            Some(crate::protocol::PermissionMode::Plan)
        );
        assert_eq!(
            decoded.restart_permission_mode(),
            Some(crate::protocol::PermissionMode::Plan),
            "the existing guard-aware restart path fails closed for every token"
        );

        entry.guard_attempts.remove(&token);
        assert!(!has_shutdown_guard(&entry.guard_attempts));
        assert_eq!(
            mode_with_shutdown_floor(&entry.guard_attempts, entry.permission_mode),
            Some(crate::protocol::PermissionMode::Bypass)
        );
    }
}
