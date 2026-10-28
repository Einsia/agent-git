//! Interactive prompting.
//!
//! # Non-interactive environments must work
//!
//! agit runs in CI and in pipes. There stdin is not a tty, and code that tries to read input
//! gets EOF immediately — done wrong, that becomes "silently took the default" or "hangs waiting
//! for input that never comes".
//!
//! So every function checks [`can_ask`] first and returns `None` ("cannot ask") when nobody can
//! answer, leaving the caller to decide what to do. The caller usually raises a "say it explicitly
//! with `--flag`" error — safer than guessing an answer.

use crate::Result;
use dialoguer::console::{Key, Term};
use dialoguer::theme::{SimpleTheme, Theme};
use dialoguer::{Input, Password, Select};
use std::io::IsTerminal;

pub(crate) fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Whether a question drawn now reaches a person who can answer it.
///
/// Terminals on stdin and stdout are necessary but not sufficient. An agent's tool shell can run
/// commands under a pseudo-terminal that nobody types into, and a question there blocks the
/// command until the tool call times out — while holding whatever locks it holds. So nothing is
/// asked inside an agent session (`AGIT_SESSION` or any runtime session variable), or when
/// stderr, where the question is drawn, is not a terminal. Every question in this module passes
/// through this check, so a call site cannot bring the wait back; a caller that chooses between
/// an interactive and an unattended flow tests this rather than [`interactive`].
pub(crate) fn can_ask() -> bool {
    interactive()
        && std::io::stderr().is_terminal()
        && crate::tui::Signals::from_process().agent_session.is_none()
}

/// Whether a repository-creation or selection question may be asked.
///
/// On top of [`can_ask`], nothing is asked when the answer is meant for a program: with `--json`
/// or under `CI`. A caller that cannot ask takes its documented default and says which, or
/// refuses and names the flag that answers. `-y` is an answer: a confirmation site honors it
/// before consulting this, so skipping the question never turns a `-y` into a refusal.
pub(crate) fn may_prompt() -> bool {
    can_ask() && std::env::var_os("CI").is_none() && !crate::commands::json::requested()
}

pub fn select(prompt: &str, options: &[&str]) -> Result<Option<usize>> {
    if !can_ask() || options.is_empty() {
        return Ok(None);
    }
    crate::telemetry::observe(crate::telemetry::Observation::Prompt);
    Ok(Select::new()
        .with_prompt(prompt)
        .items(options)
        .default(0)
        .interact_opt()?)
}

/// A yes/no confirmation.
///
/// Non-interactive returns None and **not** the default — a dangerous operation with no one to
/// ask must refuse to run rather than take the default.
///
/// The answer is taken on Enter, not on the `y`/`n` keystroke. If a single key answered, the
/// Enter of a habitual `y⏎` would land on the next question and take its default unseen. Esc or
/// `q` cancels at once, as in every other prompt: a cancel asks nothing further, so no keystroke
/// can spill.
///
/// The key loop never spins: a read error or end of input cancels, and a terminal the key reader
/// itself does not recognise is not asked, because there it returns `Key::Unknown` without
/// waiting.
pub fn confirm(prompt: &str, default: bool) -> Result<Option<bool>> {
    if !can_ask() {
        return Ok(None);
    }
    let term = Term::stderr();
    if !term.is_term() {
        return Ok(None);
    }
    crate::telemetry::observe(crate::telemetry::Observation::Prompt);
    let theme = SimpleTheme;
    let draw = |value: bool| -> std::io::Result<()> {
        let mut line = String::new();
        let _ = theme.format_confirm_prompt(&mut line, prompt, Some(value));
        term.clear_line()?;
        term.write_str(&line)?;
        term.flush()
    };
    let mut value = default;
    draw(value)?;
    term.hide_cursor()?;
    let answer = loop {
        match confirm_key(term.read_key()) {
            ConfirmKey::Set(chosen) => value = chosen,
            ConfirmKey::Answer => break Ok(Some(value)),
            ConfirmKey::Cancel => break Ok(None),
            ConfirmKey::Ignore => continue,
        }
        if let Err(error) = draw(value) {
            break Err(error);
        }
    };
    term.clear_line()?;
    term.show_cursor()?;
    let answer = answer?;
    let mut line = String::new();
    let _ = theme.format_confirm_prompt_selection(&mut line, prompt, answer);
    term.write_line(&line)?;
    Ok(answer)
}

#[derive(Debug, PartialEq, Eq)]
enum ConfirmKey {
    Set(bool),
    Answer,
    Cancel,
    Ignore,
}

