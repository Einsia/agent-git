//! Publication review keeps complete checked content distinct from report labels.

use crate::domain::{privacy_publication::ProjectionReport, storage};
use anyhow::Result;
use serde_json::Value;
use std::{collections::BTreeMap, io::Write, path::Path};

pub(crate) enum Recovery {
    PublicOnly,
    FullSession,
    SelectedRecords,
}

pub(crate) fn write_report(
    output: &mut impl Write,
    report: &ProjectionReport,
    recovery: Recovery,
) -> Result<()> {
    writeln!(
        output,
        "Privacy preview: {} records checked, {} omissions, {} replacements, {} secret matches",
        report.records,
        report.omissions.len(),
        report.replacements,
        report.secret_matches
    )?;
    for notice in &report.omissions {
        writeln!(
            output,
            "  Source LOG record {}: omitted from public content",
            notice.record.saturating_add(1)
        )?;
        write_value(output, &Value::String(notice.reason.clone()), 4)?;
    }
    if let Some(details) = &report.details {
        writeln!(
            output,
            "Policy version {}: {} ({} processing decisions)",
            details.policy_version,
            details.policy_digest,
            details.decisions.len()
        )?;
        for action in [
            crate::domain::privacy_publication::ProjectionAction::AllowPath,
            crate::domain::privacy_publication::ProjectionAction::ExcludeSource,
            crate::domain::privacy_publication::ProjectionAction::RewriteText,
            crate::domain::privacy_publication::ProjectionAction::MaskSecret,
        ] {
            let matches = details
                .decisions
                .iter()
                .filter(|decision| decision.action == action)
                .map(|decision| decision.matches)
                .sum::<usize>();
            if matches > 0 {
                let action = serde_json::to_string(&action)?;
                writeln!(output, "  {}: {matches} matches", action.trim_matches('"'))?;
            }
        }
        let mut rules = BTreeMap::<&str, usize>::new();
        for decision in &details.decisions {
            *rules.entry(decision.rule.as_str()).or_default() += decision.matches;
        }
        for (rule, matches) in rules {
            writeln!(output, "  Rule {rule}: {matches} matches")?;
        }
    }
    writeln!(
        output,
        "{}",
        match recovery {
            Recovery::PublicOnly =>
                "Recovery: this output contains checked public content only; original records are not included.",
            Recovery::FullSession =>
                "Recovery: the viewing key can recover the complete original LOG and VIEW, including omitted public content.",
            Recovery::SelectedRecords =>
                "Recovery: your viewing password can recover the selected original records; the share link displays the checked public content.",
        }
    )?;
    Ok(())
}

pub(crate) struct Summary<'a> {
    pub snapshots: usize,
    pub records: usize,
    pub omissions: usize,
    pub path_substitutions: usize,
    pub replacements: usize,
    pub secret_matches: usize,
    pub public_digest: &'a str,
    pub recovery: Recovery,
    pub preview_path: &'a Path,
}

pub(crate) fn write_summary(output: &mut impl Write, summary: Summary<'_>) -> Result<()> {
    let Summary {
        snapshots,
        records,
        omissions,
        path_substitutions,
        replacements,
        secret_matches,
        public_digest,
        recovery,
        preview_path,
    } = summary;
    writeln!(output, "Outgoing public session preview")?;
    writeln!(output, "Snapshots: {snapshots}")?;
    writeln!(output, "Records: {records}")?;
    writeln!(
        output,
        "Privacy processing: {path_substitutions} path substitutions, {replacements} text rewrites, {secret_matches} secret masks, {omissions} omissions"
    )?;
    writeln!(output, "Public digest: {public_digest}")?;
    writeln!(output, "Preview file: {}", preview_path.display())?;
    writeln!(
        output,
        "{}",
        match recovery {
            Recovery::PublicOnly => "Private originals: unavailable from this public-only output.",
            Recovery::FullSession => "Private originals: recoverable with the viewing password.",
            Recovery::SelectedRecords =>
                "Private originals: selected records are recoverable with the viewing password.",
        }
    )?;
    Ok(())
}

pub(crate) fn write_session(output: &mut impl Write, text: &str) -> Result<()> {
    for (index, record) in storage::parse_envelopes(text)?.into_iter().enumerate() {
        writeln!(output, "  Record {}:", index + 1)?;
        write_value(output, &record.content, 4)?;
    }
    Ok(())
}

/// Content stays complete while terminal controls remain visible escaped characters.
/// Quoted keys and indented string lines keep transcript text separate from report labels.
pub(crate) fn write_value(output: &mut impl Write, value: &Value, indent: usize) -> Result<()> {
    match value {
        Value::String(text) => {
            for line in text.split('\n') {
                let mut escaped = String::with_capacity(line.len());
                for character in line.chars() {
                    if character.is_control() {
                        escaped.extend(character.escape_default());
                    } else {
                        escaped.push(character);
                    }
                }
                writeln!(output, "{:indent$}| {escaped}", "")?;
            }
        }
        Value::Object(fields) if !fields.is_empty() => {
            for (key, value) in fields {
                writeln!(output, "{:indent$}{}:", "", serde_json::to_string(key)?)?;
                write_value(output, value, indent + 2)?;
            }
        }
        Value::Array(values) if !values.is_empty() => {
            for (index, value) in values.iter().enumerate() {
                writeln!(output, "{:indent$}[{index}]:", "")?;
                write_value(output, value, indent + 2)?;
            }
        }
        value => writeln!(output, "{:indent$}{value}", "")?,
    }
    Ok(())
}
