//! Final settlement belongs to the exact child and binding retained at Archive launch.

use std::process::Child;

use crate::domain::{merge_archive::ExplorationBinding, repo::Repo, store::Store};
use crate::{ExitCode, Result, ui};

/// A child exit cannot acknowledge an open merge or hide a failed final tail capture.
/// Capturing a landed tail is required even when the child exits unsuccessfully.
pub fn finish(
    repo: &Repo,
    store: &Store,
    binding: &ExplorationBinding,
    child: &mut Child,
) -> Result<ExitCode> {
    let status = child.wait()?;
    let captured = crate::commands::commit::archive::finish_child(repo, store, binding);
    if !status.success() {
        ui::error(&format!(
            "archive merge child did not exit successfully: {status}"
        ));
    }
    if let Err(error) = &captured {
        ui::error(&format!("{error:#}"));
    }
    Ok(if status.success() && captured.is_ok() {
        ExitCode::Ok
    } else {
        ExitCode::Precondition
    })
}
