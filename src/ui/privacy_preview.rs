//! Publication review keeps complete checked content distinct from report labels.

use crate::domain::{
    privacy::omission::Omission,
    privacy_publication::{LeftOut, ProjectionReport},
    storage,
};
use anyhow::Result;
use serde_json::Value;
use std::{collections::BTreeMap, io::Write};

pub(crate) fn write_report(
    output: &mut impl Write,
    report: &ProjectionReport,
    left_out: &LeftOut,
) -> Result<()> {
    writeln!(
        output,
        "Privacy preview: {} records checked, {} omissions, {} replacements, {} secret matches",
        report.records,
        report.omissions.len(),
        report.replacements,
        report.secret_matches
    )?;
    write_left_out(output, left_out)?;
    let native = Omission::NativeRecord.reason();
    for notice in &report.omissions {
        // Bookkeeping is counted above; a record of the conversation is named with its kind.
        let reason = match left_out.conversation.get(&notice.record) {
            _ if notice.reason != native => notice.reason.clone(),
            Some(kind) => format!("{native} ({kind}), a record of the conversation"),
            None => continue,
        };
        writeln!(
            output,
            "  Source LOG record {}: omitted from public content",
            notice.record.saturating_add(1)
        )?;
        write_value(output, &Value::String(reason), 4)?;
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

/// The records left out whole, by kind: a runtime's bookkeeping, then whether any record of the
/// conversation is among them, so a person can tell whether the copy is missing part of it.
fn write_left_out(output: &mut impl Write, left_out: &LeftOut) -> Result<()> {
    const SHOWN: usize = 6;
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let total = left_out.bookkeeping.iter().map(|(_, n)| n).sum::<usize>();
    if total > 0 {
        let runtimes = left_out.runtimes.iter().collect::<Vec<_>>();
        let whose = match runtimes[..] {
            [runtime] if crate::adapter::runtime_label(runtime) != "agent" => {
                crate::adapter::runtime_label(runtime)
            }
            _ => "runtime",
        };
        let shown = if left_out.bookkeeping.len() > SHOWN + 1 {
            SHOWN
        } else {
            left_out.bookkeeping.len()
        };
        let mut kinds = left_out.bookkeeping[..shown]
            .iter()
            .map(|(kind, n)| format!("{n} {kind}"))
            .collect::<Vec<_>>();
        let rest = &left_out.bookkeeping[shown..];
        if !rest.is_empty() {
            let records = rest.iter().map(|(_, n)| n).sum::<usize>();
            kinds.push(format!("{records} of {} other kinds", rest.len()));
        }
        writeln!(
            output,
            "  {total} {whose} bookkeeping record{} left out, not part of the conversation: {}",
            plural(total),
            kinds.join(", ")
        )?;
    }
    let mut kinds = BTreeMap::<&str, usize>::new();
    for kind in left_out.conversation.values() {
        *kinds.entry(kind).or_default() += 1;
    }
    match left_out.conversation.len() {
        0 if total > 0 => writeln!(output, "  no record of the conversation itself is left out")?,
        0 => {}
        n => writeln!(
            output,
            "  {n} record{} of the conversation left out, in a format agit can't show: {}",
            plural(n),
            kinds
                .iter()
                .map(|(kind, n)| format!("{n} {kind}"))
                .collect::<Vec<_>>()
                .join(", ")
        )?,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::privacy_publication::ProjectionNotice;

    fn report_of(left_out: &LeftOut, omissions: &[(usize, &str)]) -> String {
        let report = ProjectionReport {
            records: 40,
            omissions: omissions
                .iter()
                .map(|(record, reason)| ProjectionNotice {
                    record: *record,
                    reason: (*reason).into(),
                })
                .collect(),
            ..Default::default()
        };
        let mut output = Vec::new();
        write_report(&mut output, &report, left_out).unwrap();
        String::from_utf8(output).unwrap()
    }

    /// Bookkeeping is one line of kinds and counts, without a line per record; a record of the
    /// conversation keeps its own line with its kind, and the summary says whether there is one.
    #[test]
    fn left_out_records_are_counted_by_kind() {
        let native = Omission::NativeRecord.reason();
        let kinds = ["a", "b", "c", "d", "e", "f", "g", "h"];
        let left_out = LeftOut {
            bookkeeping: kinds
                .iter()
                .enumerate()
                .map(|(i, kind)| (format!("kind-{kind}"), 9 - i))
                .collect(),
            runtimes: ["claude-code".to_owned()].into(),
            conversation: Default::default(),
        };
        let omissions = (0..30).map(|record| (record, native)).collect::<Vec<_>>();
        let mut with_policy = omissions.clone();
        with_policy.push((31, "file content is excluded by policy"));
        let text = report_of(&left_out, &with_policy);
        assert!(
            text.contains(
                "  44 Claude Code bookkeeping records left out, not part of the conversation: \
                 9 kind-a, 8 kind-b, 7 kind-c, 6 kind-d, 5 kind-e, 4 kind-f, 5 of 2 other kinds\n  \
                 no record of the conversation itself is left out\n"
            ),
            "{text}"
        );
        assert!(!text.contains("Source LOG record 1:"), "{text}");
        assert!(
            text.contains(
                "  Source LOG record 32: omitted from public content\n    | file content"
            ),
            "{text}"
        );

        // Seven kinds are all shown; a second runtime is named as the runtime.
        let seven = LeftOut {
            bookkeeping: left_out.bookkeeping[..7].to_vec(),
            runtimes: ["claude-code".to_owned(), "codex".to_owned()].into(),
            ..Default::default()
        };
        let text = report_of(&seven, &omissions);
        assert!(
            text.contains(
                "  42 runtime bookkeeping records left out, not part of the conversation: \
                 9 kind-a, 8 kind-b, 7 kind-c, 6 kind-d, 5 kind-e, 4 kind-f, 3 kind-g\n"
            ),
            "{text}"
        );

        let conversation = LeftOut {
            conversation: [(4, "assistant".to_owned()), (6, "assistant".to_owned())].into(),
            ..Default::default()
        };
        let text = report_of(&conversation, &omissions);
        assert!(
            text.contains(
                "  2 records of the conversation left out, in a format agit can't show: 2 assistant\n"
            ),
            "{text}"
        );
        assert!(!text.contains("bookkeeping"), "{text}");
        assert!(
            text.contains(&format!(
                "  Source LOG record 5: omitted from public content\n    | {native} (assistant), a record of the conversation\n"
            )),
            "{text}"
        );
        assert_eq!(text.matches("Source LOG record").count(), 2, "{text}");
    }
}
