//! The side people use: the terminal interface.
//!
//! This module answers one question — **whether to open the interface right now**. Rendering,
//! the screens and the event loop are in the submodules below; the decision stands on its own
//! because the two ways of being wrong do not cost the same (see [`verdict`]).
//!
//! # The tests (`docs/07_tui.md` §1)
//!
//! `agit <cmd>` enters the TUI if and only if all four hold:
//!
//! 1. the command's **key argument is empty** — the caller decides that, so it is not here;
//! 2. **stdin and stdout are both a tty**;
//! 3. **not inside an agent session** (neither `AGIT_SESSION` nor any harness session id
//!    environment variable is set);
//! 4. not explicitly turned off (`--no-tui` / `AGIT_TUI=0` / `--json` / `-q` / `-y`).
//!    `main.rs` translates `--json` into `AGIT_TUI=0`; the other two each have their own
//!    environment variable.
//!
//! `--tui` (`AGIT_TUI=1`) overrides test 3, **but not test 4**: `--json` asks for
//! machine-readable output, which is incompatible with "pop up a full-screen interface", and
//! letting one flag silently override another only manufactures "why was this run different"
//! questions.
//!
//! # How a caller uses it
//!
//! ```ignore
//! pub fn run(args: Args) -> CmdResult {
//!     if args.target.is_none() {
//!         match tui::should_enter() {
//!             tui::Verdict::Enter => return tui::run(tui::Screen::Sessions),
//!             tui::Verdict::Explain(note) => tui::warn_skipped(&note),
//!             tui::Verdict::NoTerminal => return Ok(ExitCode::Interactive),
//!             tui::Verdict::Skip => {}
//!         }
//!     }
//!     ...  // the existing logic is unchanged, byte for byte
//! }
//! ```

pub mod screens;
pub mod term;
pub mod widgets;

use std::io::IsTerminal;

/// Whether this call enters the TUI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Enter.
    Enter,
    /// Do not enter, and **say so** — the case test 3 blocks, where the user is most likely
    /// waiting for the interface.
    Explain(String),
    /// Do not enter, silently: no terminal, or the user turned it off. Neither needs explaining.
    Skip,
    /// `--tui` asked for it explicitly, but there is no terminal here. The caller reports
    /// [`crate::ExitCode::Interactive`].
    NoTerminal,
}

/// Every external fact the decision uses.
///
/// Splitting them out is what makes [`verdict`] a pure function: three of the four tests come
/// from the process environment, and changing an environment variable in a test is process-wide,
/// so it necessarily collides with other tests running in parallel.
#[derive(Debug, Clone)]
pub struct Signals {
    /// Whether stdin and stdout are both attached to a terminal.
    pub interactive: bool,
    /// `--tui` / `AGIT_TUI=1`.
    pub forced: bool,
    /// Which switch turned it off explicitly (for whether to explain later; only presence is
    /// tested now).
    pub off: Option<&'static str>,
    /// The "inside an agent session" evidence that matched: variable name + value.
    pub agent_session: Option<(&'static str, String)>,
}

impl Signals {
    /// Read them all from the current process.
    pub fn from_process() -> Signals {
        Signals {
            interactive: std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
            forced: switch("AGIT_TUI") == Some(true),
            off: off_switch(),
            agent_session: agent_session(),
        }
    }
}

/// The pure decision. The order of the four tests is the order of the branches below.
///
/// # Why test 3 speaks up and the other two do not
///
/// The three ways of not entering are nothing alike:
///
/// * **not a tty** (a pipe or CI): nobody is watching that side, and a line of explanation only
///   pollutes stderr;
/// * **turned off by the user**: they just typed `--no-tui`; saying it back is noise;
/// * **inside an agent session**: they typed the bare command expecting a list and got a screen
///   of plain text. Unexplained, that is exactly the "implicit" `docs/07_tui.md` §1 argues
///   against — so this one has to be said, and has to say which test blocked it and how to get
///   around it.
pub fn verdict(s: &Signals) -> Verdict {
    // Test 4 comes first: explicitly off is off, and `--tui` does not override it.
    if s.off.is_some() {
        return Verdict::Skip;
    }
    // Test 2. Here `--tui` does not override; it turns into an error: the interface was asked
    // for explicitly and there is no terminal at all, and degrading silently reads as the
    // command having had no effect.
    if !s.interactive {
        return if s.forced {
            Verdict::NoTerminal
        } else {
            Verdict::Skip
        };
    }
    if s.forced {
        return Verdict::Enter;
    }
    // Test 3.
    match &s.agent_session {
        Some((var, value)) => Verdict::Explain(skip_note(var, value)),
        None => Verdict::Enter,
    }
}

/// Decide once from the current process.
pub fn should_enter() -> Verdict {
    verdict(&Signals::from_process())
}

/// The sentence for when test 3 blocks.
///
/// None of its three parts is mere wording: the **test** (which variable, what value) tells the
/// user what the verdict rests on, the **way around it** gets them what they wanted in one step,
/// and **"it runs as usual"** says this is not a refusal. Drop any one of them and the user is
/// left guessing.
pub fn skip_note(var: &str, value: &str) -> String {
    format!(
        "not opening the TUI: this looks like an agent session ({var}={value}). \
         use `agit --tui` to open it anyway."
    )
}

/// Print the [`Verdict::Explain`] sentence. It goes to stderr, so stdout is still consumable by
/// a pipe.
pub fn warn_skipped(note: &str) {
    crate::ui::warning(note);
}

/// The evidence for "inside an agent session".
///
/// `AGIT_SESSION` comes first: agit injects it itself, so its presence means agit put this
/// process into some agent session. The rest are what each harness exposes on its own, covering
/// "typing agit by hand inside a claude the user started"; the full set of names is shared with
/// the context-resolution chain, so the two cannot drift apart.
fn agent_session() -> Option<(&'static str, String)> {
    let named = |var: &'static str| {
        std::env::var(var)
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| (var, v))
    };
    named("AGIT_SESSION").or_else(|| {
        // The full set of variable names has one home, `infra::runtime_session`: the
        // context-resolution chain, the guard on `import @` and this lookup must read the same
        // table, or "does this count as inside an agent session" gets two answers depending on
        // the entry point.
        crate::infra::runtime_session::ENV_SESSIONS
            .iter()
            .find_map(|(var, _)| named(var))
    })
}

