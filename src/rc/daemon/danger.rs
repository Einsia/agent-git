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
//! * [`Daemon::prepare_spawn`](super::Daemon::prepare_spawn) refuses to launch without one and
//!   stamps `SessionInfo.dangerous` **itself** — the judged bit no longer depends on the caller
//!   remembering to copy it across;
//! * the other two constructors cannot launch a session:
//!   [`TranscriptDanger::fresh_transcript`] ("this run resumes no transcript") and [`judge`]
//!   (read-only following; it judges no caller). `prepare_spawn` confronts the slip with
//!   `spec.resume_from` on the spot, and either of those two together with `--resume` is
//!   refused there rather than quietly let through.
//!
//! So "add a new resume path" either asks the transcript or does not compile / does not launch.

use crate::rc::roster::Roster;
use std::path::Path;

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
    /// (the gate on a mode the start names is `require_owner_to_loosen`, in `prepare_start_session`,
    /// judged from the machine's own default). Using
    /// this slip together with `--resume` is refused on the spot by `prepare_spawn`.
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
    /// drive it. `prepare_spawn` confronts it with `spec.resume_from`.
    pub(super) fn cleared_a_transcript(self) -> bool {
        self.judged.is_some() && self.authorized
    }
}

/// **Ask** only, judge no caller: `session.watch` is read-only following and open to anyone,
/// but the warning marker in the web interface says "what this session **has done**", and
/// hardcoding false hides it exactly where it most needs to show.
///
/// **The slip it produces cannot launch a session** (`prepare_spawn` takes only [`authorize`]'s):
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
        judged: Some(
            roster.transcript_ever_dangerous(runtime, thread_id, workspace_id, cwd)
                || transcript_records_unchecked(runtime, thread_id, cwd),
        ),
        authorized: false,
    }
}

/// Whether the harness transcript itself says it ran without permission checks, whatever the
/// ledger knows. A conversation run in a terminal with checks off (Claude Code's
/// `--dangerously-skip-permissions`, or Codex outside a workspace-write or read-only sandbox) and closed
/// before the daemon saw it has no row that says so, and its takeover or share would otherwise
/// mint a clean identity. Its own records do say so. A Claude Code or Codex transcript that
/// cannot be found or read, or that records no mode, counts as unchecked: nothing shows it was
/// supervised. A runtime that records no mode at all is judged by the ledger alone.
fn transcript_records_unchecked(runtime: &str, thread_id: &str, cwd: &str) -> bool {
    let read: fn(&Path) -> crate::Result<bool> = match runtime {
        "claude-code" => crate::rc::claude_inbox::transcript_ran_unchecked,
        "codex" => crate::rc::native_settings::codex_ran_unchecked,
        _ => return false,
    };
    if thread_id.is_empty() {
        return false;
    }
    let Some(transcript) = crate::adapter::get(runtime)
        .ok()
        .and_then(|adapter| adapter.resolve(thread_id, Some(Path::new(cwd))))
    else {
        return true;
    };
    read(&transcript).unwrap_or(true)
}

/// Ask, for a launch that hands this transcript to a session: the verdict carries the monotonic
/// bit into the session it starts, whoever asked for it.
pub(super) fn authorize(
    roster: &Roster,
    runtime: &str,
    thread_id: &str,
    workspace_id: &str,
    cwd: &str,
) -> TranscriptDanger {
    TranscriptDanger {
        authorized: true,
        ..judge(roster, runtime, thread_id, workspace_id, cwd)
    }
}

/// [`authorize`] for a Codex conversation of a native source in `workspace_id`, located at
/// `transcript`.
/// It ran unchecked if the ledger says so, if its current mode is Bypass or unknown, or if any
/// turn its rollout records ran outside a workspace-write or read-only sandbox, whatever mode it
/// is in now: a conversation switched back to Default still holds what it read with full access.
/// A rollout that cannot be read counts as unchecked.
pub(super) fn authorize_source(
    roster: &Roster,
    workspace_id: &str,
    source: &str,
    thread: &str,
    cwd: &str,
    transcript: &Path,
    mode: Option<crate::protocol::PermissionMode>,
) -> TranscriptDanger {
    let dangerous =
        roster.transcript_ever_dangerous_in("codex", Some(source), thread, workspace_id, cwd)
            || mode.is_none_or(|mode| mode == crate::protocol::PermissionMode::Bypass)
            || crate::rc::native_settings::codex_ran_unchecked(transcript).unwrap_or(true);
    TranscriptDanger {
        judged: Some(dangerous),
        authorized: true,
    }
}

/// Stamp the judged bit into this session.
///
/// **Monotonic**: an `info.dangerous` that is already true (a session freshly started with
/// bypass, say) is never washed out by the verdict.
///
/// `prepare_spawn` calls this in one place instead of letting every path copy the bit into
/// `SessionInfo` itself: a path that misses the copy reports a session that ran unchecked as
/// clean, and then the `Need::Drive` gate in `session_channel` counts for nothing against it —
/// the gate and the marker must come from the same verdict.
pub(super) fn stamp(info: &mut crate::protocol::SessionInfo, danger: TranscriptDanger) {
    info.dangerous = info.dangerous || danger.ever_dangerous();
}
