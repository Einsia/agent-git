//! Automatic publication runs after settlement releases its branch and session locks.

use crate::domain::repo::Repo;
use crate::ui;
use std::path::Path;
use std::process::Stdio;

pub(super) const AUTOMATIC_ENV: &str = "AGIT_AUTO_PUSH";

/// The refusal an automatic publication meets while its repository's `push.auto` is off.
pub(crate) const DISABLED_FOR_REPOSITORY: &str =
    "automatic publication is disabled for this repository";

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
        || std::env::var_os(AUTOMATIC_ENV).is_some()
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
    match super::project::blocks_auto_push(&repo, slug, branch) {
        Ok(false) => {}
        Ok(true) => return,
        Err(error) => {
            ui::warning(&format!(
                "saved locally; project policy could not be verified: {error:#}"
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
                // The next turn uploads again, so the way to stop it comes with the notice.
                if super::project::binds(slug) {
                    ui::hint(&format!("not this one? `agit project drop {target}`"));
                }
            }
        }
        Ok(output) => match failure_reason(&output.stdout) {
            Some((reason, hints)) => {
                ui::warning(&format!(
                    "saved locally; automatic push failed for {target}: {reason}"
                ));
                if hints.is_empty() {
                    ui::hint(&format!("retry with `agit push {target}`"));
                }
                for hint in hints {
                    ui::hint(&hint);
                }
            }
            None => unexplained(&target),
        },
        Err(_) => unexplained(&target),
    }
}

fn unexplained(target: &str) {
    ui::warning(&format!(
        "saved locally; automatic push failed for {target}. Run `agit push {target}` to inspect the problem and retry."
    ));
}

/// The child's own error and hint lines, so the reason and its next commands are read where the
/// turn was settled. A child that produced no envelope, or no error line in it, has no reason to
/// relay.
fn failure_reason(stdout: &[u8]) -> Option<(String, Vec<String>)> {
    let document: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let lines = document["diagnostics"]["stderr"].as_array()?;
    let messages = |level: &str| {
        lines
            .iter()
            .filter(|line| line["level"] == level)
            .filter_map(|line| line["message"].as_str().map(str::to_owned))
            .collect::<Vec<_>>()
    };
    let reason = messages("error").join("; ");
    (!reason.is_empty()).then(|| (reason, messages("hint")))
}

/// The settling command repeats the child's reason and next commands; reading only the exit
/// status would leave "run push to inspect" as the whole explanation.
#[cfg(test)]
#[test]
fn a_failed_child_relays_its_error_and_hints() {
    let envelope = serde_json::json!({
        "ok": false,
        "diagnostics": {"stderr": [
            {"level": "info", "message": "Reading remote history and session modes…"},
            {"level": "error", "message": "remote branch work has diverged from this checkout"},
            {"level": "hint", "message": "keep both: `agit fork alice/app@work -b work-local`"},
        ]},
    });
    assert_eq!(
        failure_reason(envelope.to_string().as_bytes()),
        Some((
            "remote branch work has diverged from this checkout".into(),
            vec!["keep both: `agit fork alice/app@work -b work-local`".into()]
        ))
    );
    assert_eq!(failure_reason(b"not an envelope"), None);
}
