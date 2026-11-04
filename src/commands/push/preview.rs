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
        write_left_out(&mut output, history)?;
        if let Some(display) = &mut self.display {
            display.rewind()?;
            std::io::copy(display, &mut output)?;
        }
        output.flush()?;
        Ok(())
    }
}

/// Say how many records the publication adds that keep parts the privacy check left out, for a
/// part whose summary is not shown.
pub(super) fn show_left_out(history: &ProjectedHistory) -> Result<()> {
    let mut output = std::io::stdout().lock();
    write_left_out(&mut output, history)?;
    output.flush()?;
    Ok(())
}

fn write_left_out(output: &mut impl Write, history: &ProjectedHistory) -> Result<()> {
    match history.left_out_records() {
        0 => {}
        1 => writeln!(
            output,
            "1 record in this upload has parts the privacy check left out; the conversation file on this computer still has them."
        )?,
        records => writeln!(
            output,
            "{records} records in this upload have parts the privacy check left out; the conversation files on this computer still have them."
        )?,
    }
    Ok(())
}
