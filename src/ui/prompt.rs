//! Interactive prompting.
//!
//! # Non-interactive environments must work
//!
//! agit runs in CI and in pipes. There stdin is not a tty, and code that tries to read input
//! gets EOF immediately — done wrong, that becomes "silently took the default" or "hangs waiting
//! for input that never comes".
//!
//! So every function checks for a tty first and returns `None` ("cannot ask") when there is
//! none, leaving the caller to decide what to do. The caller usually raises a "say it explicitly
//! with `--flag`" error — safer than guessing an answer.

use crate::Result;
use dialoguer::{Confirm, Input, Password, Select};
use std::io::IsTerminal;

fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

pub fn select(prompt: &str, options: &[&str]) -> Result<Option<usize>> {
    if !interactive() || options.is_empty() {
        return Ok(None);
    }
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
    if !interactive() {
        return Ok(None);
    }
    Ok(Confirm::new()
        .with_prompt(prompt)
        .default(default)
        .interact_opt()?)
}

pub fn input(prompt: &str, default: Option<&str>) -> Result<Option<String>> {
    if !interactive() {
        return Ok(None);
    }
    // The builder methods take self by value, so this rebinds instead of calling on a mutable
    // binding.
    let mut i = Input::<String>::new().with_prompt(prompt);
    if let Some(d) = default {
        i = i.default(d.to_string());
    }
    Ok(Some(i.interact_text()?))
}

/// Read a password without echoing it.
pub fn password(prompt: &str) -> Result<Option<String>> {
    if !interactive() {
        return Ok(None);
    }
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
