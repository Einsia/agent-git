//! Human review reads the frozen public snapshots and captured plain file payloads.

use crate::domain::{privacy_git::ProjectedHistory, session_privacy::PublishedSnapshot};
use crate::hub::git::CapturedPublication;
use crate::ui::privacy_preview::write_value;
use anyhow::Result;
use std::io::Write;

pub(super) fn show(
    history: &ProjectedHistory,
    captured: &CapturedPublication,
    full: bool,
) -> Result<()> {
    let mut output = std::io::stdout().lock();
    let manifest = history.session_manifest();
    let encrypted = manifest
        .snapshots
        .iter()
        .filter(|entry| matches!(entry, PublishedSnapshot::SessionOwnerV2 { .. }))
        .count();
    writeln!(
        output,
        "Publication: {encrypted} encrypted snapshots, {} plain/file snapshots, {} preserved commits.",
        manifest.snapshots.len() - encrypted,
        manifest.preserved_commits.len()
    )?;
    if full {
        for snapshot in history.session_preview()? {
            writeln!(output, "Snapshot {}:", snapshot["commit"])?;
            write_value(&mut output, &snapshot, 2)?;
        }
        for file in history.plain_file_preview(captured)? {
            write_value(&mut output, &file, 2)?;
        }
    }
    output.flush()?;
    Ok(())
}
