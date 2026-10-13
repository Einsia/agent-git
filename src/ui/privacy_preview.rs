//! Publication review keeps complete checked content distinct from report labels.

use crate::domain::{privacy_publication::ProjectionReport, storage};
use anyhow::Result;
use serde_json::Value;
use std::{collections::BTreeMap, io::Write};

pub(crate) fn write_report(output: &mut impl Write, report: &ProjectionReport) -> Result<()> {
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
        "Recovery: this output contains checked public content only; original records are not included."
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
