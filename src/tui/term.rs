//! Terminal state guard: enter the full-screen interface, suspend, come back, restore.
//!
//! # Why there is a suspend and not just enter/exit
//!
//! The agit TUI's main action is to **hand the user into claude / codex's own interface**
//! (`docs/07_tui.md` §2). At that moment the whole terminal goes back — the agent's TUI wants the
//! same alt screen and the same raw mode. It is taken over again afterwards. So there are four
//! actions here, not two.
//!
//! # The only invariant: taking and giving must balance
//!
//! The terminal is state **outside the process**. One restore too few leaves the user with a
//! shell that does not echo and shows no cursor, and they will not connect that to agit — they
//! will think the terminal is broken. One restore too many is just as harmful: leaving the alt
//! screen again when it has already been left wipes what was in the user's scrollback.
//!
//! This invariant must hold for **any** sequence of actions, including a panic partway through,
//! an early return, and a subprocess crashing while suspended. So the state machine is pulled out
//! on its own ([`State`]) where it can be tested exhaustively; [`Guard`] is only responsible for
//! actually performing the actions the state machine computes, and for finishing up in `Drop`.
//!
//! # Two lessons carried over from `agit show --tui`
//!
//! * restoring goes through `Drop`, not "remember to call it at the end of the function" — panics
//!   and early returns both have to be covered;
//! * only `KeyEventKind::Press` is handled (see [`next_key`]).
//!
//! # Beyond balancing, each action must also be complete
//!
//! The exhaustive test over [`State`] only guarantees that takes and gives are equal in
//! **number**. Right count, one step missing from the action, and the symptom is just as hard to
//! track down — forgetting to show the cursor again when giving the terminal back leaves the user
//! with a terminal that shows no cursor. So the escape-sequence half is split out into
//! [`write_take`] / [`write_give`]: they write to a single [`std::io::Write`] and can be checked
//! byte by byte in a test with no tty.

use crate::Result;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// Which state the guard is currently in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// We hold the terminal: raw mode + alt screen.
    Owned,
    /// The terminal has been handed to someone else (a subprocess is running).
    Lent,
    /// Finished; no terminal state is held any more.
    Done,
}

/// What one state transition has to do to the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Take: raw mode + alt screen.
    Take,
    /// Give: leave the alt screen + turn off raw mode.
    Give,
    /// Do nothing.
    Nothing,
}

/// One operation on the guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Suspend,
    Resume,
    Drop,
}

impl State {
    /// Pure state machine. **It is what guarantees the balance**; [`Guard`] only executes.
    ///
    /// Every transition is idempotent: a repeated suspend, a drop on `Lent`, a resume of a guard
    /// that has already finished — all of these happen on real code paths (a panic while
    /// suspended, with `Drop` running right after on a terminal that has already been given
    /// back). Idempotent means each of them is `Nothing` rather than doing the action again.
    pub fn apply(self, op: Op) -> (State, Effect) {
        match (self, op) {
            (State::Owned, Op::Suspend) => (State::Lent, Effect::Give),
            (State::Lent, Op::Resume) => (State::Owned, Effect::Take),
            // Finishing gives the terminal back only if it is still held. A drop on `Lent` is a
            // common path: the TUI process panics while a subprocess is running.
            (State::Owned, Op::Drop) => (State::Done, Effect::Give),
            (State::Lent, Op::Drop) => (State::Done, Effect::Nothing),
            // Everything else is a no-op: a repeated suspend, a resume of something that is not
            // suspended, a second drop.
            (s, _) => (s, Effect::Nothing),
        }
    }
}

/// RAII guard that holds the terminal state.
pub struct Guard {
    state: State,
}

