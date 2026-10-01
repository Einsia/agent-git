//! Semantic projection uses durable mappings and keeps valid placeholders opaque.

use super::crypto::UserKey;
use super::dictionary::{Dictionary, Origin, Record, token_identity};
use super::policy::{Snapshot, Source};
use super::storage::Store;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_UNIT_BYTES: usize = 8 * 1024 * 1024;
const MAX_FINDINGS: usize = 1024;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Complete,
    Partial,
    Skipped,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    ProtectText,
    ProtectStream,
    ProtectJsonl,
    ProtectEnvelopes,
    ProtectNative,
    HydrateText,
    HydrateJsonl,
    HydrateEnvelopes,
    Manage,
    ProjectPublication,
}

#[derive(Serialize, Deserialize)]
pub struct Outcome {
    pub content: String,
    pub status: Status,
    pub replacements: usize,
    pub unresolved: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed: Option<usize>,
}

impl Outcome {
    pub fn skipped(text: &str) -> Self {
        Self {
            content: text.to_owned(),
            status: Status::Skipped,
            replacements: 0,
            unresolved: 0,
            consumed: None,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct NativeBatch {
    pub runtime: String,
    pub session: String,
    pub records: Vec<Value>,
    pub pointers: Vec<Vec<String>>,
}

pub struct Projector {
    pub store: Store,
    pub dictionary: Dictionary,
    pub policy: Snapshot,
    pub key: Option<UserKey>,
    pub complete: bool,
}

/// Recognition validates bounded token syntax before treating any text as opaque.
pub fn tokens(text: &str) -> impl Iterator<Item = (usize, usize, &str)> {
    text.match_indices("{{AGIT_SECRET_V")
        .filter_map(|(start, _)| {
            let tail = &text[start..];
            let end = tail
                .as_bytes()
                .windows(2)
                .take(128)
                .position(|bytes| bytes == b"}}")?
                + 2;
            let token = &tail[..end];
            token_identity(token).map(|_| (start, start + end, token))
        })
}

impl Projector {
    pub fn transform(&mut self, text: &str, mode: Mode) -> Outcome {
        if text.len() > MAX_UNIT_BYTES {
            return Outcome::skipped(text);
        }
        if matches!(mode, Mode::ProtectNative) {
            return self.native_batch(text);
        }
        if matches!(mode, Mode::ProtectStream) {
            return self.stream(text);
        }
        let hydrate = matches!(
            mode,
            Mode::HydrateText | Mode::HydrateJsonl | Mode::HydrateEnvelopes
        );
        if matches!(mode, Mode::ProtectText | Mode::HydrateText) {
            return if hydrate {
                self.hydrate(text)
            } else {
                self.protect(text)
            };
        }
        let mut outcome = Outcome {
            content: String::with_capacity(text.len()),
            status: Status::Complete,
            replacements: 0,
            unresolved: 0,
            consumed: None,
        };
        for line in text.split_inclusive('\n') {
            let body = line.strip_suffix('\n').unwrap_or(line);
            let next = if matches!(mode, Mode::HydrateEnvelopes | Mode::ProtectEnvelopes) {
                self.envelope(line, hydrate)
            } else if let Ok(mut value) = serde_json::from_str::<Value>(body) {
                let mut next = Outcome {
                    content: String::new(),
                    status: Status::Complete,
                    replacements: 0,
                    unresolved: 0,
                    consumed: None,
                };
                self.value(&mut value, hydrate, &mut next, None, true);
                next.content = if next.replacements > 0 {
                    serde_json::to_string(&value).unwrap_or_else(|_| body.to_owned())
                } else {
                    body.to_owned()
                };
                next
            } else if hydrate {
                self.hydrate(body)
            } else {
                self.protect(body)
            };
            outcome.content.push_str(&next.content);
            if line.ends_with('\n') {
                outcome.content.push('\n');
            }
            outcome.replacements += next.replacements;
            outcome.unresolved += next.unresolved;
            if next.status != Status::Complete {
                outcome.status = Status::Partial;
            }
        }
        outcome
    }

    fn native_batch(&mut self, text: &str) -> Outcome {
        let Ok(mut batch) = serde_json::from_str::<NativeBatch>(text) else {
            return Outcome::skipped(text);
        };
        if batch.records.len() != batch.pointers.len() {
            return Outcome::skipped(text);
        }
        let mut outcome = Outcome {
            content: String::new(),
            status: Status::Complete,
            replacements: 0,
            unresolved: 0,
            consumed: None,
        };
        for (value, pointers) in batch.records.iter_mut().zip(batch.pointers) {
            let mut mask = crate::domain::secrets::identity::native_record_mask(
                &batch.runtime,
                &batch.session,
                value,
            );
            for pointer in pointers {
                if let Some(text) = value.pointer(&pointer).and_then(Value::as_str) {
                    mask.0.push((pointer, 0..text.len()));
                }
            }
            let mut retained = Vec::new();
            for (pointer, span) in mask.0 {
                if let Some(field) = value.pointer_mut(&pointer)
                    && let Some(text) = field.as_str()
                    && span == (0..text.len())
                    && !self.policy.explicitly_blocks(text)
                {
                    retained.push((pointer, field.take()));
                }
            }
            self.value(value, false, &mut outcome, None, true);
            for (pointer, original) in retained {
                if let Some(field) = value.pointer_mut(&pointer) {
                    *field = original;
                }
            }
        }
        outcome.content = serde_json::to_string(&batch.records).unwrap_or_else(|_| text.to_owned());
        outcome
    }

    fn stream(&mut self, text: &str) -> Outcome {
        const LIMIT: usize = 128 * 1024;
        if text.len() >= LIMIT {
            let mut outcome = self.protect(text);
            outcome.consumed = Some(text.len());
            outcome.status = Status::Partial;
            return outcome;
        }
        // Keep literal lookahead and assignment context. Incomplete key blocks wait for a terminator.
        let hold = self.policy.max_literal_bytes.clamp(128, LIMIT);
        let mut boundary = text.len().saturating_sub(hold);
        while !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        let mut cut = text[..boundary].rfind(char::is_whitespace).unwrap_or(0);
        if let Some(start) = text.rfind("-----BEGIN ")
            && !text[start..].contains("-----END ")
        {
            cut = cut.min(start);
        }
        let batch = self.policy.scan(text, MAX_FINDINGS);
        for hit in batch.findings {
            if hit.start < cut && hit.end > cut {
                cut = hit.start;
            }
        }
        for (start, end, _) in tokens(text) {
            if start < cut && end > cut {
                cut = start;
            }
        }
        let mut outcome = self.protect(&text[..cut]);
        outcome.consumed = Some(cut);
        outcome
    }

    fn envelope(&mut self, line: &str, hydrate: bool) -> Outcome {
        let Ok(mut envelope) = crate::domain::storage::parse_envelope_line(line) else {
            return Outcome::skipped(line.strip_suffix('\n').unwrap_or(line));
        };
        let mut outcome = Outcome {
            content: String::new(),
            status: Status::Complete,
            replacements: 0,
            unresolved: 0,
            consumed: None,
        };
        self.value(&mut envelope.content, hydrate, &mut outcome, None, true);
        envelope.object_hash = crate::domain::transcript::object_hash(&envelope.content);
        outcome.content = crate::domain::storage::envelope_line(&envelope)
            .trim_end_matches('\n')
            .to_owned();
        outcome
    }

    fn value(
        &mut self,
        value: &mut Value,
        hydrate: bool,
        outcome: &mut Outcome,
        field: Option<&str>,
        protocol_root: bool,
    ) {
        match value {
            Value::String(text) => {
                let next = if hydrate {
                    self.hydrate(text)
                } else {
                    self.protect_field(text, field)
                };
                *text = next.content;
                outcome.replacements += next.replacements;
                outcome.unresolved += next.unresolved;
                if next.status != Status::Complete {
                    outcome.status = Status::Partial;
                }
            }
            Value::Array(values) => {
                for value in values {
                    self.value(value, hydrate, outcome, None, false);
                }
            }
            Value::Object(values) => {
                // Only carrier fields have identity semantics. Nested tool data remains content.
                for (field, value) in values {
                    if protocol_root
                        && !hydrate
                        && value
                            .as_str()
                            .is_none_or(|text| !self.policy.explicitly_blocks(text))
                        && matches!(
                            field.as_str(),
                            "_session_id"
                                | "_source"
                                | "id"
                                | "type"
                                | "_object_hash"
                                | "session_id"
                                | "sessionId"
                                | "event_id"
                                | "uuid"
                                | "parentUuid"
                                | "object_hash"
                                | "commit_id"
                                | "tree_id"
                                | "blob_id"
                                | "ref_name"
                                | "schema_version"
                                | "layout_version"
                                | "projection_version"
                                | "created_at"
                                | "updated_at"
                                | "timestamp"
                                | "provenance"
                        )
                    {
                        continue;
                    }
                    self.value(value, hydrate, outcome, Some(field), false);
                }
            }
            _ => {}
        }
    }

    fn protect(&mut self, text: &str) -> Outcome {
        self.protect_field(text, None)
    }

    fn protect_field(&mut self, text: &str, field: Option<&str>) -> Outcome {
        // Decoded values keep assignment context without putting JSON escape bytes in a record.
        let prefix = field
            .filter(|field| field.len() <= 128 && field.is_ascii())
            .map(|field| format!("{field} = "))
            .unwrap_or_default();
        let input = if prefix.is_empty() {
            std::borrow::Cow::Borrowed(text)
        } else {
            std::borrow::Cow::Owned(format!("{prefix}{text}"))
        };
        let mut batch = self.policy.scan(&input, MAX_FINDINGS);
        batch.findings.retain_mut(|hit| {
            if hit.start < prefix.len() {
                return false;
            }
            hit.start -= prefix.len();
            hit.end -= prefix.len();
            true
        });
        let mut complete = self.complete && batch.complete;
        let opaque: Vec<_> = tokens(text).map(|(start, end, _)| (start, end)).collect();
        let findings: Vec<_> = batch
            .findings
            .into_iter()
            .filter(|hit| {
                !opaque
                    .iter()
                    .any(|(start, end)| hit.start < *end && hit.end > *start)
            })
            .collect();
        let mut new = Vec::<Record>::new();
        let mut seen = std::collections::HashSet::new();
        for finding in &findings {
            let Some(original) = text.get(finding.start..finding.end) else {
                complete = false;
                continue;
            };
            if self.dictionary.for_value(original).is_none() && seen.insert(original) {
                let origin = match finding.source {
                    Source::Default => Origin::Heuristic,
                    Source::GlobalUser => Origin::GlobalUser,
                    Source::RepositoryUser => Origin::RepositoryUser,
                };
                match Record::new(&self.store.dictionary_id, original, origin) {
                    Ok(record) => new.push(record),
                    Err(_) => complete = false,
                }
            }
        }
        if !new.is_empty() {
            if self.store.append(&new, self.key.as_ref()).is_ok() {
                for record in new {
                    complete &= self.dictionary.accept(record).is_ok();
                }
            } else {
                complete = false;
            }
        }
        let mut outcome = Outcome {
            content: String::with_capacity(text.len()),
            status: Status::Complete,
            replacements: 0,
            unresolved: 0,
            consumed: None,
        };
        let mut cursor = 0;
        for finding in findings {
            let Some(original) = text.get(finding.start..finding.end) else {
                continue;
            };
            if let Some(record) = self.dictionary.for_value(original) {
                outcome.content.push_str(&text[cursor..finding.start]);
                outcome.content.push_str(&record.token);
                outcome.replacements += 1;
                cursor = finding.end;
            }
        }
        outcome.content.push_str(&text[cursor..]);
        if !complete {
            outcome.status = Status::Partial;
        }
        outcome
    }

    fn hydrate(&self, text: &str) -> Outcome {
        let mut outcome = Outcome {
            content: String::with_capacity(text.len()),
            status: Status::Complete,
            replacements: 0,
            unresolved: 0,
            consumed: None,
        };
        let mut cursor = 0;
        for (start, end, token) in tokens(text) {
            if let Some(record) = self.dictionary.get(token).filter(|record| {
                outcome.content.len() + start - cursor + record.original.len() + text.len() - end
                    <= MAX_UNIT_BYTES
            }) {
                outcome.content.push_str(&text[cursor..start]);
                outcome.content.push_str(&record.original);
                outcome.replacements += 1;
                cursor = end;
            } else {
                outcome.unresolved += 1;
            }
        }
        outcome.content.push_str(&text[cursor..]);
        if outcome.unresolved > 0 || !self.complete {
            outcome.status = Status::Partial;
        }
        outcome
    }
}