/// What one read means to [`confirm`].
///
/// Enter arrives as `Key::Enter` from both key readers, and as a bare `\r` or `\n` character
/// from consoles that pass it through. Ctrl-C and Ctrl-D arrive as characters in raw mode and
/// cancel, as a failed read does. A modifier key alone (`Key::Shift`, or `Key::Unknown` for Ctrl
/// on Windows) is ignored, so typing `Y` does not cancel.
fn confirm_key(key: std::io::Result<Key>) -> ConfirmKey {
    match key {
        Ok(Key::Char('y' | 'Y')) => ConfirmKey::Set(true),
        Ok(Key::Char('n' | 'N')) => ConfirmKey::Set(false),
        Ok(Key::Enter | Key::Char('\r' | '\n')) => ConfirmKey::Answer,
        Ok(Key::Escape | Key::CtrlC | Key::Char('q' | '\u{3}' | '\u{4}')) | Err(_) => {
            ConfirmKey::Cancel
        }
        Ok(_) => ConfirmKey::Ignore,
    }
}

pub fn input(prompt: &str, default: Option<&str>) -> Result<Option<String>> {
    if !can_ask() {
        return Ok(None);
    }
    // The builder methods take self by value, so this rebinds instead of calling on a mutable
    // binding.
    crate::telemetry::observe(crate::telemetry::Observation::Prompt);
    let mut i = Input::<String>::new().with_prompt(prompt);
    if let Some(d) = default {
        i = i.default(d.to_string());
    }
    Ok(Some(i.interact_text()?))
}

/// Read a password without echoing it.
pub fn password(prompt: &str) -> Result<Option<String>> {
    if !can_ask() {
        return Ok(None);
    }
    crate::telemetry::observe(crate::telemetry::Observation::Prompt);
    Ok(Some(Password::new().with_prompt(prompt).interact()?))
}

/// Pick one of several runtimes.
///
/// A single candidate is not asked about — a question with only one answer wastes the user's
/// time.
pub fn pick_runtime(candidates: &[&'static str], action: &str) -> Result<Option<&'static str>> {
    match candidates {
        [] => Ok(None),
        [only] => Ok(Some(only)),
        many => Ok(select(&format!("Which runtime to {action}?"), many)?.map(|i| many[i])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Under test stdin is not a tty, so every function returns None instead of blocking.
    ///
    /// This pins the tty check: an implementation that drops it hangs here until the test times
    /// out.
    #[test]
    fn non_interactive_never_blocks() {
        assert_eq!(select("x", &["a", "b"]).unwrap(), None);
        assert_eq!(confirm("x", true).unwrap(), None);
        assert_eq!(input("x", Some("d")).unwrap(), None);
        assert_eq!(password("x").unwrap(), None);
    }

    #[test]
    fn confirmation_answers_only_on_enter() {
        assert_eq!(confirm_key(Ok(Key::Char('y'))), ConfirmKey::Set(true));
        assert_eq!(confirm_key(Ok(Key::Char('N'))), ConfirmKey::Set(false));
        for enter in [Key::Enter, Key::Char('\r'), Key::Char('\n')] {
            assert_eq!(confirm_key(Ok(enter)), ConfirmKey::Answer);
        }
    }

    #[test]
    fn confirmation_cancels_on_end_of_input_and_read_errors() {
        for key in [
            Key::Escape,
            Key::CtrlC,
            Key::Char('q'),
            Key::Char('\u{3}'),
            Key::Char('\u{4}'),
        ] {
            assert_eq!(confirm_key(Ok(key)), ConfirmKey::Cancel);
        }
        for kind in [
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::Interrupted,
            std::io::ErrorKind::Other,
        ] {
            assert_eq!(confirm_key(Err(kind.into())), ConfirmKey::Cancel);
        }
    }

    /// Typing a capital `Y` presses Shift first, which Windows reports as its own key.
    #[test]
    fn confirmation_ignores_modifier_keys() {
        for key in [Key::Shift, Key::Alt, Key::Unknown, Key::ArrowLeft] {
            assert_eq!(confirm_key(Ok(key)), ConfirmKey::Ignore);
        }
    }

    #[test]
    fn single_runtime_needs_no_question() {
        // A single candidate comes back directly even when there is no way to ask.
        assert_eq!(pick_runtime(&["codex"], "launch").unwrap(), Some("codex"));
        assert_eq!(pick_runtime(&[], "launch").unwrap(), None);
    }
}
