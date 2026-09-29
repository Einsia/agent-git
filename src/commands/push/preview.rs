//! Human review reads the same frozen public envelopes that publication will send.

use crate::domain::privacy_publication::ProjectionAction;
use crate::domain::{privacy_envelope::PrivacyEnvelope, privacy_git::ProjectedHistory, storage};
use crate::ui::privacy_preview::{Recovery, Summary, write_session, write_summary, write_value};
use anyhow::{Context, Result};
use std::{io::Write, path::Path};

pub(super) fn show(history: &ProjectedHistory, preview_path: &Path, full: bool) -> Result<()> {
    let mut output = std::io::stdout().lock();
    if !full {
        let reports = history.reports();
        let path_substitutions = reports
            .iter()
            .filter_map(|report| report.details.as_ref())
            .flat_map(|details| details.decisions.iter())
            .filter(|decision| {
                matches!(
                    decision.action,
                    ProjectionAction::AllowPath | ProjectionAction::ExcludeSource
                )
            })
            .map(|decision| decision.matches)
            .sum();
        write_summary(
            &mut output,
            Summary {
                snapshots: reports.len(),
                records: reports.iter().map(|report| report.records).sum(),
                omissions: reports.iter().map(|report| report.omissions.len()).sum(),
                path_substitutions,
                replacements: reports.iter().map(|report| report.replacements).sum(),
                secret_matches: reports.iter().map(|report| report.secret_matches).sum(),
                public_digest: &history.public_digest()?,
                recovery: Recovery::FullSession,
                preview_path,
            },
        )?;
        write_examples(history, &mut output)?;
        output.flush()?;
        return Ok(());
    }
    writeln!(output, "\nOutgoing public session preview")?;
    writeln!(
        output,
        "Original records and path mappings are recoverable with your viewing password."
    )?;
    for commit in history.plan().commit_objects() {
        let bytes = history
            .repo()
            .show_result(commit, "privacy/envelope.json")?
            .context("projected snapshot has no envelope")?;
        let envelope = PrivacyEnvelope::parse(bytes.as_bytes())?;
        writeln!(output, "\nSnapshot {commit}")?;
        writeln!(output, "Policy: {}", envelope.policy_digest)?;
        writeln!(output, "Public content: {}", envelope.snapshot_digest)?;
        let public = &envelope.public_projection;
        writeln!(output, "Processing report:")?;
        write_value(&mut output, &public["report"], 2)?;
        writeln!(output, "Public metadata:")?;
        write_value(&mut output, &public["metadata"], 2)?;
        for name in ["log", "view"] {
            let text = public["session"][name]
                .as_str()
                .context("public preview has no session text")?;
            if name == "view" && public["session"]["log"].as_str() == Some(text) {
                writeln!(output, "VIEW is identical to the complete LOG above.")?;
                continue;
            }
            writeln!(output, "Complete public {}:", name.to_uppercase())?;
            write_session(&mut output, text)?;
        }
    }
    output.flush()?;
    Ok(())
}

fn write_examples(history: &ProjectedHistory, output: &mut impl Write) -> Result<()> {
    let mut emitted = 0;
    for commit in history.plan().commit_objects() {
        let bytes = history
            .repo()
            .show_result(commit, "privacy/envelope.json")?
            .context("projected snapshot has no envelope")?;
        let envelope = PrivacyEnvelope::parse(bytes.as_bytes())?;
        let log = envelope.public_projection["session"]["log"]
            .as_str()
            .context("public preview has no session log")?;
        for record in storage::parse_envelopes(log)? {
            let blocks = record.content["message"]["content"]
                .as_array()
                .into_iter()
                .flatten();
            for block in blocks {
                let Some(text) = block["text"].as_str() else {
                    continue;
                };
                let escaped = text
                    .chars()
                    .flat_map(|character| {
                        if character.is_control() {
                            character.escape_default().collect::<Vec<_>>()
                        } else if character == '\n' {
                            vec![' ']
                        } else {
                            vec![character]
                        }
                    })
                    .collect::<String>();
                writeln!(output, "Example: {}", crate::ui::truncate(&escaped, 120))?;
                emitted += 1;
                if emitted == 2 {
                    return Ok(());
                }
            }
        }
    }
    if emitted == 0 {
        writeln!(output, "Examples: none")?;
    }
    Ok(())
}
