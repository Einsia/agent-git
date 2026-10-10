//! Semantic projection uses durable mappings and keeps valid placeholders opaque.

use super::crypto::UserKey;
use super::dictionary::{Dictionary, Origin, Record, token_identity};
use super::policy::{Batch, Snapshot, Source};
use super::storage::Store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};
use std::ops::Range;

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

fn whole_token(text: &str) -> bool {
    tokens(text)
        .next()
        .is_some_and(|(start, end, _)| start == 0 && end == text.len())
}

fn has_wrapped_carrier(value: &Value) -> bool {
    crate::adapter::protocol_ids::layout(value)
        .encoded
        .iter()
        .any(|(pointer, _)| {
            value
                .pointer(pointer)
                .and_then(Value::as_str)
                .is_some_and(whole_token)
        })
}

struct EncodedField {
    range: Range<usize>,
    escapes: Vec<Range<usize>>,
}

fn encoded_fields(text: &str, value: &Value) -> crate::Result<Vec<EncodedField>> {
    fn collect(
        raw: &RawValue,
        value: &Value,
        base: usize,
        fields: &mut Vec<EncodedField>,
    ) -> crate::Result<()> {
        match value {
            Value::String(_) => {
                let start = raw.get().as_ptr() as usize - base;
                let bytes = raw.get().as_bytes();
                let mut escapes = Vec::new();
                let mut cursor = 1;
                while cursor < bytes.len() - 1 {
                    if bytes[cursor] == b'\\' {
                        let end = cursor + if bytes[cursor + 1] == b'u' { 6 } else { 2 };
                        escapes.push(start + cursor..start + end);
                        cursor = end;
                    } else {
                        cursor += 1;
                    }
                }
                fields.push(EncodedField {
                    range: start..start + raw.get().len(),
                    escapes,
                });
            }
            Value::Array(values) => {
                let raw: Vec<&RawValue> = serde_json::from_str(raw.get())?;
                for (raw, value) in raw.into_iter().zip(values) {
                    collect(raw, value, base, fields)?;
                }
            }
            Value::Object(values) => {
                let raw: std::collections::BTreeMap<String, &RawValue> =
                    serde_json::from_str(raw.get())?;
                for (key, value) in values {
                    collect(raw[key], value, base, fields)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let raw = serde_json::from_str::<&RawValue>(text)?;
    let mut fields = Vec::new();
    collect(raw, value, text.as_ptr() as usize, &mut fields)?;
    Ok(fields)
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
        let preserve_protocol_ids = hydrate
            && text.split_inclusive('\n').any(|line| {
                if matches!(mode, Mode::HydrateEnvelopes) {
                    crate::domain::storage::parse_envelope_line(line)
                        .map(|envelope| has_wrapped_carrier(&envelope.content))
                        .unwrap_or(false)
                } else {
                    serde_json::from_str::<Value>(line)
                        .map(|value| has_wrapped_carrier(&value))
                        .unwrap_or(false)
                }
            });
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
                self.envelope(line, hydrate, preserve_protocol_ids)
            } else if let Ok(mut value) = serde_json::from_str::<Value>(body) {
                let mut next = Outcome {
                    content: String::new(),
                    status: Status::Complete,
                    replacements: 0,
                    unresolved: 0,
                    consumed: None,
                };
                self.record(&mut value, hydrate, &mut next, preserve_protocol_ids);
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
            self.record(value, false, &mut outcome, false);
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

    fn envelope(&mut self, line: &str, hydrate: bool, preserve_protocol_ids: bool) -> Outcome {
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
        self.record(
            &mut envelope.content,
            hydrate,
            &mut outcome,
            preserve_protocol_ids,
        );
        envelope.object_hash = crate::domain::transcript::object_hash(&envelope.content);
        outcome.content = crate::domain::storage::envelope_line(&envelope)
            .trim_end_matches('\n')
            .to_owned();
        outcome
    }

    fn record(
        &mut self,
        value: &mut Value,
        hydrate: bool,
        outcome: &mut Outcome,
        preserve_protocol_ids: bool,
    ) {
        let layout = crate::adapter::protocol_ids::layout(value);
        self.schema_value(
            value,
            &layout,
            outcome,
            true,
            hydrate,
            preserve_protocol_ids,
        );
    }

    fn schema_value(
        &mut self,
        value: &mut Value,
        layout: &crate::adapter::protocol_ids::Layout,
        outcome: &mut Outcome,
        protocol_root: bool,
        hydrate: bool,
        preserve_protocol_ids: bool,
    ) {
        let mut retained = Vec::new();
        for (pointer, nested) in &layout.encoded {
            let Some(mut text) = value
                .pointer(pointer)
                .and_then(Value::as_str)
                .map(str::to_owned)
            else {
                continue;
            };
            let wrapped = hydrate && whole_token(&text);
            if wrapped {
                // Unwrap only the carrier token before parsing. Its field tokens need
                // decoded string boundaries to preserve quotes, backslashes and newlines.
                let next = self.hydrate_carrier(&text);
                outcome.replacements += next.replacements;
                outcome.unresolved += next.unresolved;
                if next.status != Status::Complete {
                    outcome.status = Status::Partial;
                }
                text = next.content;
                if let Some(field) = value.pointer_mut(pointer) {
                    *field = Value::String(text.clone());
                }
            }
            let Ok(mut decoded) = serde_json::from_str::<Value>(&text) else {
                if wrapped && let Some(field) = value.pointer_mut(pointer) {
                    retained.push((pointer.as_str(), field.take()));
                }
                continue;
            };
            if !hydrate && let Some(batch) = self.scan_encoded(&text, &decoded) {
                let next = self.protect_batch(&text, batch);
                outcome.replacements += next.replacements;
                outcome.unresolved += next.unresolved;
                if next.status != Status::Complete {
                    outcome.status = Status::Partial;
                }
                if let Some(field) = value.pointer_mut(pointer) {
                    *field = Value::String(next.content);
                    retained.push((pointer.as_str(), field.take()));
                }
                continue;
            }
            let expanded_layout =
                if hydrate && nested.fields.is_empty() && nested.encoded.is_empty() {
                    crate::adapter::protocol_ids::layout(value)
                        .encoded
                        .into_iter()
                        .find(|(candidate, _)| candidate == pointer)
                        .map(|(_, layout)| layout)
                } else {
                    None
                };
            let nested = expanded_layout.as_ref().unwrap_or(nested);
            let replacements = outcome.replacements;
            self.schema_value(
                &mut decoded,
                nested,
                outcome,
                false,
                hydrate,
                preserve_protocol_ids,
            );
            if outcome.replacements != replacements
                && let Some(field) = value.pointer_mut(pointer)
            {
                *field = Value::String(decoded.to_string());
            }
            if let Some(field) = value.pointer_mut(pointer) {
                retained.push((pointer.as_str(), field.take()));
            }
        }
        if hydrate && preserve_protocol_ids {
            for identity in &layout.fields {
                let token = value
                    .pointer(&identity.pointer)
                    .and_then(Value::as_str)
                    .filter(|text| token_identity(text).is_some())
                    .map(str::to_owned);
                if let Some(token) = token {
                    if let Some(field) = value.pointer_mut(&identity.pointer) {
                        *field = Value::Null;
                    }
                    retained.push((identity.pointer.as_str(), Value::String(token)));
                }
            }
        }
        for identity in layout.fields.iter().filter(|_| !hydrate) {
            if let Some(field) = value.pointer_mut(&identity.pointer)
                && let Some(text) = field.as_str()
            {
                let next = self.protect_field(text, identity.pointer.rsplit('/').next(), true);
                outcome.replacements += next.replacements;
                outcome.unresolved += next.unresolved;
                if next.status != Status::Complete {
                    outcome.status = Status::Partial;
                }
                *field = Value::Null;
                retained.push((identity.pointer.as_str(), Value::String(next.content)));
            }
        }
        self.value(value, hydrate, outcome, None, protocol_root);
        for (pointer, original) in retained {
            if let Some(field) = value.pointer_mut(pointer) {
                *field = original;
            }
        }
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
                    self.protect_field(text, field, false)
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
        self.protect_field(text, None, false)
    }

    fn scan_encoded(&self, text: &str, decoded: &Value) -> Option<Batch> {
        let mut batch = self.policy.explicit_findings(text, MAX_FINDINGS);
        if batch.findings.is_empty() && batch.complete {
            return None;
        }
        let mut fields = match encoded_fields(text, decoded) {
            Ok(fields) => fields,
            Err(_) => {
                batch.complete = false;
                Vec::new()
            }
        };
        fields.sort_unstable_by_key(|field| field.range.start);
        let structural = batch.findings.iter().position(|hit| {
            let preceding = fields.partition_point(|field| field.range.start < hit.start);
            !preceding.checked_sub(1).is_some_and(|index| {
                let field = &fields[index];
                let escape = field
                    .escapes
                    .partition_point(|escape| escape.end <= hit.start);
                hit.end < field.range.end
                    && field
                        .escapes
                        .get(escape)
                        .is_none_or(|escape| escape.start >= hit.end)
            })
        });
        if structural.is_none() && batch.complete {
            return None;
        }
        // A fragment crossing syntax makes the carrier opaque as a unit. Partial replacement
        // loses schema evidence on later projections and cannot preserve JSON escapes on hydration.
        let mut hit = batch.findings.swap_remove(structural.unwrap_or(0));
        hit.start = 0;
        hit.end = text.len();
        batch.findings = vec![hit];
        Some(batch)
    }

    fn protect_field(&mut self, text: &str, field: Option<&str>, identity: bool) -> Outcome {
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
            // A typed protocol identifier supplies evidence against randomness alone. Credential
            // signatures and explicit blocks still apply even inside a native identifier field.
            if identity
                && crate::adapter::protocol_ids::identifier_shape(text)
                && hit.source == Source::Default
                && hit.rule == "high-entropy-value"
            {
                return false;
            }
            if hit.start < prefix.len() {
                return false;
            }
            hit.start -= prefix.len();
            hit.end -= prefix.len();
            true
        });
        let opaque: Vec<_> = tokens(text).map(|(start, end, _)| (start, end)).collect();
        batch.findings.retain(|hit| {
            !opaque
                .iter()
                .any(|(start, end)| hit.start < *end && hit.end > *start)
        });
        self.protect_batch(text, batch)
    }

    fn protect_batch(&mut self, text: &str, batch: Batch) -> Outcome {
        let mut complete = self.complete && batch.complete;
        let findings = batch.findings;
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

    fn hydrate_carrier(&self, text: &str) -> Outcome {
        let mut outcome = Outcome {
            content: text.into(),
            status: Status::Complete,
            replacements: 0,
            unresolved: 0,
            consumed: None,
        };
        let mut seen = std::collections::HashSet::new();
        while whole_token(&outcome.content) {
            if !seen.insert(outcome.content.clone()) {
                outcome.unresolved += 1;
                outcome.status = Status::Partial;
                break;
            }
            let next = self.hydrate(&outcome.content);
            let progress = next.replacements > 0;
            outcome.content = next.content;
            outcome.replacements += next.replacements;
            outcome.unresolved += next.unresolved;
            if next.status != Status::Complete {
                outcome.status = Status::Partial;
            }
            if !progress {
                break;
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Hidden protocol tokens share aliases with visible results, and hidden valid IDs reserve
    /// their names before allocation. Tool arguments hydrate as data rather than protocol links.
    #[test]
    fn wrapped_carriers_share_materialized_aliases_and_reserve_hidden_identifiers() {
        let root = tempfile::tempdir().unwrap();
        let mut projector = Projector {
            store: Store::open(&root.path().join("privacy"), None).unwrap(),
            dictionary: Dictionary::default(),
            policy: Snapshot::compile(&[], &[], false).unwrap(),
            key: None,
            complete: true,
        };
        let id = "call_x7Qp9Ls2Vn4Rm8Tc6Yz3Ba1W";
        let quoted = "quoted \" private value\nwith \\ escapes";
        let fragment = "\"name\":\"read\"},\"id\"";
        let calls =
            json!([{"id":id,"function":{"name":"read","arguments":{"id":id,"quoted":quoted}}}]);
        let record = json!({"type":"hermes_message","data":{"role":"assistant","session_id":"session","tool_calls":calls.to_string()}});
        let result = json!({"type":"hermes_message","data":{"role":"tool","session_id":"session","tool_call_id":id,"content":"complete"}});
        projector.policy = Snapshot::compile(
            &[id, quoted].map(|value| super::super::policy::Literal {
                value,
                source: Source::RepositoryUser,
            }),
            &[],
            false,
        )
        .unwrap();
        let fields = projector.transform(&format!("{record}\n{result}\n"), Mode::ProtectJsonl);
        let baseline = crate::domain::install::repair_protocol_ids(&fields.content).unwrap();
        let baseline: Vec<Value> = baseline
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let collision = baseline[1]["data"]["tool_call_id"].as_str().unwrap();
        let mut rows: Vec<Value> = fields
            .content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let mut calls: Value =
            serde_json::from_str(rows[0]["data"]["tool_calls"].as_str().unwrap()).unwrap();
        calls
            .as_array_mut()
            .unwrap()
            .push(json!({"id":collision,"function":{"name":"read","arguments":{}}}));
        rows[0]["data"]["tool_calls"] = json!(calls.to_string());
        projector.policy = Snapshot::compile(
            &[id, quoted, fragment].map(|value| super::super::policy::Literal {
                value,
                source: Source::RepositoryUser,
            }),
            &[],
            false,
        )
        .unwrap();
        let protected =
            projector.transform(&format!("{}\n{}\n", rows[0], rows[1]), Mode::ProtectJsonl);
        let saved = crate::domain::transcript::wrap_lines(
            &protected.content,
            "hermes",
            &format!("agit-{}", "a".repeat(40)),
        );
        let repaired = crate::domain::install::repair_saved_protocol_ids(&saved).unwrap();
        let hydrated = projector.transform(&repaired, Mode::HydrateEnvelopes);
        assert!(hydrated.status == Status::Complete);
        assert_eq!(hydrated.unresolved, 0);
        let hydrated =
            crate::domain::install::repair_saved_protocol_ids(&hydrated.content).unwrap();
        let native = crate::domain::transcript::unwrap_strict(&hydrated).unwrap();
        let rows: Vec<Value> = native
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let calls: Value =
            serde_json::from_str(rows[0]["data"]["tool_calls"].as_str().unwrap()).unwrap();
        assert_eq!(calls[0]["id"], rows[1]["data"]["tool_call_id"]);
        assert_ne!(calls[0]["id"], collision);
        assert_eq!(calls[1]["id"], collision);
        assert_eq!(calls[0]["function"]["arguments"]["id"], id);
        assert_eq!(calls[0]["function"]["arguments"]["quoted"], quoted);
        let hermes = crate::adapter::get("hermes").unwrap();
        hermes.parse(&native).unwrap();
        let open = hermes.open_tool_calls(&native);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].call_id, collision);
        let localized = hermes
            .localize(
                &native,
                "materialized-session",
                std::path::Path::new("/workspace"),
            )
            .unwrap();
        let localized: Vec<Value> = localized
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(localized.len(), rows.len() + 2);
        assert_eq!(localized[2]["data"]["tool_call_id"], calls[0]["id"]);
        assert_eq!(localized[2]["data"]["content"], "complete");
        assert_eq!(localized[3]["data"]["tool_call_id"], collision);
        assert_eq!(
            localized[3]["data"]["content"],
            crate::domain::install::OPEN_CALL_PLACEHOLDER_OUTPUT
        );
    }

    /// Ordinary encoded arrays restore protocol fields with their surrounding records. Delaying
    /// these tokens without an opaque carrier leaves calls and results with different identities.
    #[test]
    fn unwrapped_encoded_carriers_hydrate_protocol_ids_normally() {
        let root = tempfile::tempdir().unwrap();
        let mut projector = Projector {
            store: Store::open(&root.path().join("privacy"), None).unwrap(),
            dictionary: Dictionary::default(),
            policy: Snapshot::compile(&[], &[], false).unwrap(),
            key: None,
            complete: true,
        };
        let id = "call_x7Qp9Ls2Vn4Rm8Tc6Yz3Ba1W";
        let calls = json!([{"id":id,"function":{"name":"read","arguments":{"id":id}}}]);
        let assistant = json!({"type":"hermes_message","data":{"role":"assistant","session_id":"session","tool_calls":calls.to_string()}});
        let result = json!({"type":"hermes_message","data":{"role":"tool","session_id":"session","tool_call_id":id,"content":"complete"}});
        projector.policy = Snapshot::compile(
            &[super::super::policy::Literal {
                value: id,
                source: Source::RepositoryUser,
            }],
            &[],
            false,
        )
        .unwrap();
        let protected =
            projector.transform(&format!("{assistant}\n{result}\n"), Mode::ProtectJsonl);
        assert!(protected.status == Status::Complete);
        let hydrated = projector.transform(&protected.content, Mode::HydrateJsonl);
        assert!(hydrated.status == Status::Complete);
        assert_eq!(hydrated.unresolved, 0);
        let rows: Vec<Value> = hydrated
            .content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let restored: Value =
            serde_json::from_str(rows[0]["data"]["tool_calls"].as_str().unwrap()).unwrap();
        assert_eq!(restored, calls);
        assert_eq!(rows[1], result);
    }

    /// A carrier can wrap field tokens. Its inner values must hydrate after JSON decoding,
    /// or quote-bearing secrets corrupt the carrier and unresolvable entries appear complete.
    #[test]
    fn encoded_carrier_hydration_restores_existing_field_tokens_in_one_pass() {
        let root = tempfile::tempdir().unwrap();
        let mut projector = Projector {
            store: Store::open(&root.path().join("privacy"), None).unwrap(),
            dictionary: Dictionary::default(),
            policy: Snapshot::compile(&[], &[], false).unwrap(),
            key: None,
            complete: true,
        };
        let id = "call_x7Qp9Ls2Vn4Rm8Tc6Yz3Ba1W";
        let secret = "registered-private-value";
        let quoted = "quoted \" private value\nwith \\ escapes";
        let fragment = "\"name\":\"read\"},\"id\"";
        let calls = json!([{"id":id,"function":{"name":"read","arguments":{"secret":secret,"quoted":quoted}}}]);
        let record = json!({"type":"hermes_message","data":{"role":"assistant","session_id":"session","tool_calls":calls.to_string()}});
        let result = json!({"type":"hermes_message","data":{"role":"tool","session_id":"session","tool_call_id":id,"content":"complete"}});
        let transcript = format!("{record}\n{result}\n");
        projector.policy = Snapshot::compile(
            &[secret, quoted].map(|value| super::super::policy::Literal {
                value,
                source: Source::RepositoryUser,
            }),
            &[],
            false,
        )
        .unwrap();
        let fields = projector.transform(&transcript, Mode::ProtectJsonl);
        assert!(fields.status == Status::Complete);
        let row: Value = serde_json::from_str(fields.content.lines().next().unwrap()).unwrap();
        let protected_calls: Value =
            serde_json::from_str(row["data"]["tool_calls"].as_str().unwrap()).unwrap();
        let inner = protected_calls[0]["function"]["arguments"]["secret"]
            .as_str()
            .unwrap();
        assert!(token_identity(inner).is_some());
        projector.policy = Snapshot::compile(
            &[secret, quoted, fragment].map(|value| super::super::policy::Literal {
                value,
                source: Source::RepositoryUser,
            }),
            &[],
            false,
        )
        .unwrap();
        let protected = projector.transform(&fields.content, Mode::ProtectJsonl);
        assert!(protected.status == Status::Complete);
        let outer: Value = serde_json::from_str(protected.content.lines().next().unwrap()).unwrap();
        let outer = outer["data"]["tool_calls"].as_str().unwrap();
        assert!(token_identity(outer).is_some());
        let hydrated = projector.transform(&protected.content, Mode::HydrateJsonl);
        assert!(hydrated.status == Status::Complete);
        assert_eq!(hydrated.unresolved, 0);
        let rows: Vec<Value> = hydrated
            .content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let restored: Value =
            serde_json::from_str(rows[0]["data"]["tool_calls"].as_str().unwrap()).unwrap();
        assert_eq!(restored, calls);
        assert_eq!(rows[1], result);
        let saved = crate::domain::transcript::wrap_lines(
            &protected.content,
            "hermes",
            &format!("agit-{}", "a".repeat(40)),
        );
        let hydrated = projector.transform(&saved, Mode::HydrateEnvelopes);
        assert!(hydrated.status == Status::Complete);
        assert_eq!(hydrated.unresolved, 0);
        let native = crate::domain::transcript::unwrap_strict(&hydrated.content).unwrap();
        let hermes = crate::adapter::get("hermes").unwrap();
        hermes.parse(&native).unwrap();
        assert!(hermes.open_tool_calls(&native).is_empty());

        let dictionary = std::mem::take(&mut projector.dictionary);
        for entry in dictionary.records().filter(|entry| entry.token != inner) {
            projector.dictionary.accept(entry.clone()).unwrap();
        }
        let missing = projector.transform(&protected.content, Mode::HydrateJsonl);
        assert!(missing.status == Status::Partial);
        assert_eq!(missing.unresolved, 1);
        let row: Value = serde_json::from_str(missing.content.lines().next().unwrap()).unwrap();
        let remaining: Value =
            serde_json::from_str(row["data"]["tool_calls"].as_str().unwrap()).unwrap();
        assert_eq!(remaining[0]["function"]["arguments"]["secret"], inner);
        assert_eq!(remaining[0]["function"]["arguments"]["quoted"], quoted);
        projector.dictionary = Dictionary::default();
        let missing = projector.transform(&protected.content, Mode::HydrateJsonl);
        assert!(missing.status == Status::Partial);
        assert_eq!(missing.unresolved, 1);
        assert_eq!(missing.content, protected.content);

        let mut cycle = dictionary.get(outer).unwrap().clone();
        cycle.original = cycle.token.clone();
        projector.dictionary.accept(cycle).unwrap();
        let cyclic = projector.transform(&protected.content, Mode::HydrateJsonl);
        assert!(cyclic.status == Status::Partial);
        assert_eq!(cyclic.unresolved, 1);
    }

    /// Explicit fragments can cross JSON fields. Redacting only decoded values misses these
    /// blocks; scanning the carrier as plain text also changes unrelated protocol identifiers.
    #[test]
    fn encoded_json_fragments_remain_private_and_restore_completed_call_pairs() {
        let root = tempfile::tempdir().unwrap();
        let mut projector = Projector {
            store: Store::open(&root.path().join("privacy"), None).unwrap(),
            dictionary: Dictionary::default(),
            policy: Snapshot::compile(&[], &[], false).unwrap(),
            key: None,
            complete: true,
        };
        let first = "call_x7Qp9Ls2Vn4Rm8Tc6Yz3Ba1W";
        let second = "call_n8Rp2Va6Jx4Bt9Ls3Wq7Mc1Z";
        let quoted = "quoted \" private value";
        let calls = json!([
            {"id":first,"function":{"name":"read","arguments":{}}},
            {"id":second,"function":{"name":"read","arguments":{"echo":second,"secret":quoted}}}
        ]);
        let carrier = calls.to_string();
        let escaped = carrier.replace("\\\"", "\\u0022");
        let whole_object = calls[0].to_string();
        let fragment = "\"name\":\"read\"},\"id\"";
        for (carrier, block) in [
            (&carrier, whole_object.as_str()),
            (&carrier, fragment),
            (&escaped, "u0022"),
        ] {
            assert!(carrier.contains(block));
            projector.policy = Snapshot::compile(
                &[
                    super::super::policy::Literal {
                        value: block,
                        source: Source::GlobalUser,
                    },
                    super::super::policy::Literal {
                        value: quoted,
                        source: Source::RepositoryUser,
                    },
                ],
                &[],
                false,
            )
            .unwrap();
            let record = json!({"type":"hermes_message","data":{"role":"assistant","session_id":"session","tool_calls":carrier}});
            let results: Vec<Value> = [first, second]
                .into_iter()
                .map(|id| json!({"type":"hermes_message","data":{"role":"tool","session_id":"session","tool_call_id":id,"content":"complete"}}))
                .collect();
            let transcript = format!("{record}\n{}\n{}\n", results[0], results[1]);
            let protected = projector.transform(&transcript, Mode::ProtectJsonl);
            assert!(protected.status == Status::Complete);
            assert!(protected.replacements > 0);
            let row: Value =
                serde_json::from_str(protected.content.lines().next().unwrap()).unwrap();
            let raw = row["data"]["tool_calls"].as_str().unwrap();
            assert!(!raw.contains(block));
            assert!(!raw.contains(&serde_json::to_string(quoted).unwrap()));
            assert!(raw.starts_with("{{AGIT_SECRET_V2:"));
            let repeated = projector.transform(&protected.content, Mode::ProtectJsonl);
            assert_eq!(repeated.content, protected.content);
            assert_eq!(repeated.replacements, 0);
            let hydrated = projector.transform(&repeated.content, Mode::HydrateJsonl);
            assert!(hydrated.status == Status::Complete);
            let rows: Vec<Value> = hydrated
                .content
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(rows[0], record);
            assert_eq!(rows[1..], results);
            let hermes = crate::adapter::get("hermes").unwrap();
            hermes.parse(&hydrated.content).unwrap();
            assert!(hermes.open_tool_calls(&hydrated.content).is_empty());
            let localized = hermes
                .localize(
                    &repeated.content,
                    "materialized-session",
                    std::path::Path::new("/workspace"),
                )
                .unwrap();
            assert_eq!(localized.lines().count(), rows.len() + 1);
            let localized = projector.transform(&localized, Mode::HydrateJsonl);
            hermes.parse(&localized.content).unwrap();
            assert!(hermes.open_tool_calls(&localized.content).is_empty());
        }
    }

    /// Native linkage survives every projection entry point. The same bytes in arguments or
    /// results remain scannable, so a field-name allowlist cannot satisfy this contract.
    #[test]
    fn protocol_identifiers_survive_projection_without_exempting_tool_data() {
        let root = tempfile::tempdir().unwrap();
        let mut projector = Projector {
            store: Store::open(&root.path().join("privacy"), None).unwrap(),
            dictionary: Dictionary::default(),
            policy: Snapshot::compile(&[], &[], false).unwrap(),
            key: None,
            complete: true,
        };
        let id = "call_x7Qp9Ls2Vn4Rm8Tc6Yz3Ba1W";
        let fixtures = [
            (
                "codex",
                json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","client_id":id,"content":[{"type":"text","text":id}]}}}),
                "/payload/item/client_id",
                "/payload/item/content",
            ),
            (
                "codex",
                json!({"type":"response_item","payload":{"type":"function_call","call_id":id,"name":"exec_command","arguments":json!({"call_id":id}).to_string()}}),
                "/payload/call_id",
                "/payload/arguments",
            ),
            (
                "codex",
                json!({"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":id,"output":id}}),
                "/payload/call_id",
                "/payload/output",
            ),
            (
                "codex",
                json!({"type":"compacted","payload":{"replacement_history":[{"type":"local_shell_call","call_id":id,"action":{"command":[id]}}]}}),
                "/payload/replacement_history/0/call_id",
                "/payload/replacement_history/0/action",
            ),
            (
                "claude-code",
                json!({"type":"assistant","message":{"role":"assistant","id":id,"content":[{"type":"tool_use","id":id,"name":"Read","input":{"id":id}}]}}),
                "/message/content/0/id",
                "/message/content/0/input",
            ),
            (
                "claude-desktop",
                json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":id}]}}),
                "/message/content/0/tool_use_id",
                "/message/content/0/content",
            ),
            (
                "opencode",
                json!({"kind":"part","id":"prt_fixture","session_id":"ses_fixture","message_id":id,"data":{"type":"tool","callID":id,"state":{"input":{"id":id}}}}),
                "/data/callID",
                "/data/state/input",
            ),
            (
                "opencode",
                json!({"kind":"message","id":"msg_fixture","session_id":"ses_fixture","data":{"role":"assistant","parentID":id,"content":id}}),
                "/data/parentID",
                "/data/content",
            ),
            (
                "hermes",
                json!({"type":"hermes_message","data":{"role":"assistant","session_id":id,"tool_calls":[{"id":id,"type":"function","function":{"name":"read_file","arguments":json!({"id":id}).to_string()}}]}}),
                "/data/tool_calls/0/id",
                "/data/tool_calls/0/function/arguments",
            ),
            (
                "hermes",
                json!({"type":"hermes_message","data":{"role":"tool","session_id":"session","tool_call_id":id,"content":id}}),
                "/data/tool_call_id",
                "/data/content",
            ),
            (
                "openclaw",
                json!({"type":"message","id":"entry","parentId":id,"message":{"role":"assistant","content":[{"type":"toolCall","id":id,"name":"read","arguments":{"id":id}}]}}),
                "/message/content/0/id",
                "/message/content/0/arguments",
            ),
            (
                "openclaw",
                json!({"type":"message","message":{"role":"toolResult","toolCallId":id,"content":id}}),
                "/message/toolCallId",
                "/message/content",
            ),
            (
                "openclaw",
                json!({"type":"leaf","id":"entry","targetId":id,"appendParentId":id,"label":id}),
                "/targetId",
                "/label",
            ),
            (
                "workbuddy",
                json!({"type":"function_call","sessionId":"session","callId":id,"name":"Read","arguments":{"id":id}}),
                "/callId",
                "/arguments",
            ),
        ];
        for (runtime, original, identity, data) in fixtures {
            for mode in [
                Mode::ProtectJsonl,
                Mode::ProtectNative,
                Mode::ProtectEnvelopes,
            ] {
                let text = match mode {
                    Mode::ProtectNative => serde_json::to_string(&NativeBatch {
                        runtime: runtime.into(),
                        session: "aaaaaaaa-0000-4000-8000-000000000001".into(),
                        records: vec![original.clone()],
                        pointers: vec![vec![]],
                    })
                    .unwrap(),
                    Mode::ProtectEnvelopes => crate::domain::transcript::wrap_lines(
                        &original.to_string(),
                        runtime,
                        &format!("agit-{}", "a".repeat(40)),
                    ),
                    _ => original.to_string(),
                };
                let output = projector.transform(&text, mode);
                assert!(output.status == Status::Complete, "{runtime}");
                let mut record: Value = serde_json::from_str(output.content.trim()).unwrap();
                record = match mode {
                    Mode::ProtectNative => record[0].take(),
                    Mode::ProtectEnvelopes => record["content"].take(),
                    _ => record,
                };
                assert_eq!(
                    record.pointer(identity).unwrap(),
                    id,
                    "{runtime}: {identity}"
                );
                let body = record.pointer(data).unwrap().to_string();
                assert!(!body.contains(id), "{runtime}: {data}");
                assert!(body.contains("AGIT_SECRET_V2:"), "{runtime}: {data}");
            }
        }

        let secret = "registered-private-value";
        projector.policy = Snapshot::compile(
            &[super::super::policy::Literal {
                value: secret,
                source: Source::RepositoryUser,
            }],
            &[],
            false,
        )
        .unwrap();
        let encoded = json!({"type":"hermes_message","data":{"role":"assistant","session_id":"session","tool_calls":json!([{"id":id,"function":{"name":"read","arguments":json!({"id":id,"secret":secret}).to_string()}}]).to_string()}});
        let result = json!({"type":"hermes_message","data":{"role":"tool","session_id":"session","tool_call_id":id,"content":"complete"}});
        let transcript = format!("{encoded}\n{result}\n");
        // A blocked argument must not turn an encoded carrier into ordinary text. Completed
        // calls stay paired through materialization without an artificial unfinished result.
        let output = projector.transform(&transcript, Mode::ProtectJsonl);
        assert!(output.status == Status::Complete);
        let rows: Vec<Value> = output
            .content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let calls: Value =
            serde_json::from_str(rows[0]["data"]["tool_calls"].as_str().unwrap()).unwrap();
        assert_eq!(calls[0]["id"], id);
        assert_eq!(calls[0]["id"], rows[1]["data"]["tool_call_id"]);
        assert!(
            !calls[0]["function"]["arguments"]
                .as_str()
                .unwrap()
                .contains(id)
        );
        assert!(!output.content.contains(secret));
        let hermes = crate::adapter::get("hermes").unwrap();
        hermes.parse(&output.content).unwrap();
        assert!(hermes.open_tool_calls(&output.content).is_empty());
        let localized = hermes
            .localize(
                &output.content,
                "materialized-session",
                std::path::Path::new("/workspace"),
            )
            .unwrap();
        let localized: Vec<Value> = localized
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(localized.len(), rows.len() + 1);
        let localized_calls: Value =
            serde_json::from_str(localized[1]["data"]["tool_calls"].as_str().unwrap()).unwrap();
        assert_eq!(localized_calls[0]["id"], id);
        assert_eq!(localized[2]["data"]["tool_call_id"], id);
        assert_eq!(localized[2]["data"]["content"], "complete");
        let restored = projector.transform(&output.content, Mode::HydrateJsonl);
        let restored: Vec<Value> = restored
            .content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let restored_calls: Value =
            serde_json::from_str(restored[0]["data"]["tool_calls"].as_str().unwrap()).unwrap();
        let original_calls: Value =
            serde_json::from_str(encoded["data"]["tool_calls"].as_str().unwrap()).unwrap();
        assert_eq!(restored_calls, original_calls);
        assert_eq!(restored[1], result);

        let unknown =
            json!({"payload":{"call_id":id},"message":{"content":[{"type":"tool_use","id":id}]}});
        assert!(
            !projector
                .transform(&unknown.to_string(), Mode::ProtectJsonl)
                .content
                .contains(id)
        );
        let credential = json!({"type":"response_item","payload":{"type":"function_call","call_id":"AKIA2E7YQXK4NMZ5VJ3T","name":"read","arguments":"{}"}});
        assert!(
            !projector
                .transform(&credential.to_string(), Mode::ProtectJsonl)
                .content
                .contains("AKIA2E7YQXK4NMZ5VJ3T")
        );
        projector.policy = Snapshot::compile(
            &[super::super::policy::Literal {
                value: id,
                source: Source::RepositoryUser,
            }],
            &[],
            false,
        )
        .unwrap();
        let blocked = json!({"type":"response_item","payload":{"type":"function_call","call_id":id,"name":"read","arguments":"{}"}});
        assert!(
            !projector
                .transform(&blocked.to_string(), Mode::ProtectJsonl)
                .content
                .contains(id)
        );

        let quoted = "quoted \" private value";
        projector.policy = Snapshot::compile(
            &[super::super::policy::Literal {
                value: quoted,
                source: Source::RepositoryUser,
            }],
            &[],
            false,
        )
        .unwrap();
        let calls = json!([{"id":id,"function":{"name":"read","arguments":{"secret":quoted}}}]);
        let record = json!({"type":"hermes_message","data":{"role":"assistant","tool_calls":calls.to_string()}});
        let protected = projector.transform(&record.to_string(), Mode::ProtectJsonl);
        let hydrated = projector.transform(&protected.content, Mode::HydrateJsonl);
        let record: Value = serde_json::from_str(&hydrated.content).unwrap();
        let restored: Value =
            serde_json::from_str(record["data"]["tool_calls"].as_str().unwrap()).unwrap();
        assert_eq!(
            restored, calls,
            "encoded carrier hydration must retain JSON quoting"
        );
        let serialized = calls.to_string();
        projector.policy = Snapshot::compile(
            &[super::super::policy::Literal {
                value: &serialized,
                source: Source::RepositoryUser,
            }],
            &[],
            false,
        )
        .unwrap();
        let protected = projector.transform(&record.to_string(), Mode::ProtectJsonl);
        let protected: Value = serde_json::from_str(&protected.content).unwrap();
        assert!(
            protected["data"]["tool_calls"]
                .as_str()
                .unwrap()
                .starts_with("{{AGIT_SECRET_V2:")
        );
    }
}
