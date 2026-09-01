//! The **only** test for the monotonic danger bit, and the slip that carries it around the
//! daemon.
//!
//! # Why this is a module and not a helper
//!
//! The rule itself is short: **before handing a stretch of harness transcript to anyone, ask
//! whether that transcript ever ran unchecked; the bit that comes back is written into this
//! session, and every `turn.start` / `turn.steer` / `approval.decide` behind `Need::Drive`
//! reads it.**
//!
//! Short, and easy to get wrong on every path — move the test into a helper and the next path
//! that forgets to call it is a miss. Three ways to miss it:
//!
//! * `session.watch` looks a harness-native thread id up in the logical ledger and never finds
//!   anything (a miss);
//! * the takeover half of `session.resume` **mints** a logical id on the spot for a transcript
//!   the ledger does not know, and that id is clean to the ledger forever (a miss);
//! * the logical-id half of `session.resume` does find its own row — but when the same
//!   directory is bound once by each of two workspaces, that row is only **one of** this
//!   transcript's identities, and ws-a's row poisoned while ws-b's row is clean is the
//!   **designed** normal (a miss).
//!
//! What the three share is that "forgetting to call that helper" turns nothing red: the path
//! that was missed runs as usual; it just quietly hands a conversation that ran unchecked to the
//! operator. So this holds not one more helper but **a slip nothing proceeds without**:
//!
//! * [`TranscriptDanger`]'s fields are private, and this module is a **sibling** submodule of
//!   `daemon` (`sessions` / `dispatch` / `guard` cannot reach private fields), so only the three
//!   constructors in this module produce one;
//! * [`Daemon::spawn_session`](super::Daemon::spawn_session) refuses to launch without one and
//!   stamps `SessionInfo.dangerous` **itself** — the judged bit no longer depends on the caller
//!   remembering to copy it across;
//! * the other two constructors cannot launch a session:
//!   [`TranscriptDanger::fresh_transcript`] ("this run resumes no transcript") and [`judge`]
//!   (read-only following; it judges no caller). `spawn_session` confronts the slip with
//!   `spec.resume_from` on the spot, and either of those two together with `--resume` is
//!   refused there rather than quietly let through.
//!
//! So "add a new resume path" either asks the transcript or does not compile / does not launch.

use super::{RpcError, require_owner_to_drive};
use crate::rc::roster::Roster;

/// The danger verdict on one transcript — **only this module produces one**.
///
/// Holding one means "this path has already asked the ledger". It cannot be forged elsewhere
/// (the fields are private), so every path that hands a transcript out has to really ask once.
#[derive(Debug, Clone, Copy)]
#[must_use = "this bit goes to spawn_session: it is what that owner-only gate rests on, and the fact written into SessionInfo"]
pub(super) struct TranscriptDanger {
    /// `Some(monotonic bit)` = some transcript was judged; `None` = this launch resumes no
    /// transcript.
    judged: Option<bool>,
    /// This verdict **judged the caller too** ([`authorize`]).
    ///
    /// Read-only following goes through [`judge`], which judges no caller — that slip cannot
    /// launch a session. Without this field, the next resume path only has to pick `judge`
    /// (shorter signature, no caller to pass) to get around the owner-only gate, and that is
    /// exactly the shape of this hole: the step that is missed turns nothing red.
    authorized: bool,
}

impl TranscriptDanger {
    /// This session runs **without continuing any existing transcript** (`session.start`:
    /// `resume_from == None`).
    ///
    /// Only here is there no history to judge — a freshly started conversation context is empty
    /// (the gate on the starting mode is `require_owner_to_loosen`, in `start_session`). Using
    /// this slip together with `--resume` is refused on the spot by `spawn_session`.
    pub(super) fn fresh_transcript() -> Self {
        Self {
            judged: None,
            authorized: false,
        }
    }

    /// Whether this session ever ran unchecked.
    pub(super) fn ever_dangerous(self) -> bool {
        self.judged.unwrap_or(false)
    }

    /// Whether this slip really asked about a transcript and judged whether this caller may
    /// drive it. `spawn_session` confronts it with `spec.resume_from`.
    pub(super) fn cleared_a_transcript(self) -> bool {
        self.judged.is_some() && self.authorized
    }
}

/// **Ask** only, judge no caller: `session.watch` is read-only following and open to anyone,
/// but the warning marker in the web interface says "what this session **has done**", and
/// hardcoding false hides it exactly where it most needs to show.
///
/// **The slip it produces cannot launch a session** (`spawn_session` takes only [`authorize`]'s):
/// handing a transcript into a session that can be driven is another matter, and that step
/// judges the caller too.
pub(super) fn judge(
    roster: &Roster,
    runtime: &str,
    thread_id: &str,
    workspace_id: &str,
    cwd: &str,
) -> TranscriptDanger {
    TranscriptDanger {
        judged: Some(roster.transcript_ever_dangerous(runtime, thread_id, workspace_id, cwd)),
        authorized: false,
    }
}

/// Ask + judge the caller: whether handing a transcript **to this caller** is allowed.
///
/// Resuming a session that once ran with no approvals is owner-only — its context may still
/// hold what it read unsupervised then, and `--resume` loads all of it back into the model.
///
/// The two (looking history up by transcript + the owner test) are tied into one function so
/// that one test can pin both: split apart, quietly switching the call site back to "ask only
/// about my own row" turns no test red, and that step is the one that hands unchecked context
/// to the operator.
pub(super) fn authorize(
    roster: &Roster,
    caller: &crate::protocol::CallerClaim,
    runtime: &str,
    thread_id: &str,
    workspace_id: &str,
    cwd: &str,
) -> Result<TranscriptDanger, RpcError> {
    let danger = judge(roster, runtime, thread_id, workspace_id, cwd);
    require_owner_to_drive(Some(caller), danger.ever_dangerous())?;
    Ok(TranscriptDanger {
        authorized: true,
        ..danger
    })
}

/// Stamp the judged bit into this session.
///
/// **Monotonic**: an `info.dangerous` that is already true (a session freshly started with
/// bypass, say) is never washed out by the verdict.
///
/// `spawn_session` calls this in one place instead of letting every path copy the bit into
/// `SessionInfo` itself: a path that misses the copy reports a session that ran unchecked as
/// clean, and then the `Need::Drive` gate in `session_channel` counts for nothing against it —
/// the gate and the marker must come from the same verdict.
pub(super) fn stamp(info: &mut crate::protocol::SessionInfo, danger: TranscriptDanger) {
    info.dangerous = info.dangerous || danger.ever_dangerous();
}
