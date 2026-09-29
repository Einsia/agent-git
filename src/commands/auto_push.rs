//! Automatic publication runs after settlement releases its branch and session locks.

use crate::domain::repo::Repo;
use crate::ui;
use std::path::Path;
use std::process::Stdio;

pub(super) const AUTOMATIC_ENV: &str = "AGIT_AUTO_PUSH";

/// A background child may use saved consent, never a parent's interactive confirmation flag.
pub(crate) fn configure(command: &mut std::process::Command) {
    command.env(AUTOMATIC_ENV, "1").env_remove("AGIT_YES");
}

#[cfg(all(test, unix))]
#[test]
fn unattended_children_cannot_inherit_interactive_consent() {
    let mut command = std::process::Command::new("sh");
    command.env("AGIT_YES", "1").args([
        "-c",
        "test \"$AGIT_AUTO_PUSH\" = 1 && test -z \"${AGIT_YES+x}\"",
    ]);
    configure(&mut command);
    assert!(command.status().unwrap().success());
}

pub(super) fn branch_tip(directory: &Path, branch: &str) -> Option<String> {
    let repo = Repo::open(directory)?;
    let branch_ref = format!("refs/heads/{branch}");
    if let Some(commit) = repo.native_branch_commit(&branch_ref) {
        return Some(commit);
    }
    repo.git(&["rev-parse", "--verify", &branch_ref])
        .ok()
        .map(|value| value.trim().to_owned())
}

pub(super) fn after_settlement(
    directory: &Path,
    slug: &str,
    branch: &str,
    before: Option<&str>,
    quiet: bool,
) {
    if std::env::var_os(super::commit::SUPERVISOR_RESULT_ENV).is_some()
        || crate::rc::harness::settlement_is_delegated()
    {
        return;
    }
    let Some(after) = branch_tip(directory, branch) else {
        return;
    };
    if before == Some(after.as_str()) {
        return;
    }
    let repo = Repo::at(directory);
    match repo.auto_push_enabled() {
        Ok(false) => return,
        Ok(true) => {}
        Err(error) => {
            ui::warning(&format!(
                "saved locally; automatic pushing is disabled: {error:#}"
            ));
            return;
        }
    }
    let target = format!("{slug}@{branch}");
    // A child process isolates the noninteractive publish policy and its JSON output from the
    // settlement caller. It uses the ordinary identity, access, visibility and secret gates.
    let outcome = std::env::current_exe().and_then(|executable| {
        let mut command = crate::infra::background::command(executable);
        configure(&mut command);
        command
            .args(["--json", "push", &target])
            .current_dir(directory)
            .env("AGIT_SESSION", &target)
            .env("AGIT_TUI", "0")
            .stdin(Stdio::null());
        // Statistics attribute the push to the settling invocation, which may be a hook.
        if let Some(parent) = crate::telemetry::parent_invocation_id() {
            command.env("AGIT_TELEMETRY_PARENT_ID", parent);
        }
        command.output()
    });
    match outcome {
        Ok(output) if output.status.success() => {
            if !quiet {
                ui::success(&format!("automatically pushed {target}"));
            }
        }
        _ => {
            ui::warning(&format!(
                "saved locally; automatic push failed for {target}. Run `agit push {target}` to inspect the problem and retry."
            ));
        }
    }
}
