//! Human review reads the frozen public snapshots and captured plain file payloads.

use crate::domain::{
    privacy_git::{PreviewEntry, ProjectedHistory},
    repo::Repo,
    session_privacy::PublishedSnapshot,
};
use crate::hub::git::CapturedPublication;
use crate::ui::privacy_preview::write_value;
use anyhow::Result;
use std::{
    fs::File,
    io::{Seek, Write},
    path::PathBuf,
};

pub(super) struct Preview {
    pub path: PathBuf,
    display: Option<File>,
}

pub(super) fn prepare(
    history: &ProjectedHistory,
    destination: &serde_json::Value,
    source: &Repo,
    captured: &CapturedPublication,
    full: bool,
) -> Result<Preview> {
    // The terminal copy is spooled alongside the JSON so every file is read once, and a failed
    // preview cannot leave partially reviewed content on stdout.
    let mut display = full.then(tempfile::tempfile).transpose()?;
    let path = history.write_preview_with(destination, source, captured, |entry| {
        if let Some(output) = &mut display {
            match entry {
                PreviewEntry::Snapshot(snapshot) => {
                    writeln!(output, "Snapshot {}:", snapshot["commit"])?;
                    write_value(output, snapshot, 2)?;
                }
                PreviewEntry::File(file) => write_value(output, file, 2)?,
            }
        }
        Ok(())
    })?;
    Ok(Preview { path, display })
}

impl Preview {
    pub(super) fn show(&mut self, history: &ProjectedHistory) -> Result<()> {
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
        if let Some(display) = &mut self.display {
            display.rewind()?;
            std::io::copy(display, &mut output)?;
        }
        output.flush()?;
        Ok(())
    }
}
