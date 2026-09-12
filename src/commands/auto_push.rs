//! Automatic publication runs after settlement releases its branch and session locks.

use crate::domain::repo::Repo;
use crate::ui;
use std::path::Path;
use std::process::{Command, Stdio};

pub(super) fn branch_tip(directory: &Path, branch: &str) -> Option<String> {
    Repo::open(directory)?
        .git(&["rev-parse", "--verify", &format!("refs/heads/{branch}")])
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
        Command::new(executable)
            .args(["--json", "push", &target])
            .current_dir(directory)
            .env("AGIT_SESSION", &target)
            .env("AGIT_TUI", "0")
            .env_remove("AGIT_YES")
            .stdin(Stdio::null())
            .output()
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