/// The switches that turn the TUI off explicitly; returns the one that matched.
///
/// `--json` / `-q` / `-y` count too: each of them says "the consumer this time is not a person
/// sitting at a terminal" (machine-readable, quiet, do not ask me). A full-screen interface is
/// compatible with none of the three.
fn off_switch() -> Option<&'static str> {
    if switch("AGIT_TUI") == Some(false) {
        return Some("--no-tui");
    }
    // `--json` is **not in this table**: it has no environment variable of its own (JSON is a
    // parameter threaded down through `dispatch`). `main.rs` sees `--json` and writes `AGIT_TUI`
    // as `0` directly, which is the branch above — an `AGIT_JSON` that nobody writes would read
    // as "this test is handled" while being dead.
    for (var, flag) in [("AGIT_QUIET", "--quiet"), ("AGIT_YES", "--yes")] {
        if std::env::var_os(var).is_some() {
            return Some(flag);
        }
    }
    None
}

/// Read a three-state switch: `1`/`true` is on, `0`/`false` is off, anything else (including
/// unset) takes no position.
///
/// "set to any value" is not on: `AGIT_TUI=0` is the most natural way to turn it off, and
/// reading it as "non-empty is true" turns it on instead — the worst kind of counter-intuitive
/// behavior.
fn switch(var: &str) -> Option<bool> {
    parse_switch(&std::env::var(var).ok()?)
}

/// Value parsing. Split from [`switch`] only so it can be tested directly — changing an
/// environment variable is process-wide and collides with other tests running in parallel.
/// **This is the only copy of the logic**; do not write a second one in the tests.
fn parse_switch(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Signals, Verdict, verdict};

    fn sig() -> Signals {
        Signals {
            interactive: true,
            forced: false,
            off: None,
            agent_session: None,
        }
    }

    #[test]
    fn a_human_at_a_terminal_gets_the_tui() {
        assert_eq!(verdict(&sig()), Verdict::Enter);
    }

    /// In a pipe or CI it degrades silently — nobody is watching that side, and a line of
    /// explanation is only noise.
    #[test]
    fn a_pipe_gets_plain_text_without_a_word() {
        let s = Signals {
            interactive: false,
            ..sig()
        };
        assert_eq!(verdict(&s), Verdict::Skip);
    }

    /// Inside an agent session the interface does not open, **but it must be explained**.
    ///
    /// This pins the wording itself: the test (variable name and value) and the way around it
    /// are both required, or the user facing a screen of plain text can only guess whether
    /// something is broken.
    #[test]
    fn an_agent_session_is_refused_out_loud() {
        let s = Signals {
            agent_session: Some(("AGIT_SESSION", "nana/payments@refund-fix".into())),
            ..sig()
        };
        let Verdict::Explain(note) = verdict(&s) else {
            panic!("an agent session must be explained, not silently degraded");
        };
        assert!(
            note.contains("AGIT_SESSION"),
            "the note must name which test: {note}"
        );
        assert!(
            note.contains("nana/payments@refund-fix"),
            "the note must carry the matched value: {note}"
        );
        assert!(
            note.contains("--tui"),
            "the note must give the way around it: {note}"
        );
    }

    /// `--tui` overrides test 3.
    #[test]
    fn an_explicit_request_wins_over_the_agent_session_check() {
        let s = Signals {
            forced: true,
            agent_session: Some(("CLAUDE_CODE_SESSION_ID", "abc".into())),
            ..sig()
        };
        assert_eq!(verdict(&s), Verdict::Enter);
    }

    /// But `--tui` does **not** override test 4.
    ///
    /// `--json` asks for machine-readable output; letting one flag silently override another
    /// only manufactures "why was this run different" questions.
    #[test]
    fn an_explicit_request_does_not_win_over_an_explicit_off_switch() {
        for off in ["--no-tui", "--json", "--quiet", "--yes"] {
            let s = Signals {
                forced: true,
                off: Some(off),
                ..sig()
            };
            assert_eq!(verdict(&s), Verdict::Skip, "{off} turns the TUI off");
        }
    }

    /// Asking for the interface explicitly with no terminal is an error, not a silent
    /// degradation.
    #[test]
    fn asking_for_the_tui_without_a_terminal_is_an_error() {
        let s = Signals {
            interactive: false,
            forced: true,
            ..sig()
        };
        assert_eq!(verdict(&s), Verdict::NoTerminal);
    }

    /// `AGIT_TUI=0` must mean off. Reading it as "non-empty is true" is the most
    /// counter-intuitive way to be wrong.
    #[test]
    fn the_switch_reads_zero_as_off_not_as_set() {
        assert_eq!(super::parse_switch("0"), Some(false));
        assert_eq!(super::parse_switch("false"), Some(false));
        assert_eq!(super::parse_switch("1"), Some(true));
        assert_eq!(super::parse_switch("TRUE"), Some(true));
        assert_eq!(super::parse_switch("maybe"), None);
    }
}
