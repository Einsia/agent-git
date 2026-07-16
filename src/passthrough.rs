//! Transparent git passthrough.
//!
//! PRD: "The default scope must be a transparent Git wrapper: arguments, exit codes, stdout, hooks, remotes, and
//! credential helpers all stay compatible."
//!
//! So for non-native subcommands we spawn (not exec) the corresponding repo's git, inherit stdio, propagate the exit code,
//! then run a post-hook once it finishes (if a ref moved, write a WorkspaceRevision). We spawn rather than exec to keep
//! the chance to run the post-hook; inheriting stdio lets the credential helper, interactive prompts, and hooks all work as usual.

use crate::scope::{self, Scope};
use crate::workspace;
use anyhow::Result;
use std::path::Path;
use std::process::{Command, Stdio};

pub fn run(scope: Scope, args: &[String]) -> Result<i32> {
    let root = scope::root_for(scope)?;
    let subcommand = args.first().cloned().unwrap_or_default();

    // HEAD before any ref moved, used to tell whether it actually changed.
    let before = head_of(&root);

    let status = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    let code = status.code().unwrap_or(-1);

    // post-hook: command succeeded, it's a ref-moving subcommand, and HEAD actually changed → record a pairing.
    if code == 0 && workspace::moves_ref(&subcommand) {
        let after = head_of(&root);
        if after != before {
            // A pairing failure shouldn't fail the main command, just warn.
            if let Err(e) = workspace::record(&workspace::trigger_label(scope, &subcommand)) {
                eprintln!("agit: executed, but failed to generate WorkspaceRevision: {e:#}");
            }
        }
    }

    Ok(code)
}

fn head_of(root: &Path) -> String {
    scope::git_in_status(root, &["rev-parse", "HEAD"]).1
}
