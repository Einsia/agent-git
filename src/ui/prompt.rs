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
use dialoguer::{Confirm, Input, Password, Select};
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
pub fn confirm(prompt: &str, default: bool) -> Result<Option<bool>> {
    if !can_ask() {
        return Ok(None);
    }
    crate::telemetry::observe(crate::telemetry::Observation::Prompt);
    Ok(Confirm::new()
        .with_prompt(prompt)
        .default(default)
        .interact_opt()?)
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
    fn single_runtime_needs_no_question() {
        // A single candidate comes back directly even when there is no way to ask.
        assert_eq!(pick_runtime(&["codex"], "launch").unwrap(), Some("codex"));
        assert_eq!(pick_runtime(&[], "launch").unwrap(), None);
    }
}