impl Guard {
    /// Take the terminal over.
    pub fn enter() -> Result<Guard> {
        apply_effect(Effect::Take)?;
        Ok(Guard {
            state: State::Owned,
        })
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// Hand the terminal to someone else (a subprocess is about to start).
    pub fn suspend(&mut self) -> Result<()> {
        self.step(Op::Suspend)
    }

    /// The subprocess ended; take the terminal over again.
    pub fn resume(&mut self) -> Result<()> {
        self.step(Op::Resume)
    }

    fn step(&mut self, op: Op) -> Result<()> {
        let (next, effect) = self.state.apply(op);
        // Record the state before doing the action: when the action fails the state has moved
        // on anyway, so `Drop` does not act a second time on a stale state. With the terminal
        // already in an intermediate state, repeating an action is worse than doing nothing.
        self.state = next;
        apply_effect(effect)
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let (next, effect) = self.state.apply(Op::Drop);
        self.state = next;
        // There is nothing else to do when the cleanup fails; reporting an error here only
        // makes the output messier.
        let _ = apply_effect(effect);
    }
}

fn apply_effect(effect: Effect) -> Result<()> {
    match effect {
        Effect::Take => take_steps(
            || Ok(enable_raw_mode()?),
            || write_take(&mut std::io::stdout()),
            || {
                let _ = disable_raw_mode();
            },
        ),
        // The order is the reverse of taking: give the screen back first, then turn off raw mode.
        Effect::Give => give_steps(
            || write_give(&mut std::io::stdout()),
            || Ok(disable_raw_mode()?),
        ),
        Effect::Nothing => Ok(()),
    }
}

/// The two steps of taking over, plus undoing the first when the second fails.
///
/// A failure here means `Guard::enter()` produced **no `Guard`**, so no `Drop` cleans up for us.
/// Without the rollback, raw mode stays on the user's shell — no echo, no sight of what they
/// typed, and they will not connect that to agit.
///
/// Failure is injectable into each of the three steps, so "whichever step blows up leaves no
/// intermediate state" is asserted, not believed.
fn take_steps(
    enable: impl FnOnce() -> Result<()>,
    write: impl FnOnce() -> Result<()>,
    undo_enable: impl FnOnce(),
) -> Result<()> {
    enable()?;
    if let Err(e) = write() {
        undo_enable();
        return Err(e);
    }
    Ok(())
}

/// The two steps of giving back: **both steps run**, then the first error is reported.
///
/// Propagating a screen-write failure with `?` skips the step that turns off raw mode, while the
/// state machine has already moved on to `Lent` / `Done` — `Drop` sees "already given back" and
/// does not try again. One stdout error is then enough to leave the user's shell in raw mode
/// forever.
fn give_steps(
    write: impl FnOnce() -> Result<()>,
    disable: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let wrote = write();
    let disabled = disable();
    wrote.and(disabled)
}

/// The escape sequence written when taking the terminal over.
///
/// Splitting this and [`write_give`] into two functions that write to nothing but a
/// [`std::io::Write`] is what makes **the action itself** testable: `enable_raw_mode` and its
/// kind are process-level terminal operations that cannot run at all in a test environment with
/// no tty, which leaves the whole of [`apply_effect`] untestable. What the exhaustive test over
/// [`State`] guarantees is **that the counts balance** — it cannot see whether an individual
/// action is complete, and that is the gap "forgot to show the cursor again when giving the
/// terminal back" slips through.
fn write_take(w: &mut impl std::io::Write) -> Result<()> {
    crossterm::execute!(w, EnterAlternateScreen, Hide)?;
    Ok(())
}

/// The escape sequence written when giving the terminal back.
///
/// **It must include "show the cursor".** ratatui hides the cursor as soon as it draws its first
/// frame, and after `suspend()` the terminal belongs to claude / codex — without this step the
/// user is left with an input line whose cursor is invisible, and will not connect that to agit.
fn write_give(w: &mut impl std::io::Write) -> Result<()> {
    crossterm::execute!(w, LeaveAlternateScreen, Show)?;
    Ok(())
}

/// Read one key, **press only**.
///
/// Without the filter, Windows terminals report a single keystroke as both a press and a
/// release, so the cursor moves two rows for one key. This is a real failure on
/// `agit show --tui`, not a hypothetical.
pub fn next_key() -> Result<Option<KeyEvent>> {
    match event::read()? {
        Event::Key(k) if k.kind == KeyEventKind::Press => Ok(Some(k)),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::{Effect, Op, State};

    /// One normal round trip: take → give → take again → finish.
    #[test]
    fn a_round_trip_hands_the_terminal_over_and_takes_it_back() {
        let s = State::Owned;
        let (s, e) = s.apply(Op::Suspend);
        assert_eq!((s, e), (State::Lent, Effect::Give));
        let (s, e) = s.apply(Op::Resume);
        assert_eq!((s, e), (State::Owned, Effect::Take));
        let (s, e) = s.apply(Op::Drop);
        assert_eq!((s, e), (State::Done, Effect::Give));
    }

    /// A panic while suspended: `Drop` runs on a terminal that has already been given back and
    /// must not give it back a second time.
    ///
    /// Giving it back twice is not a "harmless repeat" — leaving the alt screen again once it has
    /// already been left wipes what was in the user's scrollback.
    #[test]
    fn dropping_while_lent_does_not_give_the_terminal_back_twice() {
        let (s, e) = State::Lent.apply(Op::Drop);
        assert_eq!((s, e), (State::Done, Effect::Nothing));
    }

    /// Repeated operations do nothing. All of these happen on real paths.
    #[test]
    fn repeated_operations_are_no_ops() {
        assert_eq!(State::Lent.apply(Op::Suspend).1, Effect::Nothing);
        assert_eq!(State::Owned.apply(Op::Resume).1, Effect::Nothing);
        assert_eq!(State::Done.apply(Op::Drop).1, Effect::Nothing);
        assert_eq!(State::Done.apply(Op::Resume).1, Effect::Nothing);
        assert_eq!(State::Done.apply(Op::Suspend).1, Effect::Nothing);
    }

    fn boom() -> super::Result<()> {
        Err(anyhow::anyhow!("stdout is gone"))
    }

    /// When the second step of taking over fails, the first step is put back.
    ///
    /// At the moment this step fails `Guard` is not constructed yet, so no `Drop` cleans up for
    /// us — without the rollback, raw mode stays on the user's shell. The exhaustive state
    /// machine test cannot see this: it counts actions, while what happens here is **half of one
    /// action**.
    #[test]
    fn a_failed_take_puts_back_what_it_already_took() {
        let mut undone = false;
        let r = super::take_steps(|| Ok(()), boom, || undone = true);
        assert!(r.is_err(), "a screen-write failure propagates");
        assert!(undone, "raw mode must be restored");

        // The first step itself fails: nothing has been taken yet, and nothing is rolled back.
        let mut undone = false;
        assert!(super::take_steps(boom, || Ok(()), || undone = true).is_err());
        assert!(
            !undone,
            "nothing is undone when the first step never succeeded"
        );

        // No rollback when both steps succeed.
        let mut undone = false;
        assert!(super::take_steps(|| Ok(()), || Ok(()), || undone = true).is_ok());
        assert!(!undone);
    }

    /// **Both steps of giving back run**; a failure in either must not let the other be skipped.
    ///
    /// Returning early on a screen-write failure skips turning off raw mode, while the state
    /// machine has already moved on — `Drop` takes the terminal as given back and does not try
    /// again. One stdout error is enough to leave the shell without echo forever.
    #[test]
    fn giving_the_terminal_back_runs_both_steps_whatever_fails() {
        let mut disabled = false;
        let r = super::give_steps(boom, || {
            disabled = true;
            Ok(())
        });
        assert!(r.is_err(), "the error propagates");
        assert!(
            disabled,
            "a screen-write failure must still turn off raw mode"
        );

        // The other way round: the write succeeds and the disable fails; the error is reported
        // just the same.
        let mut wrote = false;
        let r = super::give_steps(
            || {
                wrote = true;
                Ok(())
            },
            boom,
        );
        assert!(r.is_err());
        assert!(wrote);

        // When both steps fail the first error is reported — the second must not mask it.
        assert!(super::give_steps(boom, boom).is_err());
    }

    fn emitted(f: impl FnOnce(&mut Vec<u8>) -> super::Result<()>) -> Vec<u8> {
        let mut buf = Vec::new();
        f(&mut buf).unwrap();
        buf
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// Giving the terminal back must show the cursor again.
    ///
    /// ratatui hides the cursor as soon as it draws its first frame, and after `suspend()` the
    /// terminal belongs to claude / codex. Without this step the user is left with an input line
    /// whose cursor is invisible, and will not think of agit as the cause.
    ///
    /// The assertion does not hard-code the escape sequence; it compares against the bytes
    /// crossterm itself emits — whichever crossterm version is in use, this pins "is the cursor
    /// shown", not "is it that exact string".
    #[test]
    fn giving_the_terminal_back_shows_the_cursor_again() {
        let give = emitted(super::write_give);
        let show = emitted(|w| {
            crossterm::execute!(w, super::Show)?;
            Ok(())
        });
        let leave = emitted(|w| {
            crossterm::execute!(w, super::LeaveAlternateScreen)?;
            Ok(())
        });
        assert!(
            contains(&give, &show),
            "giving the terminal back must show the cursor"
        );
        assert!(
            contains(&give, &leave),
            "giving the terminal back must leave the alt screen"
        );
    }

    /// Take and give are paired: whatever hides the cursor must show it again.
    #[test]
    fn taking_and_giving_are_symmetric_about_the_cursor() {
        let take = emitted(super::write_take);
        let hide = emitted(|w| {
            crossterm::execute!(w, super::Hide)?;
            Ok(())
        });
        let show = emitted(|w| {
            crossterm::execute!(w, super::Show)?;
            Ok(())
        });
        assert!(
            contains(&take, &hide),
            "taking the terminal hides the cursor"
        );
        assert!(
            !contains(&take, &show),
            "taking the terminal must not show the cursor"
        );
        assert!(contains(&emitted(super::write_give), &show));
    }

    /// **Invariant: after any sequence of operations has run out (final `Drop` included), takes
    /// and gives must be equal in number.**
    ///
    /// Exhausts every sequence up to length 5. This is the reason the whole module exists — the
    /// consequence of not balancing is a shell with no echo and no cursor, which the user will
    /// not connect to agit.
    #[test]
    fn take_and_give_always_balance_out() {
        const OPS: [Op; 3] = [Op::Suspend, Op::Resume, Op::Drop];
        fn walk(state: State, taken: i32, depth: usize, trail: &mut Vec<Op>) {
            // Every path ends in a Drop; RAII guarantees that one happens.
            let (_, e) = state.apply(Op::Drop);
            let end = taken + delta(e);
            assert_eq!(
                end, 0,
                "sequence {trail:?} + Drop does not balance (net {end} takes still held)"
            );
            if depth == 0 {
                return;
            }
            for op in OPS {
                let (next, e) = state.apply(op);
                trail.push(op);
                // After a Drop the guard is gone, so the walk stops there.
                if op != Op::Drop {
                    walk(next, taken + delta(e), depth - 1, trail);
                }
                trail.pop();
            }
        }
        fn delta(e: Effect) -> i32 {
            match e {
                Effect::Take => 1,
                Effect::Give => -1,
                Effect::Nothing => 0,
            }
        }
        // The starting point is just after `Guard::enter()`: one take has already happened.
        walk(State::Owned, 1, 5, &mut Vec::new());
    }
}
