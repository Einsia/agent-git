//! Archive publications can end between Codex's two representations of a request. Activity
//! credits the first saved copy, not the later publication containing its mirror or reply.

use super::*;
use crate::adapter::{Event, Session};

enum Record {
    Boundary,
    Prompt { event: bool, text: String },
}

fn record(envelope: &Envelope, session: &str) -> Option<Record> {
    if envelope.source != "codex" || envelope.session_id != session {
        return None;
    }
    let content = &envelope.content;
    if content["type"] == "session_meta" {
        return Some(Record::Boundary);
    }
    let payload = &content["payload"];
    let (event, kind, text) = match (content["type"].as_str()?, payload["type"].as_str()?) {
        ("event_msg", "task_complete" | "task_started" | "turn_aborted") => {
            return Some(Record::Boundary);
        }
        ("event_msg", "user_message") => {
            let text = payload["message"].as_str()?;
            if text.trim().is_empty() {
                return None;
            }
            let (kind, text) = crate::adapter::codex::classify_message("user", text.to_owned());
            (true, kind, text)
        }
        ("response_item", "message") if payload["role"] == "user" => {
            let (kind, text) = crate::adapter::codex::classify_response_message_for(payload, None)?;
            (false, kind, text)
        }
        _ => return None,
    };
    (kind == EventKind::UserPrompt).then_some(Record::Prompt { event, text })
}

impl<S: Store> Extractor<'_, S> {
    pub(super) fn archive_prompts(
        &mut self,
        candidate: &Candidate,
        sequence: &Sequence,
        offset: usize,
        envelopes: &[Envelope],
        session: &mut Session,
    ) -> Result<()> {
        let logical = &candidate.meta.session;
        let records: Vec<_> = envelopes
            .iter()
            .enumerate()
            .filter_map(|(line, envelope)| Some((line, record(envelope, logical)?)))
            .collect();
        // Equal requests from the same representation are distinct. Only a matching copy
        // from the other representation consumes an outstanding request in this execution.
        let mut balance = HashMap::<String, i64>::new();
        if matches!(records.first(), Some((_, Record::Prompt { .. }))) && offset != 0 {
            self.prior_prompt_balance(candidate, sequence, offset, &mut balance)?;
        }
        let mut prompt_lines = std::collections::HashSet::new();
        let mut prompts = Vec::new();
        for (line, record) in records {
            match record {
                Record::Boundary => balance.clear(),
                Record::Prompt { event, text } => {
                    prompt_lines.insert(line);
                    let copies = balance.entry(text.clone()).or_default();
                    let new = if event { *copies >= 0 } else { *copies <= 0 };
                    *copies += if event { 1 } else { -1 };
                    if new {
                        prompts.push(Event::text(EventKind::UserPrompt, text, None).at_line(line));
                    }
                }
            }
        }
        if !prompt_lines.is_empty() {
            session.events.retain(|event| {
                event.kind != EventKind::UserPrompt
                    || !event.line.is_some_and(|line| prompt_lines.contains(&line))
            });
            session.events.extend(prompts);
            session.events.sort_by_key(|event| event.line);
        }
        Ok(())
    }

    fn prior_prompt_balance(
        &mut self,
        candidate: &Candidate,
        sequence: &Sequence,
        mut before: usize,
        balance: &mut HashMap<String, i64>,
    ) -> Result<()> {
        let mut bytes = 0;
        while before > 0 {
            let mut ids: Vec<_> = sequence.ids(before.saturating_sub(32), before).collect();
            ids.reverse();
            let (raw, count) = self.events(&candidate.key.commit, &ids, true)?;
            bytes += raw.len();
            ensure!(
                bytes <= MAX_CONTEXT_BYTES,
                "Codex archive execution context exceeds the read budget"
            );
            for envelope in authenticate(&raw, Some(&ids[..count]))? {
                match record(&envelope, &candidate.meta.session) {
                    Some(Record::Boundary) => return Ok(()),
                    Some(Record::Prompt { event, text }) => {
                        *balance.entry(text).or_default() += if event { 1 } else { -1 };
                    }
                    None => {}
                }
            }
            before -= count;
        }
        Ok(())
    }
}
