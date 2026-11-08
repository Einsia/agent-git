//! This device's record of each message agit's plugin handed to a running Claude Code turn with
//! a tool step's result.
//!
//! Claude Code keeps such a message only as a hook's context, and that record does not name the
//! plugin that wrote it: any other plugin could write the same text. A context therefore counts as
//! the person's words only where a record here says this device put exactly that text into that
//! conversation, and each record vouches for one context.
//!
//! A record holds no text: per conversation, the message's id and a hash of the exact text handed
//! over. It is written before the plugin is given the text, so it already exists when Claude Code
//! writes the context and a viewer's live transcript reads it, and it is removed again when the
//! message doesn't go with a tool step after all. It lives in an owner-only file under the
//! account's AGIT_HOME state, outside every capture path, and is never committed, uploaded,
//! previewed or logged.
//!
//! A record lasts as long as the conversation's local transcript, because publication reads that
//! history again at every push: the daemon's start removes the records of conversations whose
//! transcript is gone, and logout or a sign-in under another account removes them all. A record
//! that is missing, unreadable or for another conversation vouches for nothing: losing one only
//! ever withholds words, never publishes something extra.

use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    path::PathBuf,
};

/// How a message for the running turn is framed for the model: the way Claude Code frames the
/// messages a person types while a turn runs.
pub const FRAMING: &str = "The user sent a new message while you were working:\n";

const NAMESPACE: &str = "step-deliveries";

/// The text the plugin hands to the model for `message`.
pub fn framed(message: &str) -> String {
    format!("{FRAMING}{message}")
}

/// The person's words in a text framed by [`framed`].
pub fn words(attached: &str) -> Option<&str> {
    attached
        .strip_prefix(FRAMING)
        .filter(|words| !words.trim().is_empty())
}

fn digest(attached: &str) -> String {
    hex::encode(Sha256::digest(attached.as_bytes()))
}

/// A conversation id is a file name here only when it is a plain id.
fn file(directory: PathBuf, session: &str) -> Option<PathBuf> {
    (!session.is_empty()
        && session.len() <= 128
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    .then(|| directory.join(session))
}

/// Records that the plugin handed `attached` to conversation `session`'s model for the message
/// `id`.
pub fn record(session: &str, id: &str, attached: &str) -> crate::Result<()> {
    let path = file(super::state_dir(NAMESPACE)?, session)
        .ok_or_else(|| anyhow::anyhow!("the conversation id can't name a delivery record"))?;
    let line = serde_json::json!({"id": id, "digest": digest(attached)});
    let mut file = crate::infra::config::state_file_options()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(format!("{line}\n").as_bytes())?;
    file.sync_data()?;
    Ok(())
}

/// Removes the record of message `id` in conversation `session`: it didn't go with a tool step.
pub fn forget_one(session: &str, id: &str) -> crate::Result<()> {
    let Some(path) = file(super::namespace_dir(NAMESPACE)?, session) else {
        return Ok(());
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let kept: String = text
        .lines()
        .filter(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .map_or(true, |record| record["id"].as_str() != Some(id))
        })
        .map(|line| format!("{line}\n"))
        .collect();
    if kept.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    let directory = path.parent().unwrap_or(std::path::Path::new("."));
    let mut temporary = tempfile::Builder::new()
        .prefix(".deliveries-")
        .tempfile_in(directory)?;
    temporary.write_all(kept.as_bytes())?;
    temporary.as_file().sync_data()?;
    temporary.persist(&path).map_err(|error| error.error)?;
    Ok(())
}

/// What one conversation's records vouch for, each record once: the hashes still unspent, with
/// the ids that vouch for them.
#[derive(Default)]
pub struct Deliveries {
    unspent: HashMap<String, Vec<String>>,
}

impl Deliveries {
    /// The records of conversation `session`. Missing or unreadable records vouch for nothing.
    pub fn load(session: &str) -> Self {
        let Some(path) = super::namespace_dir(NAMESPACE)
            .ok()
            .and_then(|directory| file(directory, session))
        else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let mut seen = HashSet::new();
        let mut unspent: HashMap<String, Vec<String>> = HashMap::new();
        for line in text.lines() {
            let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let (Some(id), Some(digest)) = (record["id"].as_str(), record["digest"].as_str())
            else {
                continue;
            };
            // One message vouches once, however often its record was written.
            if seen.insert(id.to_owned()) {
                unspent
                    .entry(digest.to_owned())
                    .or_default()
                    .push(id.to_owned());
            }
        }
        Self { unspent }
    }

    /// The person's words in `attached`, if a record not yet spent vouches for exactly that text;
    /// the record is spent.
    pub fn take<'a>(&mut self, attached: &'a str) -> Option<&'a str> {
        let words = words(attached)?;
        let ids = self.unspent.get_mut(&digest(attached))?;
        ids.pop()?;
        Some(words)
    }
}

/// The records of every conversation one pass over a history reads, loaded as each first comes
/// up, so each record vouches once in that pass.
#[derive(Default)]
pub struct Conversations(HashMap<String, Deliveries>);

