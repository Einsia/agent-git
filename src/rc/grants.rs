//! Which commands the operator can answer on their own — **granted one at a time by the owner**,
//! persisted on this machine.
//!
//! # Why this file must ship together with policy's inversion
//!
//! After the inversion the built-in list in [`crate::rc::policy`] holds only the "read bytes,
//! write stdout" entries: `git status`, `cargo test`, `npm test` and `make` all go back to the
//! owner. And the operator role is defined as "answering routine approvals" — after the
//! inversion no routine approval is left, so the role is gone.
//!
//! The pressure lands on the only way out: switching the session to bypass. That is a strictly
//! worse trade — a reversible allow, one command at a time, becomes an **irreversible**
//! session-level surrender (`ever_dangerous` is monotonic).
//!
//! So loosening is still one owner action; it only moves from "editing code" to typing a
//! command.
//!
//! # Why a grant lives on the machine and never goes through the hub
//!
//! This is a decision **the owner of this machine** makes about **this machine**. Going through
//! the hub means three new RPC verbs, a permission model, hub-side storage and a screen of UI,
//! and every one of them has to answer "is this request really from the owner?" again — exactly
//! the question this feature avoids. Whoever types `agit rc grant` is already in this machine's
//! shell, and there is no stronger proof than that.
//!
//! # Why this does not go in `Mirror`
//!
//! `Mirror::adopt` `clear()`s the whole table on every reconnect — the hub is the truth about
//! "what the workspace is". A grant is not the hub's truth, and a reconnect must not erase it.
//! So: a file of its own.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// `~/.agit/rc/grants.json`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Grants {
    /// workspace_id → the command names the operator can answer on their own.
    #[serde(default)]
    pub heads: BTreeMap<String, BTreeSet<String>>,
}

const FILE: &str = "grants.json";

impl Grants {
    pub fn load() -> Grants {
        super::load_json(FILE)
    }

    pub fn save(&self) -> crate::Result<()> {
        super::save_json(FILE, self)
    }

    /// The command names the operator can answer on their own in this workspace.
    pub fn for_workspace(&self, workspace_id: &str) -> BTreeSet<String> {
        self.heads.get(workspace_id).cloned().unwrap_or_default()
    }

    /// Grant one. The name must be a **bare command name** — see [`is_bare_command_name`].
    pub fn grant(&mut self, workspace_id: &str, head: &str) -> crate::Result<()> {
        if !is_bare_command_name(head) {
            anyhow::bail!(
                "`{head}` is not a bare command name — grant `git`, not a path or a command line"
            );
        }
        self.heads
            .entry(workspace_id.to_string())
            .or_default()
            .insert(head.to_string());
        self.save()
    }

    /// Revoke one. Returns whether it was there.
    pub fn revoke(&mut self, workspace_id: &str, head: &str) -> crate::Result<bool> {
        let existed = self
            .heads
            .get_mut(workspace_id)
            .is_some_and(|s| s.remove(head));
        self.save()?;
        Ok(existed)
    }
}

/// A command name that can be granted.
///
/// **A bare name, not a command line and not a path.** What is granted is "the program `PATH`
/// resolves to"; granting a command line grants arbitrary code (`sh -c '...'` is a command
/// line), and granting a path allows it to point at the file the agent itself just wrote into
/// the workspace.
pub fn is_bare_command_name(head: &str) -> bool {
    !head.is_empty()
        && head.len() <= 64
        && head == head.trim()
        && head
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
        // `.` / `..` read as directories to `resolve_in_path`, and they are not command names.
        && !head.starts_with('.')
}

#[cfg(test)]
mod tests {
    use super::is_bare_command_name;

    /// What is granted is a **name**, not a command line and not a path.
    ///
    /// Granting a command line grants arbitrary code (`sh -c '...'` is itself a command line);
    /// granting a path allows it to point at the file the agent just wrote into the repo.
    #[test]
    fn only_a_bare_command_name_can_be_granted() {
        for ok in ["git", "cargo", "npm", "make", "go", "python3", "clang++"] {
            assert!(is_bare_command_name(ok), "`{ok}`");
        }
        for bad in [
            "",
            " git",
            "git ",
            "/usr/bin/curl",
            "./x",
            "../x",
            ".hidden",
            "sh -c 'curl x'",
            "git;curl",
            "git|sh",
            "git$(x)",
            "a/b",
        ] {
            assert!(!is_bare_command_name(bad), "`{bad}` must not be grantable");
        }
    }
}