impl Conversations {
    /// The owner's words in `record` when it is a tool step's context that this device's records
    /// vouch for; empty for anything else.
    pub fn words(&mut self, record: &serde_json::Value) -> Vec<String> {
        let attachment = &record["attachment"];
        if record["type"] != "attachment"
            || attachment["type"] != "hook_additional_context"
            || attachment["hookName"] != "tool.call"
        {
            return Vec::new();
        }
        let Some(session) = record["sessionId"].as_str() else {
            return Vec::new();
        };
        let deliveries = self
            .0
            .entry(session.to_owned())
            .or_insert_with(|| Deliveries::load(session));
        attachment["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .filter_map(|text| deliveries.take(text).map(str::to_owned))
            .collect()
    }

    /// `record` as Claude Code records a message typed while a turn runs, when it is a tool step's
    /// context these records vouch for: publication then carries the words through the same
    /// rules as a typed message. `None` for anything else, which publication leaves as it is.
    pub fn as_typed(&mut self, record: &serde_json::Value) -> Option<serde_json::Value> {
        let words = self.words(record);
        if words.is_empty() {
            return None;
        }
        let mut typed = serde_json::json!({
            "type": "queue-operation",
            "operation": "remove",
            "reason": "absorbed_mid_turn",
            "content": words.join("\n\n"),
        });
        for field in ["timestamp", "sessionId", "uuid", "parentUuid"] {
            if let Some(value) = record.get(field) {
                typed[field] = value.clone();
            }
        }
        Some(typed)
    }
}

/// For each of a history's records, in order, the typed message it is published as when it is a
/// Claude Code tool step's context that this device's records vouch for, else `None`.
pub fn typed_records<'a>(
    records: impl IntoIterator<Item = (&'a str, &'a serde_json::Value)>,
) -> Vec<Option<serde_json::Value>> {
    let mut conversations = Conversations::default();
    records
        .into_iter()
        .map(|(source, record)| {
            (source == "claude-code")
                .then(|| conversations.as_typed(record))
                .flatten()
        })
        .collect()
}

/// Removes every record: the account they were made under is leaving this device.
pub fn forget_all() {
    if let Ok(directory) = super::namespace_dir(NAMESPACE) {
        let _ = std::fs::remove_dir_all(directory);
    }
}

/// Removes the records of conversations that no longer exist on this device.
pub fn sweep(exists: impl Fn(&str) -> bool) {
    let Ok(entries) = super::namespace_dir(NAMESPACE)
        .and_then(|directory| std::fs::read_dir(directory).map_err(Into::into))
    else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let gone = name.to_str().is_none_or(|session| !exists(session));
        if gone {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const CONVERSATION: &str = "4e6c2b1a-0d0e-4c43-9a51-8f0e2f4a7b10";

    fn context(uuid: &str, conversation: &str, text: &str) -> Value {
        json!({
            "type": "attachment",
            "uuid": uuid,
            "parentUuid": null,
            "sessionId": conversation,
            "timestamp": "2026-11-06T08:00:00.000Z",
            "attachment": {
                "type": "hook_additional_context",
                "hookName": "tool.call",
                "hookEvent": "PostToolUse",
                "toolUseID": "toolu_1-context",
                "content": [text],
            },
        })
    }

    fn published(records: &[Value]) -> Vec<Option<Value>> {
        let typed = typed_records(records.iter().map(|record| ("claude-code", record)));
        crate::domain::publication_records::published_sequence(
            records
                .iter()
                .zip(&typed)
                .map(|(record, typed)| ("claude-code", typed.as_ref().unwrap_or(record))),
        )
    }

    /// A tool step's context publishes as the person's typed words only where a record vouches
    /// for exactly that text in that conversation, and each record once. Unvouched text, a second
    /// identical context, text vouched for in another conversation, a message whose record was
    /// removed because it didn't go with a step, and anything after every record is removed stay
    /// context and are withheld. Matching the framing alone would publish
    /// whatever another plugin writes in the same words; matching without spending a record
    /// would publish every echo of the owner's message.
    #[test]
    fn only_a_vouched_context_publishes_as_the_typed_words_and_only_once() {
        let home = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(home.path(), || {
            let sent = framed("also update the README");
            record(CONVERSATION, "client-1", &sent).unwrap();
            let records = [
                context("u1", CONVERSATION, &sent),
                context("u2", CONVERSATION, &sent),
                context("u3", CONVERSATION, &framed("words nobody sent")),
                context("u4", "5f7d3c2b-1e1f-4d54-8b62-9f1a3a5b8c21", &sent),
            ];
            let out = published(&records);
            let first = out[0].as_ref().expect("the vouched context publishes");
            assert_eq!(first["type"], "queue-operation");
            assert_eq!(first["reason"], "absorbed_mid_turn");
            assert_eq!(first["content"], "also update the README");
            assert!(
                out[1..].iter().all(Option::is_none),
                "a second identical context, unvouched text and another conversation's text \
                 stay withheld: {out:?}"
            );

            let mut local = Conversations::default();
            assert_eq!(local.words(&records[0]), ["also update the README"]);

            // A message that didn't go with a step loses its record; the other's stays.
            let other = framed("and the CHANGELOG");
            record(CONVERSATION, "client-2", &other).unwrap();
            forget_one(CONVERSATION, "client-2").unwrap();
            let after = published(&[
                context("u5", CONVERSATION, &other),
                context("u6", CONVERSATION, &sent),
            ]);
            assert!(after[0].is_none(), "a removed record vouches for nothing");
            assert!(after[1].is_some(), "removing one record keeps the others");

            forget_all();
            assert!(published(&records).iter().all(Option::is_none));
        });
    }
}
