//! Semantic projection uses durable mappings and keeps valid placeholders opaque.

use super::crypto::UserKey;
use super::detector::placeholder;
use super::dictionary::{Dictionary, Label, Origin, Record, token_id};
use super::policy::{Batch, Snapshot, Source};
use super::storage::Store;
use crate::adapter::protocol_ids::KeyContext;
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};
use std::ops::Range;

pub const MAX_UNIT_BYTES: usize = 8 * 1024 * 1024;

/// What a record keeps in place of a string no request can carry and no window can divide.
pub(crate) const OMITTED_TEXT: &str = "[content omitted: too large for the privacy check]";
/// What a long text keeps in place of a part with too many findings for the privacy check to
/// finish or divide.
pub(crate) const CROWDED_TEXT: &str = "[content omitted: too many findings for the privacy check]";
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
    FreezeHydration,
    HydrateFrozenJsonl,
    HydrationValues,
    /// Bounded input and output pages for ordinary native restoration.
    HydrateTransfer,
    /// Load the dictionary and the policy for a repository, under the start-up deadline, so
    /// that later requests spend their budget on content only.
    Prepare,
}

/// Why an outcome is not Complete. Callers report and count it; no reason lets a caller use
/// content the worker did not protect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// The caller's budget ran out before the worker processed everything.
    Deadline,
    /// Another caller in this process was using the worker.
    LockBusy,
    /// The worker failed and is not started again until its back-off ends.
    Backoff,
    /// The worker could not be started or did not become ready within its start-up deadline.
    SpawnFailed,
    /// The worker's reply was missing or malformed.
    ProtocolError,
    /// One unit was larger than the worker accepts.
    Oversize,
    /// The worker could not read or write its dictionary store.
    StoreBusy,
    /// The worker applied only the part of the policy this device could load.
    SourceIncomplete,
    /// One field held more findings than a single pass replaces.
    FindingsCap,
    /// A rule this device registered under another account or Hub could not be read, so a value
    /// it blocks could pass unreplaced.
    DeviceRules,
    /// A legacy secret dictionary, the repository's or this computer's, could not be read whole,
    /// so a value it blocks could pass unreplaced.
    LegacyRules,
    /// A blocked JSON scalar cannot be restored by the established string mapping format.
    UnsupportedScalar,
    /// Hydration left placeholders as they are that this device's dictionary, loaded whole, never
    /// held; every placeholder it holds was restored. Such a placeholder is text the person saw,
    /// from wherever it was made, and nothing on this device restores it.
    UnheldPlaceholders,
}

#[derive(Serialize, Deserialize)]
pub struct Outcome {
    pub content: String,
    pub status: Status,
    pub replacements: usize,
    pub unresolved: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed: Option<usize>,
    /// How many bytes at the end of `content` are input the worker did not process, copied
    /// unchanged. Only the service sets it, for the text and record protection modes.
    #[serde(skip)]
    pub unprocessed: usize,
    /// How many records in `content` lost parts too large for the privacy check to divide, a
    /// placeholder standing in their place. Only the service sets it.
    #[serde(skip)]
    pub omitted: usize,
    /// Why the outcome is not Complete. On a Complete outcome, [`Reason::FindingsCap`] says the
    /// input held more findings than one scan finishes: a text was replaced whole, or a stream
    /// released nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
}

impl Outcome {
    pub fn skipped(text: &str) -> Self {
        Self {
            content: text.to_owned(),
            status: Status::Skipped,
            replacements: 0,
            unresolved: 0,
            consumed: None,
            unprocessed: 0,
            omitted: 0,
            reason: None,
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

/// Where a stream may cut a line too long to wait for its newline: at whitespace with no
/// assignment separator within a window on either side, so a name leaves together with the
/// separator after it. Zero when no such point exists. A rule that needs more context than the
/// window still sees it in the released text every stream request carries as its context.
fn long_line_cut(text: &str, boundary: usize) -> usize {
    const WINDOW: usize = 96;
    let mut end = boundary;
    while let Some(cut) = text[..end].rfind(char::is_whitespace) {
        let from = floor_boundary(text, cut.saturating_sub(WINDOW));
        let to = ceil_boundary(text, (cut + WINDOW).min(text.len()));
        let Some(at) = text[from..to].find(['=', ':']) else {
            return cut;
        };
        // Every cut within the window of this separator fails the same way.
        end = floor_boundary(text, (from + at).saturating_sub(WINDOW - 1));
    }
    0
}

/// Where an unclosed private-key block begins: the last BEGIN line, in any letter case, with no
/// complete footer after it.
fn open_key_block(text: &str) -> Option<usize> {
    static FOOTER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"-----end[ a-z0-9_-]{0,100}-----")
            .expect("the key footer pattern is valid")
    });
    let lower = text.to_ascii_lowercase();
    let start = lower.rfind("-----begin")?;
    (!FOOTER.is_match(&lower[start..])).then_some(start)
}

/// The field of `object` that carries a base64 payload, and the type that payload is declared as:
/// a base64 source's `data` beside its `media_type`, an MCP result's `data` or `blob` beside its
/// `mimeType`, and a file's `data` beside its `mime`. A field anywhere else, such as `data` in
/// tool output, is an ordinary string.
pub(crate) fn carrier(object: &serde_json::Map<String, Value>) -> Option<(&'static str, String)> {
    let declared = |name: &str| object.get(name).and_then(Value::as_str).map(str::to_owned);
    if object.get("type").and_then(Value::as_str) == Some("base64")
        && let Some(media_type) = declared("media_type")
    {
        return Some(("data", media_type));
    }
    for (label, field) in [("mimeType", "data"), ("mimeType", "blob"), ("mime", "data")] {
        if let Some(media_type) = declared(label)
            && object.get(field).is_some_and(Value::is_string)
        {
            return Some((field, media_type));
        }
    }
    None
}

/// What a string carries, judged by its declared type and its bytes.
pub(crate) enum Payload {
    /// An ordinary string: not a base64 carrier, or not valid base64.
    Plain,
    /// Binary that is not text: never scanned or replaced, since a placeholder in it would only
    /// corrupt the image, document or font. A short string that decodes to bytes that aren't text
    /// is not taken for binary: a credential is short and can be valid base64, so it is checked
    /// as the string it is.
    Binary,
    /// Text carried as base64: `prefix` is the data URI's header, or empty for a carrier field,
    /// and `decoded` the text. It is checked as the text it is.
    Text { prefix: String, decoded: String },
    /// Declared as text, but not valid base64 or its bytes are not UTF-8: it can't be checked, so
    /// it is not published.
    Unreadable,
}

/// What a record keeps in place of a payload declared as text that can't be read as text.
pub(crate) const UNREADABLE_TEXT: &str =
    "[content omitted: encoded text the privacy check cannot read]";

/// What `text` carries: a base64 data URI, or the payload of a carrier field declared as
/// `declared` (see [`carrier`]). Only binary media whose decoded header proves its declared type
/// is left as it is unread. Any other payload is decoded: bytes that are UTF-8 are text, whatever
/// the label says, so a mislabel can't hide them. A payload of any length is decoded, as a
/// credential fits in far less than media does. A payload declared as text that doesn't decode is
/// unreadable like one whose bytes aren't text: part of it may still decode to a value.
pub(crate) fn payload(text: &str, declared: Option<&str>) -> Payload {
    /// The shortest payload of undeclared bytes taken for binary.
    const SHORTEST_BINARY: usize = 256;
    let (prefix, media_type, encoded) = match text.strip_prefix("data:").and_then(|uri| {
        let at = uri.find(";base64,").filter(|at| *at <= 128)?;
        let parameters = &uri[..at];
        (!parameters
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b'"' | b'\\' | b'<' | b'>')))
        .then(|| (at + "data:;base64,".len(), parameters))
    }) {
        Some((end, parameters)) => (
            &text[..end],
            parameters.split(';').next().unwrap_or_default(),
            &text[end..],
        ),
        None => match declared {
            Some(media_type) => ("", media_type, text),
            None => return Payload::Plain,
        },
    };
    if super::detector::verified_media(media_type, encoded) {
        return Payload::Binary;
    }
    let Some(bytes) = decoded_bytes(encoded) else {
        return if textual(media_type) {
            Payload::Unreadable
        } else {
            Payload::Plain
        };
    };
    match String::from_utf8(bytes) {
        Ok(decoded) => Payload::Text {
            prefix: prefix.to_owned(),
            decoded,
        },
        Err(_) if textual(media_type) => Payload::Unreadable,
        Err(_) if encoded.len() >= SHORTEST_BINARY => Payload::Binary,
        Err(_) => Payload::Plain,
    }
}

/// How many readings of a wrapped payload, longest first, [`text_spans`] decodes before it takes the
/// longest. Text after a payload adds one reading for each line it starts with base64 alone.
const WRAP_TRIES: usize = 8;

/// The data URIs inside `text` declared as a text type: each one's range, and where its payload
/// starts. A payload with no label beside it is not found here: nothing says it is text. A payload
/// that runs up to a line break may go on with the base64 that starts the next line, as a tool
/// wraps it, and so on; the span ends at the longest of those readings that decodes as text, of
/// the last [`WRAP_TRIES`] tried. When none does, it takes them all and is replaced whole, so text
/// after a wrapped payload can never end it early and leave the rest unread.
fn text_spans(text: &str) -> Vec<(Range<usize>, usize)> {
    static SPAN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r#"data:([a-zA-Z0-9.+-]{1,64}/[a-zA-Z0-9.+-]{1,64})(?:;[^;,\s"'\\<>]{1,40})*;base64,[A-Za-z0-9+/]+={0,2}"#,
        )
        .expect("the data URI pattern is valid")
    });
    if !text.contains(";base64,") {
        return Vec::new();
    }
    let readable = |encoded: &str| {
        decoded_bytes(encoded).is_some_and(|bytes| std::str::from_utf8(&bytes).is_ok())
    };
    let mut spans: Vec<(Range<usize>, usize)> = Vec::new();
    for found in SPAN.captures_iter(text) {
        let Some(span) = found.get(0) else { continue };
        if !textual(&found[1])
            || spans
                .last()
                .is_some_and(|(last, _)| span.start() < last.end)
        {
            continue;
        }
        let Some(at) = span.as_str().find(";base64,") else {
            continue;
        };
        let payload = span.start() + at + ";base64,".len();
        let mut wrapped = span.end();
        let mut ends = vec![wrapped];
        while !text[payload..wrapped].ends_with('=') {
            let rest = &text[wrapped..];
            let line_break = if rest.starts_with("\r\n") {
                2
            } else if rest.starts_with('\n') {
                1
            } else {
                break;
            };
            let next = &text[wrapped + line_break..];
            let run = next
                .bytes()
                .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/'))
                .count();
            let padding = next[run..]
                .bytes()
                .take(2)
                .take_while(|byte| *byte == b'=')
                .count();
            if run + padding == 0 {
                break;
            }
            wrapped += line_break + run + padding;
            ends.push(wrapped);
        }
        let end = ends
            .iter()
            .rev()
            .take(WRAP_TRIES)
            .copied()
            .find(|end| readable(&text[payload..*end]))
            .unwrap_or(wrapped);
        spans.push((span.start()..end, payload));
    }
    spans
}

/// The bytes of a base64 payload, whitespace aside and padding optional. `None` when it is not
/// valid base64.
fn decoded_bytes(encoded: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    );
    let compact: Vec<u8> = encoded
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    engine.decode(&compact).ok()
}

/// Whether `text` may hold base64 that protection encoded again with a placeholder inside: a data
/// URI, or a carrier's type label. A placeholder there is not visible in `text` itself, so a
/// restoration that looks only for visible placeholders must not skip such text.
pub(crate) fn may_carry_encoded_text(text: &str) -> bool {
    text.contains(";base64,") || text.contains("media_type") || text.contains("mime")
}

/// `outcome`, for text whose data URIs `carried` checked or restored first, with what that did.
fn with_spans(mut outcome: Outcome, carried: Option<Outcome>) -> Outcome {
    if let Some(carried) = carried {
        outcome.replacements += carried.replacements;
        outcome.unresolved += carried.unresolved;
        if carried.status != Status::Complete {
            outcome.status = Status::Partial;
            outcome.reason = outcome.reason.or(carried.reason);
        }
    }
    outcome
}

/// Whether a declared type says its bytes are text.
fn textual(media_type: &str) -> bool {
    let media_type = media_type.to_ascii_lowercase();
    media_type.starts_with("text/")
        || media_type.ends_with("+json")
        || media_type.ends_with("+xml")
        || matches!(
            media_type.as_str(),
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-yaml"
                | "application/yaml"
        )
}

/// Whether `text`, declared as `declared` when it is a carrier field, is binary media left as it
/// is (see [`payload`]).
pub(crate) fn media_payload(declared: Option<&str>, text: &str) -> bool {
    matches!(payload(text, declared), Payload::Binary)
}

/// `text` as `encoded` re-encodes it: under `prefix` when `changed` is the text it now holds, or
/// exactly as it was when nothing changed, so a payload with nothing to replace keeps its bytes,
/// line wrapping and padding included.
pub(crate) fn reencoded(text: &str, prefix: &str, decoded: &str, changed: &str) -> String {
    if changed == decoded {
        return text.to_owned();
    }
    use base64::Engine as _;
    format!(
        "{prefix}{}",
        base64::engine::general_purpose::STANDARD.encode(changed)
    )
}

fn floor_boundary(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn ceil_boundary(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

pub struct Projector {
    pub store: Store,
    pub dictionary: Dictionary,
    pub policy: Snapshot,
    pub key: Option<UserKey>,
    pub complete: bool,
    /// Why the loaded policy is incomplete, when the loader knows: reported in place of the
    /// generic [`Reason::SourceIncomplete`].
    pub missing: Option<Reason>,
    /// Remaining expansion for a complete native event carried through bounded IPC pages.
    pub hydration_growth: std::cell::Cell<Option<usize>>,
}

/// Recognition validates bounded token syntax before treating any text as opaque. Decoded
/// text carries raw delimiters, so escaped spellings are not placeholders here.
pub fn tokens(text: &str) -> impl Iterator<Item = (usize, usize, &str)> {
    let legacy = text
        .match_indices("{{AGIT_SECRET_V")
        .filter_map(|(start, _)| {
            let tail = &text[start..];
            let end = tail
                .as_bytes()
                .windows(2)
                .take(128)
                .position(|bytes| bytes == b"}}")?
                + 2;
            Some((start, start + end))
        });
    let typed = text
        .match_indices(placeholder::TYPED_OPEN)
        .filter_map(|(start, _)| {
            let close = text[start..]
                .char_indices()
                .take(96)
                .find(|(_, c)| *c == placeholder::TYPED_CLOSE)?
                .0;
            Some((start, start + close + placeholder::TYPED_CLOSE.len_utf8()))
        });
    let mut found: Vec<_> = legacy
        .chain(typed)
        .filter(|&(start, end)| token_id(&text[start..end]).is_some())
        .map(|(start, end)| (start, end, &text[start..end]))
        .collect();
    found.sort_unstable_by_key(|&(start, _, _)| start);
    found.into_iter()
}

fn whole_token(text: &str) -> bool {
    tokens(text)
        .next()
        .is_some_and(|(start, end, _)| start == 0 && end == text.len())
}

pub(crate) fn has_wrapped_carrier(value: &Value) -> bool {
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
    media: bool,
}

fn encoded_fields(text: &str, value: &Value) -> crate::Result<Vec<EncodedField>> {
    fn collect(
        raw: &RawValue,
        value: &Value,
        base: usize,
        declared: Option<&str>,
        fields: &mut Vec<EncodedField>,
    ) -> crate::Result<()> {
        match value {
            Value::String(text) => {
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
                    media: media_payload(declared, text),
                });
            }
            Value::Array(values) => {
                let raw: Vec<&RawValue> = serde_json::from_str(raw.get())?;
                for (raw, value) in raw.into_iter().zip(values) {
                    collect(raw, value, base, None, fields)?;
                }
            }
            Value::Object(values) => {
                let raw: std::collections::BTreeMap<String, &RawValue> =
                    serde_json::from_str(raw.get())?;
                let carried = carrier(values);
                for (key, value) in values {
                    let declared = carried
                        .as_ref()
                        .filter(|(field, _)| field == key)
                        .map(|(_, media_type)| media_type.as_str());
                    collect(raw[key], value, base, declared, fields)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let raw = serde_json::from_str::<&RawValue>(text)?;
    let mut fields = Vec::new();
    collect(raw, value, text.as_ptr() as usize, None, &mut fields)?;
    // Members are collected by name, which is not the order they sit in the text.
    fields.sort_by_key(|field| field.range.start);
    Ok(fields)
}

/// Whether any object in `text` names a member twice (names compared after escapes are undone).
/// Parsing into a map keeps only the last of them, so the others would go unread.
fn has_duplicate_members(text: &str) -> bool {
    struct Members<'a>(Vec<(String, &'a RawValue)>);
    impl<'de> Deserialize<'de> for Members<'de> {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Visit;
            impl<'de> serde::de::Visitor<'de> for Visit {
                type Value = Members<'de>;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("an object")
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> Result<Self::Value, A::Error> {
                    let mut members = Vec::new();
                    while let Some(member) = map.next_entry::<String, &'de RawValue>()? {
                        members.push(member);
                    }
                    Ok(Members(members))
                }
            }
            deserializer.deserialize_map(Visit)
        }
    }
    fn walk(raw: &RawValue) -> bool {
        match raw.get().as_bytes().first() {
            Some(b'{') => {
                serde_json::from_str::<Members>(raw.get()).map_or(true, |Members(members)| {
                    let mut names = std::collections::HashSet::new();
                    members
                        .iter()
                        .any(|(name, value)| !names.insert(name.as_str()) || walk(value))
                })
            }
            Some(b'[') => serde_json::from_str::<Vec<&RawValue>>(raw.get())
                .map_or(true, |values| values.into_iter().any(walk)),
            _ => false,
        }
    }
    serde_json::from_str::<&RawValue>(text).map_or(true, walk)
}

impl Projector {
    #[cfg(feature = "cli")]
    pub(crate) fn hydrate_event(
        &mut self,
        text: &str,
        mode: Mode,
        preserve_protocol_ids: Option<bool>,
        maximum: usize,
        output: &mut impl std::io::Write,
    ) -> crate::Result<Outcome> {
        self.hydration_growth
            .set(Some(maximum.saturating_sub(text.len())));
        let result = (|| {
            let mut outcome = Outcome {
                content: String::new(),
                status: Status::Complete,
                replacements: 0,
                unresolved: 0,
                consumed: None,
                unprocessed: 0,
                omitted: 0,
                reason: None,
            };
            if matches!(mode, Mode::HydrateText) {
                outcome = self.hydrate_text(text);
                output.write_all(outcome.content.as_bytes())?;
                outcome.content = String::new();
                return Ok(outcome);
            }
            let body = text.strip_suffix('\n').unwrap_or(text);
            if matches!(mode, Mode::HydrateEnvelopes) {
                let mut envelope = crate::domain::storage::parse_envelope_line(text)?;
                let preserve_ids =
                    preserve_protocol_ids.unwrap_or_else(|| has_wrapped_carrier(&envelope.content));
                self.record(&mut envelope.content, true, &mut outcome, preserve_ids);
                envelope.object_hash = crate::domain::transcript::object_hash(&envelope.content);
                serde_json::to_writer(&mut *output, &envelope)?;
            } else if let Ok(mut value) = serde_json::from_str::<Value>(body) {
                let preserve_ids =
                    preserve_protocol_ids.unwrap_or_else(|| has_wrapped_carrier(&value));
                self.record(&mut value, true, &mut outcome, preserve_ids);
                if outcome.replacements > 0 {
                    serde_json::to_writer(&mut *output, &value)?;
                } else {
                    output.write_all(body.as_bytes())?;
                }
            } else {
                outcome = self.hydrate_text(body);
                output.write_all(outcome.content.as_bytes())?;
                outcome.content = String::new();
            }
            if text.ends_with('\n') {
                output.write_all(b"\n")?;
            }
            Ok(outcome)
        })();
        self.hydration_growth.set(None);
        result
    }

    pub fn transform(&mut self, text: &str, mode: Mode) -> Outcome {
        self.transform_in("", text, mode)
    }

    /// [`Self::transform`] for text that continues `context`, text a stream already released.
    /// Only stream and text protection read the context: rules match across it, and only `text`
    /// is protected and returned.
    pub fn transform_in(&mut self, context: &str, text: &str, mode: Mode) -> Outcome {
        if context.len() + text.len() > MAX_UNIT_BYTES {
            return Outcome::skipped(text);
        }
        if matches!(mode, Mode::ProtectNative) {
            return self.native_batch(text);
        }
        if matches!(mode, Mode::ProtectStream) {
            return self.stream(context, text);
        }
        let hydrate = matches!(
            mode,
            Mode::HydrateText | Mode::HydrateJsonl | Mode::HydrateEnvelopes
        );
        if matches!(mode, Mode::ProtectText | Mode::HydrateText) {
            return if hydrate {
                self.hydrate_text(text)
            } else {
                self.protect_continuation(context, text)
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
        self.transform_records(text, mode, preserve_protocol_ids)
    }

    #[cfg(feature = "cli")]
    pub(crate) fn hydrate_frozen(&mut self, text: &str, preserve_protocol_ids: bool) -> Outcome {
        if text.len() > MAX_UNIT_BYTES {
            return Outcome::skipped(text);
        }
        self.transform_records(text, Mode::HydrateJsonl, preserve_protocol_ids)
    }

    fn transform_records(
        &mut self,
        text: &str,
        mode: Mode,
        preserve_protocol_ids: bool,
    ) -> Outcome {
        let hydrate = matches!(mode, Mode::HydrateJsonl | Mode::HydrateEnvelopes);
        let mut outcome = Outcome {
            content: String::with_capacity(text.len()),
            status: Status::Complete,
            replacements: 0,
            unresolved: 0,
            consumed: None,
            unprocessed: 0,
            omitted: 0,
            reason: None,
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
                    unprocessed: 0,
                    omitted: 0,
                    reason: None,
                };
                self.record(&mut value, hydrate, &mut next, preserve_protocol_ids);
                next.content = if next.replacements > 0 {
                    serde_json::to_string(&value).unwrap_or_else(|_| body.to_owned())
                } else {
                    body.to_owned()
                };
                next
            } else if hydrate {
                self.hydrate_text(body)
            } else {
                self.protect_continuation("", body)
            };
            outcome.content.push_str(&next.content);
            if line.ends_with('\n') {
                outcome.content.push('\n');
            }
            outcome.replacements += next.replacements;
            outcome.unresolved += next.unresolved;
            if next.status != Status::Complete {
                outcome.status = Status::Partial;
                outcome.reason = outcome.reason.or(next.reason);
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
            unprocessed: 0,
            omitted: 0,
            reason: None,
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

    fn stream(&mut self, context: &str, text: &str) -> Outcome {
        const LONG_LINE: usize = 128 * 1024;
        // A lookahead window is held back and whole lines leave; a line is cut inside only once
        // more than LONG_LINE bytes before the window have no newline. The scan covers `context`
        // too, so the cut never splits a finding that began in released text. A data URI declared
        // as a text type is held whole too, so the text it carries is checked in one piece. A
        // key block with no complete footer waits from its BEGIN line on, and nothing leaves
        // while the scan cannot finish.
        let hold = self.policy.max_literal_bytes.clamp(128, LONG_LINE);
        let boundary = floor_boundary(text, text.len().saturating_sub(hold));
        let mut cut = match text[..boundary].rfind('\n') {
            Some(newline) => newline + 1,
            None if boundary >= LONG_LINE => long_line_cut(text, boundary),
            None => 0,
        };
        if let Some(start) = open_key_block(text) {
            cut = cut.min(start);
        }
        let offset = context.len();
        let batch = self.policy.scan(&format!("{context}{text}"), MAX_FINDINGS);
        let capped = !batch.complete;
        if capped {
            cut = 0;
        }
        let mut spans: Vec<(usize, usize)> = batch
            .findings
            .iter()
            .map(|hit| (hit.start, hit.end))
            .chain(tokens(text).map(|(start, end, _)| (offset + start, offset + end)))
            .chain(
                text_spans(text)
                    .into_iter()
                    .map(|(span, _)| (offset + span.start, offset + span.end)),
            )
            .collect();
        // Latest first: once the cut moves back to one span's start, every span that starts
        // before it is still checked against the new cut.
        spans.sort_unstable_by_key(|span| std::cmp::Reverse(span.0));
        let mut at = offset + cut;
        for (start, end) in spans {
            if start < at && end > at {
                at = start.max(offset);
            }
        }
        let cut = at - offset;
        let mut outcome = self.protect_continuation(context, &text[..cut]);
        outcome.consumed = Some(cut);
        // Nothing released for too many findings, as opposed to no safe cut: a caller dividing a
        // long text tries a smaller part, where more text only makes it worse.
        if capped {
            outcome.reason = outcome.reason.or(Some(Reason::FindingsCap));
        }
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
            unprocessed: 0,
            omitted: 0,
            reason: None,
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
        if !hydrate {
            for pointer in layout.data.iter().chain(&layout.content) {
                let Some(text) = value.pointer(pointer).and_then(Value::as_str) else {
                    continue;
                };
                let next = if let Some(batch) = self.encoded_findings(text, false) {
                    serde_json::from_str::<Value>(text)
                        .ok()
                        .and_then(|decoded| {
                            self.protect_in_place(text, &decoded, |projector, protected, _| {
                                projector.encoded_findings(protected, false).is_some()
                            })
                        })
                        .unwrap_or_else(|| self.protect_batch(text, batch))
                } else if let Some(protected) = self.protect_encoded_media(text) {
                    protected
                } else {
                    continue;
                };
                outcome.replacements += next.replacements;
                outcome.unresolved += next.unresolved;
                if next.status != Status::Complete {
                    outcome.status = Status::Partial;
                    outcome.reason = outcome.reason.or(next.reason);
                }
                if let Some(field) = value.pointer_mut(pointer) {
                    retained.push((pointer.as_str(), Value::String(next.content)));
                    *field = Value::Null;
                }
            }
        }
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
                    outcome.reason = outcome.reason.or(next.reason);
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
            let key_batch = if hydrate {
                None
            } else {
                self.encoded_findings(&text, true).or_else(|| {
                    nested
                        .data
                        .iter()
                        .chain(&nested.content)
                        .find_map(|pointer| {
                            decoded
                                .pointer(pointer)
                                .and_then(Value::as_str)
                                .and_then(|text| self.encoded_findings(text, false))
                        })
                })
            };
            if !hydrate && let Some(mut batch) = self.scan_encoded(&text, &decoded).or(key_batch) {
                for finding in &mut batch.findings {
                    finding.start = 0;
                    finding.end = text.len();
                }
                let next = self.protect_batch(&text, batch);
                outcome.replacements += next.replacements;
                outcome.unresolved += next.unresolved;
                if next.status != Status::Complete {
                    outcome.status = Status::Partial;
                    outcome.reason = outcome.reason.or(next.reason);
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
                    .filter(|text| token_id(text).is_some())
                    .map(str::to_owned);
                if let Some(token) = token {
                    if let Some(field) = value.pointer_mut(&identity.pointer) {
                        *field = Value::Null;
                    }
                    retained.push((identity.pointer.as_str(), Value::String(token)));
                }
            }
        }
        // Protocol identities and runtime-opaque carriers are never replaced: a placeholder there
        // changes linkage or a verified byte sequence the runtime checks on resume.
        let protocol = layout
            .fields
            .iter()
            .map(|identity| identity.pointer.as_str())
            .chain(layout.opaque.iter().map(String::as_str));
        for pointer in protocol.filter(|_| !hydrate) {
            if let Some(field) = value.pointer_mut(pointer)
                && field.is_string()
            {
                retained.push((pointer, field.take()));
            }
        }
        self.value(value, hydrate, outcome, None, protocol_root, None);
        for (pointer, original) in retained {
            if let Some(field) = value.pointer_mut(pointer) {
                *field = original;
            }
        }
        if layout.schema {
            let mut payloads = Vec::new();
            for (pointers, context) in [
                (&layout.data, KeyContext::Data),
                (&layout.content, KeyContext::Content),
            ] {
                for pointer in pointers {
                    if let Some(data) = value.pointer_mut(pointer) {
                        self.data_keys(data, hydrate, outcome, context);
                        payloads.push((pointer, data.take()));
                    }
                }
            }
            self.data_keys(value, hydrate, outcome, KeyContext::Native);
            for (pointer, data) in payloads {
                if let Some(field) = value.pointer_mut(pointer) {
                    *field = data;
                }
            }
        } else {
            self.data_keys(value, hydrate, outcome, KeyContext::Data);
        }
    }

    /// The encoded carrier `text` with each value the check finds replaced where it stands, inside
    /// one of its string values and between escapes, every other byte kept: the carrier stays the
    /// same JSON, and hydration restores its spelling byte for byte. `None` when that can't be done
    /// without leaving a found value behind (see [`Self::leaves_a_value`]); the caller then
    /// replaces the carrier whole.
    fn protect_in_place(
        &mut self,
        text: &str,
        decoded: &Value,
        finds: impl Fn(&Self, &str, &Value) -> bool,
    ) -> Option<Outcome> {
        if self.encoded_findings(text, true).is_some() || has_duplicate_members(text) {
            return None;
        }
        let fields = encoded_fields(text, decoded).ok()?;
        let mut findings = Vec::new();
        for field in &fields {
            let mut start = field.range.start + 1;
            let stops = field
                .escapes
                .iter()
                .map(|escape| (escape.start, escape.end))
                .chain(std::iter::once((field.range.end - 1, field.range.end - 1)));
            for (stop, resume) in stops {
                let batch = self.policy.scan(&text[start..stop], MAX_FINDINGS);
                if !batch.complete || (field.media && !batch.findings.is_empty()) {
                    return None;
                }
                findings.extend(batch.findings.into_iter().map(|mut finding| {
                    finding.start += start;
                    finding.end += start;
                    finding
                }));
                start = resume;
            }
        }
        let explicit = self.policy.explicit_findings(text, MAX_FINDINGS);
        if findings.is_empty()
            || !explicit.complete
            || explicit.findings.iter().any(|hit| {
                !findings
                    .iter()
                    .any(|found| found.start <= hit.start && hit.end <= found.end)
            })
        {
            return None;
        }
        let values: Vec<String> = findings
            .iter()
            .map(|finding| text[finding.start..finding.end].to_owned())
            .collect();
        let next = self.protect_batch(
            text,
            Batch {
                findings,
                complete: true,
            },
        );
        if next.status != Status::Complete || self.leaves_a_value(&next.content, &values, finds) {
            return None;
        }
        Some(next)
    }

    /// Whether `protected`, a carrier replaced in place, still holds anything the check would
    /// keep back: it no longer parses, one of the replaced `values` appears in its raw text or in
    /// any decoded string or member name (escapes undone), or the check (`finds`, the one that
    /// found them) finds something in it again. Any of these falls back to replacing the carrier
    /// whole.
    fn leaves_a_value(
        &self,
        protected: &str,
        values: &[String],
        finds: impl Fn(&Self, &str, &Value) -> bool,
    ) -> bool {
        fn holds(value: &Value, found: &str) -> bool {
            match value {
                Value::String(text) => text.contains(found),
                Value::Array(values) => values.iter().any(|value| holds(value, found)),
                Value::Object(values) => values
                    .iter()
                    .any(|(name, value)| name.contains(found) || holds(value, found)),
                _ => false,
            }
        }
        let Ok(decoded) = serde_json::from_str::<Value>(protected) else {
            return true;
        };
        values
            .iter()
            .any(|found| protected.contains(found.as_str()) || holds(&decoded, found))
            || self.scan_encoded(protected, &decoded).is_some()
            || finds(self, protected, &decoded)
    }

    /// Escaped JSON payloads retain their exact spelling for native continuation. A decoded
    /// secret makes the carrier opaque before any value replacement, so quotes, backslashes
    /// and whitespace restore byte for byte instead of being serialized anew.
    fn encoded_findings(&self, text: &str, keys_only: bool) -> Option<Batch> {
        fn find(
            projector: &Projector,
            value: &Value,
            field: Option<&str>,
            declared: Option<&str>,
            keys_only: bool,
        ) -> Option<Batch> {
            match value {
                Value::Object(values) => {
                    let carried = carrier(values);
                    for (key, value) in values {
                        let batch = projector.policy.scan_member_name(key, MAX_FINDINGS);
                        if !batch.complete || !batch.findings.is_empty() {
                            return Some(batch);
                        }
                        let declared = carried
                            .as_ref()
                            .filter(|(field, _)| field == key)
                            .map(|(_, media_type)| media_type.as_str());
                        if let Some(batch) = find(projector, value, Some(key), declared, keys_only)
                        {
                            return Some(batch);
                        }
                    }
                    None
                }
                Value::Array(values) => values
                    .iter()
                    .find_map(|value| find(projector, value, field, None, keys_only)),
                Value::String(text) if !keys_only => {
                    // The caller replaces the whole encoded value on any finding, so a finding in
                    // carried text, or carried text that can't be read, stands for the value.
                    let batch = match payload(text, declared) {
                        Payload::Binary => return None,
                        Payload::Text { decoded, .. } => {
                            let batch = projector.field_findings(&decoded, field);
                            if batch.complete && batch.findings.is_empty() {
                                projector.field_findings(text, field)
                            } else {
                                batch
                            }
                        }
                        Payload::Unreadable => Batch {
                            findings: vec![super::policy::Finding {
                                start: 0,
                                end: text.len(),
                                source: Source::Default,
                                rule: "unreadable-text",
                            }],
                            complete: true,
                        },
                        Payload::Plain => {
                            let batch = projector.field_findings(text, field);
                            if !batch.complete || !batch.findings.is_empty() {
                                return Some(batch);
                            }
                            text_spans(text)
                                .into_iter()
                                .find_map(|(span, payload)| {
                                    let decoded = decoded_bytes(&text[payload..span.end])
                                        .and_then(|bytes| String::from_utf8(bytes).ok());
                                    let Some(decoded) = decoded else {
                                        return Some(Batch {
                                            findings: vec![super::policy::Finding {
                                                start: 0,
                                                end: text.len(),
                                                source: Source::Default,
                                                rule: "unreadable-text",
                                            }],
                                            complete: true,
                                        });
                                    };
                                    let batch = projector.field_findings(&decoded, field);
                                    (!batch.complete || !batch.findings.is_empty()).then_some(batch)
                                })
                                .unwrap_or(batch)
                        }
                    };
                    (!batch.complete || !batch.findings.is_empty()).then_some(batch)
                }
                _ => None,
            }
        }
        if !text.contains('\\') {
            return None;
        }
        let decoded: Value = serde_json::from_str(text).ok()?;
        let mut batch = find(self, &decoded, None, None, keys_only)?;
        if let Some(mut finding) = batch.findings.pop() {
            finding.start = 0;
            finding.end = text.len();
            batch.findings = vec![finding];
        }
        Some(batch)
    }

    /// Binary values inside encoded tool data have the same boundary as decoded source data.
    /// Discovery masks only their raw spans; projection still uses the original bytes, so a
    /// replacement elsewhere cannot normalize JSON spelling or store a masked recovery value.
    fn protect_encoded_media(&mut self, text: &str) -> Option<Outcome> {
        let decoded: Value = serde_json::from_str(text).ok()?;
        let media: Vec<_> = encoded_fields(text, &decoded)
            .ok()?
            .into_iter()
            .filter(|field| field.media)
            .map(|field| field.range.start + 1..field.range.end - 1)
            .collect();
        if media.is_empty() {
            return None;
        }
        let mut masked = text.as_bytes().to_vec();
        for region in &media {
            masked[region.clone()].fill(b' ');
        }
        let masked = String::from_utf8(masked).ok()?;
        let mut batch = self.field_findings(&masked, None);
        batch.findings.retain(|finding| {
            !media
                .iter()
                .any(|region| finding.start < region.end && region.start < finding.end)
        });
        Some(self.protect_findings(text, batch))
    }

    /// Payload member names carry data too. Rename only after values have been checked under
    /// their original field names, and keep the map intact if aliases would merge entries.
    fn data_keys(
        &mut self,
        value: &mut Value,
        hydrate: bool,
        outcome: &mut Outcome,
        context: KeyContext,
    ) {
        self.data_keys_inner(
            value,
            hydrate,
            outcome,
            context,
            matches!(context, KeyContext::Data | KeyContext::Content),
        );
    }

    fn data_keys_inner(
        &mut self,
        value: &mut Value,
        hydrate: bool,
        outcome: &mut Outcome,
        context: KeyContext,
        scalars: bool,
    ) {
        match value {
            Value::Array(values) => {
                for value in values {
                    self.data_keys_inner(value, hydrate, outcome, context, scalars);
                }
            }
            Value::Object(values) => {
                let schema = context.schema_fields(values);
                let mut names: std::collections::HashSet<_> = values
                    .keys()
                    .filter(|name| schema.contains(&name.as_str()))
                    .cloned()
                    .collect();
                let mut changes = Vec::new();
                for name in values.keys() {
                    if schema.contains(&name.as_str()) {
                        continue;
                    }
                    let next = if hydrate {
                        self.hydrate(name)
                    } else {
                        let batch = self.policy.scan_member_name(name, MAX_FINDINGS);
                        self.protect_findings(name, batch)
                    };
                    if !names.insert(next.content.clone()) {
                        outcome.status = Status::Partial;
                        outcome.reason = Some(Reason::ProtocolError);
                        outcome.unresolved += 1;
                        return;
                    }
                    changes.push((name.clone(), next));
                }
                let mut renamed = Vec::new();
                for (name, next) in changes {
                    outcome.replacements += next.replacements;
                    outcome.unresolved += next.unresolved;
                    if next.status != Status::Complete {
                        outcome.status = Status::Partial;
                        outcome.reason = outcome.reason.or(next.reason);
                    }
                    if name != next.content {
                        renamed.push((next.content, values.remove(&name).unwrap()));
                    }
                }
                values.extend(renamed);
                for (name, value) in values {
                    let nested = match context {
                        KeyContext::Native
                            if matches!(
                                name.as_str(),
                                "message"
                                    | "payload"
                                    | "data"
                                    | "item"
                                    | "replacement_history"
                                    | "tool_calls"
                                    | "function"
                                    | "history_base"
                                    | "state"
                                    | "source"
                                    | "subagent"
                                    | "thread_spawn"
                            ) =>
                        {
                            KeyContext::Native
                        }
                        KeyContext::Native if name == "content" => KeyContext::Content,
                        KeyContext::Content if !schema.is_empty() && name == "content" => {
                            KeyContext::Content
                        }
                        _ if !schema.is_empty()
                            && matches!(
                                name.as_str(),
                                "source" | "resource" | "file" | "audio" | "input_audio"
                            ) =>
                        {
                            KeyContext::Source
                        }
                        _ => KeyContext::Data,
                    };
                    let payload = matches!(
                        name.as_str(),
                        "input" | "arguments" | "output" | "content" | "structuredContent"
                    );
                    self.data_keys_inner(
                        value,
                        hydrate,
                        outcome,
                        nested,
                        scalars
                            && !matches!(nested, KeyContext::Source)
                            && (!schema.contains(&name.as_str()) || payload),
                    );
                }
            }
            Value::Number(_) | Value::Bool(_) if scalars && !hydrate => {
                self.protect_scalar(value, outcome);
            }
            _ => {}
        }
    }

    /// Only explicit literals classify non-string data. Native schema values never enter this
    /// path; replacing control flags or counters would change the runtime's interpretation.
    fn protect_scalar(&mut self, value: &mut Value, outcome: &mut Outcome) {
        let original = value.to_string();
        let batch = self.policy.scalar_findings(&original, MAX_FINDINGS);
        if !self.complete || !batch.complete {
            outcome.status = Status::Partial;
            outcome.reason = self.missing.or(Some(Reason::SourceIncomplete));
        }
        if !batch.findings.is_empty() {
            // A string placeholder cannot restore a JSON scalar's type on existing readers.
            // Refuse capture instead of publishing the value or silently changing its type.
            outcome.status = Status::Partial;
            outcome.reason = Some(Reason::UnsupportedScalar);
        }
    }

    fn value(
        &mut self,
        value: &mut Value,
        hydrate: bool,
        outcome: &mut Outcome,
        field: Option<&str>,
        protocol_root: bool,
        declared: Option<&str>,
    ) {
        match value {
            Value::String(text) => {
                let next = match payload(text, declared) {
                    Payload::Binary => return,
                    // Carried text is checked and restored as the text it is: only a payload that
                    // changes is encoded again.
                    // A string that only looks like base64 is still checked as itself when its
                    // decoded form holds nothing.
                    Payload::Text { prefix, decoded } if hydrate => {
                        let next = self.hydrate_carried(text, &prefix, &decoded);
                        if next.content == *text && next.status == Status::Complete {
                            self.hydrate(text)
                        } else {
                            next
                        }
                    }
                    Payload::Text { prefix, decoded } => {
                        let next = self.protect_field(&decoded, field);
                        if next.content == decoded && next.status == Status::Complete {
                            self.protect_field(text, field)
                        } else {
                            Outcome {
                                content: reencoded(text, &prefix, &decoded, &next.content),
                                ..next
                            }
                        }
                    }
                    // Kept out of the dictionary as well as the record: its bytes are the file.
                    Payload::Unreadable if !hydrate => {
                        let mut omitted = Outcome::skipped(UNREADABLE_TEXT);
                        omitted.status = Status::Complete;
                        // A change like any replacement: the record is written out again.
                        omitted.replacements = 1;
                        omitted
                    }
                    Payload::Unreadable | Payload::Plain if hydrate => self.hydrate_text(text),
                    Payload::Unreadable | Payload::Plain => {
                        let carried = self.carried_spans(text, false);
                        let plain = carried
                            .as_ref()
                            .map_or(text.as_str(), |carried| &carried.content);
                        let next = self.protect_field(plain, field);
                        with_spans(next, carried)
                    }
                };
                *text = next.content;
                outcome.replacements += next.replacements;
                outcome.unresolved += next.unresolved;
                if next.status != Status::Complete {
                    outcome.status = Status::Partial;
                    outcome.reason = outcome.reason.or(next.reason);
                }
            }
            Value::Array(values) => {
                for value in values {
                    self.value(value, hydrate, outcome, field, false, None);
                }
            }
            Value::Object(values) => {
                let carried = carrier(values);
                // Only carrier fields have identity semantics. Nested tool data remains content.
                for (field, value) in values {
                    if protocol_root
                        && !hydrate
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
                    let declared = carried
                        .as_ref()
                        .filter(|(name, _)| name == field)
                        .map(|(_, media_type)| media_type.as_str());
                    self.value(value, hydrate, outcome, Some(field), false, declared);
                }
            }
            _ => {}
        }
    }

    fn protect(&mut self, text: &str) -> Outcome {
        self.protect_field(text, None)
    }

    /// `text` with each data URI inside it declared as a text type checked, or restored, as the
    /// text it is: what is replaced is replaced in the decoded text, and only a payload that
    /// changes is encoded again. Bytes that aren't UTF-8 under a text type are not published.
    /// Decoding is bounded by the record, which is bounded by the request. `None` when `text`
    /// holds no such URI.
    fn carried_spans(&mut self, text: &str, hydrate: bool) -> Option<Outcome> {
        let spans = text_spans(text);
        if spans.is_empty() {
            return None;
        }
        let mut outcome = Outcome {
            content: String::with_capacity(text.len()),
            status: Status::Complete,
            replacements: 0,
            unresolved: 0,
            consumed: None,
            unprocessed: 0,
            omitted: 0,
            reason: None,
        };
        let mut cursor = 0;
        for (span, payload) in spans {
            outcome.content.push_str(&text[cursor..span.start]);
            cursor = span.end;
            let whole = &text[span.clone()];
            let decoded = decoded_bytes(&text[payload..span.end])
                .and_then(|bytes| String::from_utf8(bytes).ok());
            let Some(decoded) = decoded else {
                if hydrate {
                    outcome.content.push_str(whole);
                } else {
                    outcome.content.push_str(UNREADABLE_TEXT);
                    outcome.replacements += 1;
                }
                continue;
            };
            let prefix = &text[span.start..payload];
            let next = if hydrate {
                self.hydrate_carried(whole, prefix, &decoded)
            } else {
                let next = self.protect(&decoded);
                Outcome {
                    content: reencoded(whole, prefix, &decoded, &next.content),
                    ..next
                }
            };
            outcome.content.push_str(&next.content);
            outcome.replacements += next.replacements;
            outcome.unresolved += next.unresolved;
            if next.status != Status::Complete {
                outcome.status = Status::Partial;
                outcome.reason = outcome.reason.or(next.reason);
            }
        }
        outcome.content.push_str(&text[cursor..]);
        Some(outcome)
    }

    /// `whole`, a payload under `prefix` that decodes to `decoded`, with the text it carries
    /// restored and encoded again, or exactly as it was when nothing in it is restored. Encoding
    /// grows the text beyond what restoring it charged; a bounded restoration with no room left
    /// for that keeps the payload as it is.
    fn hydrate_carried(&mut self, whole: &str, prefix: &str, decoded: &str) -> Outcome {
        let next = self.hydrate(decoded);
        let content = reencoded(whole, prefix, decoded, &next.content);
        let charged = next.content.len().saturating_sub(decoded.len());
        let grown = content.len().saturating_sub(whole.len());
        if let Some(remaining) = self.hydration_growth.get() {
            let Some(left) = remaining.checked_sub(grown.saturating_sub(charged)) else {
                return Outcome {
                    content: whole.to_owned(),
                    status: Status::Partial,
                    replacements: 0,
                    unresolved: next.unresolved + next.replacements,
                    reason: Some(Reason::Oversize),
                    ..next
                };
            };
            self.hydration_growth.set(Some(left));
        }
        Outcome { content, ..next }
    }

    /// `text` restored, the data URIs inside it declared as a text type included.
    fn hydrate_text(&mut self, text: &str) -> Outcome {
        let carried = self.carried_spans(text, true);
        let plain = carried
            .as_ref()
            .map_or(text, |carried| carried.content.as_str());
        let next = self.hydrate(plain);
        with_spans(next, carried)
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

    fn protect_field(&mut self, text: &str, field: Option<&str>) -> Outcome {
        let batch = self.field_findings(text, field);
        self.protect_findings(text, batch)
    }

    fn field_findings(&self, text: &str, field: Option<&str>) -> Batch {
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
        // A string holding JSON holds member names too.
        let entropy_hit = batch
            .findings
            .iter()
            .any(|hit| hit.rule == "high-entropy-value");
        let schema_words = if entropy_hit {
            super::detector::json_schema_word_regions(text)
        } else {
            Default::default()
        };
        batch.findings.retain_mut(|hit| {
            if hit.start < prefix.len() {
                return false;
            }
            hit.start -= prefix.len();
            hit.end -= prefix.len();
            hit.rule != "high-entropy-value" || !schema_words.contains(&(hit.start, hit.end))
        });
        // A credential-named field delimits its value exactly, so the whole decoded value is one
        // finding whatever punctuation, quotes included, it carries.
        let explicit = batch
            .findings
            .iter()
            .any(|hit| hit.source != Source::Default);
        if field.is_some_and(|field| super::detector::credential_field_value(field, text))
            && (explicit || !self.policy.allows(text))
        {
            let source = batch
                .findings
                .iter()
                .map(|hit| hit.source)
                .find(|source| *source != Source::Default)
                .unwrap_or(Source::Default);
            batch.findings = vec![super::policy::Finding {
                start: 0,
                end: text.len(),
                source,
                rule: "credential-assignment",
            }];
        }
        batch
    }

    /// Protects `text` as the continuation of `context`, text a stream released before it: rules
    /// match across the two, and a finding that begins in `context` is replaced where it
    /// continues into `text`.
    fn protect_continuation(&mut self, context: &str, text: &str) -> Outcome {
        let carried = self.carried_spans(text, false);
        let text = carried
            .as_ref()
            .map_or(text, |carried| carried.content.as_str());
        if context.is_empty() || text.is_empty() {
            let next = self.protect(text);
            return with_spans(next, carried);
        }
        let offset = context.len();
        let mut batch = self.policy.scan(&format!("{context}{text}"), MAX_FINDINGS);
        batch.findings.retain_mut(|hit| {
            if hit.end <= offset {
                return false;
            }
            hit.start = hit.start.max(offset) - offset;
            hit.end -= offset;
            true
        });
        let next = self.protect_findings(text, batch);
        with_spans(next, carried)
    }

    /// Replaces `batch`'s findings in `text`, except where they touch a valid placeholder.
    fn protect_findings(&mut self, text: &str, mut batch: Batch) -> Outcome {
        let opaque: Vec<_> = tokens(text).map(|(start, end, _)| (start, end)).collect();
        batch.findings.retain(|hit| {
            !opaque
                .iter()
                .any(|(start, end)| hit.start < *end && hit.end > *start)
        });
        self.protect_batch(text, batch)
    }

    fn protect_batch(&mut self, text: &str, mut batch: Batch) -> Outcome {
        // A field with more findings than one pass replaces, or one the scan could not finish,
        // is replaced whole as one finding: a pass that stopped early would leave the rest raw.
        let capped = !batch.complete && !text.is_empty();
        if capped {
            batch = Batch {
                findings: vec![super::policy::Finding {
                    start: 0,
                    end: text.len(),
                    source: Source::Default,
                    rule: "findings-cap",
                }],
                complete: true,
            };
        }
        let batch_complete = batch.complete;
        let mut complete = self.complete && batch.complete;
        let findings = batch.findings;
        let mut new = Vec::<Record>::new();
        let mut seen = std::collections::HashSet::new();
        for finding in &findings {
            let Some(original) = text.get(finding.start..finding.end) else {
                complete = false;
                continue;
            };
            // A value first replaced under a legacy spelling gains a typed alias here.
            if self
                .dictionary
                .for_value(original)
                .is_none_or(|record| placeholder::typed_parts(&record.token).is_none())
                && seen.insert(original)
            {
                let origin = match finding.source {
                    Source::Default => Origin::Heuristic,
                    Source::GlobalUser => Origin::GlobalUser,
                    Source::RepositoryUser => Origin::RepositoryUser,
                };
                match Record::new(
                    &self.store.dictionary_id,
                    original,
                    origin,
                    Label::secret(finding.rule),
                ) {
                    Ok(record) => new.push(record),
                    Err(_) => complete = false,
                }
            }
        }
        let mut stored = true;
        if !new.is_empty() {
            if self.store.append(&new, self.key.as_ref()).is_ok() {
                for record in new {
                    stored &= self.dictionary.accept(record).is_ok();
                }
            } else {
                stored = false;
            }
        }
        complete &= stored;
        let mut outcome = Outcome {
            content: String::with_capacity(text.len()),
            status: Status::Complete,
            replacements: 0,
            unresolved: 0,
            consumed: None,
            unprocessed: 0,
            omitted: 0,
            reason: None,
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
            outcome.reason = Some(if !batch_complete {
                Reason::FindingsCap
            } else if !stored {
                Reason::StoreBusy
            } else {
                self.missing.unwrap_or(Reason::SourceIncomplete)
            });
        } else if capped {
            outcome.reason = Some(Reason::FindingsCap);
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
            unprocessed: 0,
            omitted: 0,
            reason: None,
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
                outcome.reason = outcome.reason.or(next.reason);
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
            unprocessed: 0,
            omitted: 0,
            reason: None,
        };
        let mut cursor = 0;
        let mut held = 0;
        for (start, end, token) in tokens(text) {
            let known = self.dictionary.get(token);
            held += usize::from(known.is_some());
            let record = known.filter(|record| {
                if let Some(remaining) = self.hydration_growth.get() {
                    let available = remaining
                        .checked_add(end - start)
                        .and_then(|room| room.checked_sub(record.original.len()));
                    let Some(available) = available else {
                        outcome.reason = Some(Reason::Oversize);
                        return false;
                    };
                    self.hydration_growth.set(Some(available));
                    true
                } else {
                    outcome.content.len() + start - cursor + record.original.len() + text.len()
                        - end
                        <= MAX_UNIT_BYTES
                }
            });
            if let Some(record) = record {
                outcome.content.push_str(&text[cursor..start]);
                outcome.content.push_str(&record.original);
                outcome.replacements += 1;
                held -= 1;
                cursor = end;
            } else {
                outcome.unresolved += 1;
            }
        }
        outcome.content.push_str(&text[cursor..]);
        if outcome.unresolved > 0 || !self.complete {
            outcome.status = Status::Partial;
            // `held` counts placeholders the dictionary holds but this pass could not restore.
            let unheld_only = self.complete && held == 0 && outcome.reason.is_none();
            outcome.reason = outcome.reason.or(if unheld_only {
                Some(Reason::UnheldPlaceholders)
            } else {
                self.missing
            });
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Content saved by clients that placed tokens in protocol identifiers restores with hidden
    /// protocol tokens sharing aliases with visible results, and hidden valid IDs reserve their
    /// names before allocation. Tool arguments hydrate as data rather than protocol links.
    #[test]
    fn wrapped_carriers_share_materialized_aliases_and_reserve_hidden_identifiers() {
        let root = tempfile::tempdir().unwrap();
        let mut projector = Projector {
            store: Store::open(&root.path().join("privacy"), None).unwrap(),
            dictionary: Dictionary::default(),
            policy: Snapshot::compile(&[], &[], false).unwrap(),
            key: None,
            complete: true,
            missing: None,
            hydration_growth: Default::default(),
        };
        let id = "call_x7Qp9Ls2Vn4Rm8Tc6Yz3Ba1W";
        let quoted = "quoted \" private value\nwith \\ escapes";
        let fragment = "\"name\":\"read\"},\"id\"";
        let mut legacy = |value: &str| {
            let record = Record {
                token: format!(
                    "{{{{AGIT_SECRET_V2:{}:{}}}}}",
                    projector.store.dictionary_id,
                    uuid::Uuid::new_v4()
                ),
                original: value.to_owned(),
                origins: [Origin::RepositoryUser].into(),
            };
            projector
                .store
                .append(std::slice::from_ref(&record), None)
                .unwrap();
            let token = record.token.clone();
            projector.dictionary.accept(record).unwrap();
            token
        };
        let (id_token, quoted_token) = (legacy(id), legacy(quoted));
        let calls = json!([{"id":id_token,"function":{"name":"read","arguments":{"id":id_token,"quoted":quoted_token}}}]);
        let record = json!({"type":"hermes_message","data":{"role":"assistant","session_id":"session","tool_calls":calls.to_string()}});
        let result = json!({"type":"hermes_message","data":{"role":"tool","session_id":"session","tool_call_id":id_token,"content":"complete"}});
        let fields = format!("{record}\n{result}\n");
        let baseline = crate::domain::install::repair_protocol_ids(&fields).unwrap();
        let baseline: Vec<Value> = baseline
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let collision = baseline[1]["data"]["tool_call_id"].as_str().unwrap();
        let mut rows: Vec<Value> = fields
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
            missing: None,
            hydration_growth: Default::default(),
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
            missing: None,
            hydration_growth: Default::default(),
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
        assert!(token_id(inner).is_some());
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
        assert!(token_id(outer).is_some());
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
            missing: None,
            hydration_growth: Default::default(),
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
            assert!(raw.starts_with("\u{2039}secret:blocked:"));
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

    /// A credential-named field's whole decoded value is one finding, punctuation included,
    /// unless a preset or the user allowed that value. A field context fed to the detector as an
    /// unquoted assignment would end the value at `;` and publish the rest; a whole-value rule that
    /// ignored allows would override the user's decision. Lists retain their field's context,
    /// but object members establish their own context instead of inheriting a list's name.
    #[test]
    fn credential_named_fields_replace_their_whole_decoded_value() {
        let root = tempfile::tempdir().unwrap();
        let mut projector = Projector {
            store: Store::open(&root.path().join("privacy"), None).unwrap(),
            dictionary: Dictionary::default(),
            policy: Snapshot::compile(&[], &[], false).unwrap(),
            key: None,
            complete: true,
            missing: None,
            hydration_growth: Default::default(),
        };
        let password = "c7Zq4xR2mNv8LpT3;mNv8L\"pT3!wYb6Ks1Hd9Fg5Jt0";
        let passphrase = "maple-river-7";
        let opaque = "R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe";
        let record = json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Write","input":{
            "passwd":password,"passwords":[password],"file_path":"/tmp/c7Zq4xR2.cfg",
            "password":passphrase,"pwd":"aaaaaaaaaaaa",
            "passphrase":"cedar_river","admin_password":"MAPLE_RIVER7",
            "examples":{"password":"YOUR_PASSWORD","passwd":"${DB_PASSWORD}"},
            "api_keys":[opaque,[opaque],{"label":opaque},42,true],"references":[opaque]
        }}]}});
        let protected = projector.transform(&record.to_string(), Mode::ProtectJsonl);
        for piece in ["c7Zq4xR2mNv8LpT3", "mNv8L", "pT3!wYb6"] {
            assert!(!protected.content.contains(piece), "{}", protected.content);
        }
        assert!(protected.content.contains("/tmp/c7Zq4xR2.cfg"));
        let value: Value = serde_json::from_str(&protected.content).unwrap();
        let input = &value["message"]["content"][0]["input"];
        assert!(whole_token(input["password"].as_str().unwrap()));
        assert_eq!(input["pwd"], "aaaaaaaaaaaa");
        assert!(whole_token(input["passphrase"].as_str().unwrap()));
        assert!(whole_token(input["admin_password"].as_str().unwrap()));
        assert_eq!(
            input["examples"],
            record["message"]["content"][0]["input"]["examples"]
        );
        assert!(whole_token(input["passwords"][0].as_str().unwrap()));
        assert!(whole_token(input["api_keys"][0].as_str().unwrap()));
        assert!(whole_token(input["api_keys"][1][0].as_str().unwrap()));
        assert_eq!(input["api_keys"][2]["label"], opaque);
        assert_eq!(input["api_keys"][3], 42);
        assert_eq!(input["api_keys"][4], true);
        assert_eq!(input["references"], json!([opaque]));
        let restored = projector.transform(&protected.content, Mode::HydrateJsonl);
        assert_eq!(
            serde_json::from_str::<Value>(&restored.content).unwrap(),
            record
        );
        projector.policy = Snapshot::compile(
            &[],
            &[password, passphrase, opaque, "cedar_river", "MAPLE_RIVER7"],
            false,
        )
        .unwrap();
        let allowed = projector.transform(&record.to_string(), Mode::ProtectJsonl);
        assert_eq!(
            serde_json::from_str::<Value>(&allowed.content).unwrap(),
            record
        );
    }

    /// Hydration names a placeholder this device's whole dictionary never held, which a caller
    /// may keep as text the person saw, apart from one this device made and cannot restore now. A
    /// dictionary not loaded whole cannot say which a placeholder it does not show is, so then
    /// nothing is named unheld. A projector that called every unresolved placeholder unheld would
    /// let a caller keep, as text, one whose original it only failed to read.
    #[test]
    fn only_a_whole_dictionary_names_a_placeholder_unheld() {
        let root = tempfile::tempdir().unwrap();
        // Longer than its placeholder, so restoring it grows the text.
        let value = "otter-quay-ledger-heron-meadow-5512-cedar-branch-petal-river-0042";
        let mut projector = Projector {
            store: Store::open(&root.path().join("privacy"), None).unwrap(),
            dictionary: Dictionary::default(),
            policy: Snapshot::compile(
                &[super::super::policy::Literal {
                    value,
                    source: Source::RepositoryUser,
                }],
                &[],
                false,
            )
            .unwrap(),
            key: None,
            complete: true,
            missing: None,
            hydration_growth: Default::default(),
        };
        let own = projector.transform(&format!("key {value}"), Mode::ProtectText);
        assert!(tokens(&own.content).next().is_some(), "{}", own.content);
        let foreign = crate::domain::privacy::detector::placeholder::typed_token(
            "secret",
            "aws-access-token",
        )
        .unwrap();
        let text = format!("{} and {foreign}", own.content);

        let restored = projector.transform(&text, Mode::HydrateText);
        assert_eq!(restored.content, format!("key {value} and {foreign}"));
        assert!(restored.status == Status::Partial);
        assert_eq!(restored.reason, Some(Reason::UnheldPlaceholders));

        // A placeholder this device holds but cannot restore within the budget is not unheld.
        projector.hydration_growth.set(Some(0));
        let unrestored = projector.transform(&text, Mode::HydrateText);
        assert_eq!(unrestored.reason, Some(Reason::Oversize));
        projector.hydration_growth.set(None);

        projector.complete = false;
        projector.missing = Some(Reason::LegacyRules);
        let partial = projector.transform(&text, Mode::HydrateText);
        assert_eq!(partial.reason, Some(Reason::LegacyRules));
    }

    /// A blocked scalar cannot be persisted using string-only placeholders. Native controls
    /// remain untouched, and encoded JSON can use an existing whole-carrier mapping.
    #[test]
    fn blocked_data_scalars_refuse_capture_without_changing_native_controls() {
        let root = tempfile::tempdir().unwrap();
        let mut projector = Projector {
            store: Store::open(&root.path().join("privacy"), None).unwrap(),
            dictionary: Dictionary::default(),
            policy: Snapshot::compile(
                &["72836491", "false", "1.2300"].map(|value| super::super::policy::Literal {
                    value,
                    source: Source::RepositoryUser,
                }),
                &[],
                false,
            )
            .unwrap(),
            key: None,
            complete: true,
            missing: None,
            hydration_growth: Default::default(),
        };
        for scalar in [json!(72836491), json!(false), json!(1.23)] {
            let record = json!({"type":"assistant","uuid":"72836491","isMeta":false,
                "message":{"role":"assistant","usage":{"input_tokens":72836491},
                "content":[{"type":"tool_use","id":"call","name":"Inspect","input":{"value":[scalar]}}]}});
            let refused = projector.transform(&record.to_string(), Mode::ProtectJsonl);
            assert!(refused.status == Status::Partial);
            assert_eq!(refused.reason, Some(Reason::UnsupportedScalar));
            assert_eq!(
                serde_json::from_str::<Value>(&refused.content).unwrap(),
                record
            );
        }
        let record = json!({"type":"assistant","uuid":"72836491","isMeta":false,
            "message":{"role":"assistant","usage":{"input_tokens":72836491},"content":"ordinary text"}});
        let accepted = projector.transform(&record.to_string(), Mode::ProtectJsonl);
        assert!(accepted.status == Status::Complete);
        assert_eq!(
            serde_json::from_str::<Value>(&accepted.content).unwrap(),
            record
        );
        let encoded = r#"{ "number" : 72836491, "flag" : false }"#;
        let record = json!({"type":"response_item","payload":{"type":"function_call","call_id":"call","name":"Inspect","arguments":encoded}});
        let protected = projector.transform(&record.to_string(), Mode::ProtectJsonl);
        assert!(protected.status == Status::Complete);
        assert!(!protected.content.contains("72836491"));
        let restored = projector.transform(&protected.content, Mode::HydrateJsonl);
        assert!(restored.status == Status::Complete);
        assert_eq!(
            serde_json::from_str::<Value>(&restored.content).unwrap(),
            record
        );
    }

    /// With the entropy opt-in, a schema word stays a member name wherever a record holds one:
    /// a usage record, a tool's input, and JSON carried as text, with or without escapes. What
    /// such a name holds is still checked, a blocked name is still replaced, and so is an opaque
    /// name. The same word as a value or in prose is not a member name, and is still replaced.
    #[test]
    fn entropy_leaves_schema_word_member_names_and_checks_what_they_hold() {
        let root = tempfile::tempdir().unwrap();
        let word = "cacheCreationInputTokens";
        let opaque = "c7Zq4xR2mNv8LpT3wYb6Ks1Hd9Fg5Jt0";
        let carrier = |arguments: &Value| json!({"type":"response_item","payload":{"type":"function_call","call_id":"call","name":"Inspect","arguments":arguments.to_string()}});
        let mut projector = Projector {
            store: Store::open(&root.path().join("privacy"), None).unwrap(),
            dictionary: Dictionary::default(),
            policy: Snapshot::compile(&[], &[], false)
                .unwrap()
                .with_entropy(crate::domain::privacy::detector::Entropy::Include),
            key: None,
            complete: true,
            missing: None,
            hydration_growth: Default::default(),
        };
        let unchanged = [
            json!({"type":"cost-state","costUSD":0.1,"modelUsage":{"claude-opus-5-5":{"inputTokens":1,"cacheReadInputTokens":3,word:4}}}),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"call","name":"Inspect","input":{word:4}}]}}),
            carrier(&json!({word:4})),
            carrier(&json!({"note":"quoted \" text",word:4})),
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":[{"type":"text","text":json!({word:4}).to_string()}]}]}}),
        ];
        for record in &unchanged {
            let protected = projector.transform(&record.to_string(), Mode::ProtectJsonl);
            assert!(protected.status == Status::Complete);
            assert_eq!(protected.replacements, 0, "{record}");
            let projected: Value = serde_json::from_str(&protected.content).unwrap();
            assert_eq!(&projected, record);
        }
        let held = json!({"type":"cost-state","modelUsage":{"claude-opus-5-5":{word:opaque}}});
        let protected = projector.transform(&held.to_string(), Mode::ProtectJsonl);
        let projected: Value = serde_json::from_str(&protected.content).unwrap();
        let entry = &projected["modelUsage"]["claude-opus-5-5"];
        assert!(entry[word].as_str().is_some_and(|value| value != opaque));
        assert!(!protected.content.contains(opaque));
        for record in [
            json!({"type":"cost-state","modelUsage":{"claude-opus-5-5":{opaque:4}}}),
            carrier(&json!({opaque:4})),
            carrier(&json!({word:opaque})),
        ] {
            let protected = projector.transform(&record.to_string(), Mode::ProtectJsonl);
            assert_eq!(protected.replacements, 1, "{record}");
            assert!(!protected.content.contains(opaque));
        }
        for record in [
            carrier(&json!({"note":word})),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":format!("the {word} field")}]}}),
        ] {
            let protected = projector.transform(&record.to_string(), Mode::ProtectJsonl);
            assert_eq!(protected.replacements, 1, "{record}");
            assert!(!protected.content.contains(word));
        }
        projector.policy = Snapshot::compile(
            &[super::super::policy::Literal {
                value: word,
                source: Source::RepositoryUser,
            }],
            &[],
            false,
        )
        .unwrap()
        .with_entropy(crate::domain::privacy::detector::Entropy::Include);
        for record in &unchanged {
            let protected = projector.transform(&record.to_string(), Mode::ProtectJsonl);
            assert!(protected.replacements >= 1, "{record}");
            assert!(!protected.content.contains(word), "{record}");
        }
        // Beside bare entropy elsewhere in the same text, the blocked name is still a finding of
        // its own: only bare entropy gives way to a schema word.
        let text = json!({word:4,"note":opaque}).to_string();
        let beside = json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":[{"type":"text","text":text}]}]}});
        let protected = projector.transform(&beside.to_string(), Mode::ProtectJsonl);
        assert!(!protected.content.contains(word));
        assert!(!protected.content.contains(opaque));
        let batch = projector.field_findings(&text, None);
        for value in [word, opaque] {
            let at = text.find(value).unwrap();
            assert!(
                batch
                    .findings
                    .iter()
                    .any(|hit| hit.start <= at && at + value.len() <= hit.end),
                "{value}"
            );
        }
        assert!(
            batch
                .findings
                .iter()
                .any(|hit| hit.rule == "high-entropy-value")
        );
    }

    /// Schema member names stay addressable without exempting unknown keys on native-shaped
    /// records. Payload keys and encoded carriers restore exactly, and a colliding rename must
    /// never silently discard either entry or claim complete protection.
    #[test]
    fn payload_keys_restore_without_renaming_schema_or_merging_entries() {
        let root = tempfile::tempdir().unwrap();
        let private = "private-test-\u{516c}\u{53f8}-\u{1f9ea}";
        let escaped = "synthetic-quote-\"slash\\fixture";
        let mut projector = Projector {
            store: Store::open(&root.path().join("privacy"), None).unwrap(),
            dictionary: Dictionary::default(),
            policy: Snapshot::compile(
                &[
                    private,
                    escaped,
                    "message",
                    "type",
                    "source",
                    "data",
                    "blockedliteral",
                ]
                .map(|value| super::super::policy::Literal {
                    value,
                    source: Source::RepositoryUser,
                }),
                &[],
                false,
            )
            .unwrap(),
            key: None,
            complete: true,
            missing: None,
            hydration_growth: Default::default(),
        };
        // Unicode is fixture data exercising decoded JSON member names.
        let data = json!({private: "first", "message": [{"type":"assistant","message":{private: "second"}}]});
        let native = json!({"type":"assistant",private:"root","message":{"role":"assistant",private:"message","content":[{"type":"tool_use","id":"call","name":"Inspect",private:"block","input":data}]},"extra":{"type":"message",private:"nested"}});
        let image = json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":format!("iVBORw0KGgo{}blockedliteral{}", "A".repeat(256), "B".repeat(255))}});
        let result = json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":[{"type":"text","text":"ready"},data,image],"structuredContent":data}]}});
        let mimic =
            json!({"type":"message","id":"item",private:"root","message":{private:"nested"}});
        let encoded = format!(
            "{{ {} : \"value\", \"ordinary\" : 7 }}",
            serde_json::to_string(escaped).unwrap()
        );
        let arguments = json!({"type":"response_item","payload":{"type":"function_call","call_id":"call","name":"Inspect","arguments":encoded}});
        let value_encoded = format!(
            "{{ \"ordinary\" : {} }}",
            serde_json::to_string(escaped).unwrap()
        );
        let argument_value = json!({"type":"response_item","payload":{"type":"function_call","call_id":"call","name":"Inspect","arguments":value_encoded}});
        let array_encoded =
            r#"{ "api_keys" : [["\u00637Zq4xR2mNv8LpT3wYb6Ks1Hd9Fg5Jt0"]], "ordinary" : 7 }"#;
        let argument_array = json!({"type":"response_item","payload":{"type":"function_call","call_id":"call","name":"Inspect","arguments":array_encoded}});
        let ordinary_text = format!(
            "{{ \"ordinary\" : 7, \"note\" : {}, \"references\" : [\"\\u00637Zq4xR2mNv8LpT3wYb6Ks1Hd9Fg5Jt0\"] }}",
            json!("quoted \" benign")
        );
        let ordinary = json!({"type":"response_item","payload":{"type":"function_call","call_id":"call","name":"Inspect","arguments":ordinary_text}});
        let unescaped = json!({"type":"response_item","payload":{"type":"function_call","call_id":"call","name":"Inspect","arguments":"{ \"blockedliteral\" : \"mapping\", \"ordinary\" : 7 }"}});
        let calls = format!(
            "[ {{\"id\": \"call\", \"function\": {{\"name\": \"Inspect\", \"arguments\": {} }} }} ]",
            serde_json::to_string(&encoded).unwrap()
        );
        let nested = json!({"type":"hermes_message","data":{"role":"assistant","session_id":"session","tool_calls":calls}});
        for original in [
            &data,
            &native,
            &result,
            &mimic,
            &arguments,
            &argument_value,
            &argument_array,
            &ordinary,
            &unescaped,
            &nested,
        ] {
            let protected = projector.transform(&original.to_string(), Mode::ProtectJsonl);
            assert!(protected.status == Status::Complete);
            assert!(!protected.content.contains(private));
            let projected: Value = serde_json::from_str(&protected.content).unwrap();
            if original == &native {
                assert!(projected["message"]["content"][0]["input"].is_object());
            }
            if original == &arguments || original == &argument_value || original == &argument_array
            {
                assert!(whole_token(
                    projected["payload"]["arguments"].as_str().unwrap()
                ));
            }
            if original == &ordinary {
                assert_eq!(projected, ordinary);
            }
            if original == &unescaped {
                let data: Value =
                    serde_json::from_str(projected["payload"]["arguments"].as_str().unwrap())
                        .unwrap();
                assert_eq!(data["ordinary"], 7);
                assert!(!data.as_object().unwrap().contains_key("blockedliteral"));
            }
            if original == &nested {
                assert!(whole_token(
                    projected["data"]["tool_calls"].as_str().unwrap()
                ));
            }
            if original == &result {
                assert_eq!(projected["message"]["content"][0]["content"][2], image);
                assert!(projected["message"]["content"][0]["structuredContent"].is_object());
            }
            let restored = projector.transform(&protected.content, Mode::HydrateJsonl);
            assert!(restored.status == Status::Complete);
            assert_eq!(
                serde_json::from_str::<Value>(&restored.content).unwrap(),
                *original
            );
        }
        let token = projector.transform(private, Mode::ProtectText).content;
        let collision = json!({private: "first", token: "second"});
        for mode in [Mode::ProtectJsonl, Mode::HydrateJsonl] {
            let output = projector.transform(&collision.to_string(), mode);
            assert!(output.status == Status::Partial);
            assert_eq!(output.reason, Some(Reason::ProtocolError));
            assert_eq!(
                serde_json::from_str::<Value>(&output.content).unwrap(),
                collision
            );
        }
    }

    /// A value first replaced under a legacy spelling gains a typed token for new content, and
    /// the legacy token keeps restoring. Reusing the legacy record would keep minting long
    /// tokens; dropping it would strand published history.
    #[test]
    fn new_replacements_use_typed_tokens_while_legacy_tokens_restore() {
        let root = tempfile::tempdir().unwrap();
        let mut projector = Projector {
            store: Store::open(&root.path().join("privacy"), None).unwrap(),
            dictionary: Dictionary::default(),
            policy: Snapshot::compile(&[], &[], false).unwrap(),
            key: None,
            complete: true,
            missing: None,
            hydration_growth: Default::default(),
        };
        let secret = "ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
        let legacy = Record {
            token: format!(
                "{{{{AGIT_SECRET_V2:{}:{}}}}}",
                projector.store.dictionary_id,
                uuid::Uuid::new_v4()
            ),
            original: secret.to_owned(),
            origins: [Origin::Heuristic].into(),
        };
        let legacy_token = legacy.token.clone();
        projector
            .store
            .append(std::slice::from_ref(&legacy), None)
            .unwrap();
        projector.dictionary.accept(legacy).unwrap();
        let protected = projector.transform(&format!("token {secret}"), Mode::ProtectText);
        let (start, end, token) = tokens(&protected.content).next().unwrap();
        assert_eq!(
            protected.content[..start].to_owned() + &protected.content[end..],
            "token "
        );
        assert_eq!(
            placeholder::typed_parts(token).map(|(class, kind, _)| (class, kind)),
            Some(("secret", "github-pat"))
        );
        for spelling in [token, legacy_token.as_str()] {
            assert_eq!(
                projector.transform(spelling, Mode::HydrateText).content,
                secret
            );
        }
    }

    /// A base64 media payload passes through byte for byte even when a run inside it has a
    /// credential's shape, while the same run in a text field, or in a base64-shaped `data` field
    /// of ordinary tool input, is replaced; and a field holding more findings than one pass
    /// replaces becomes one placeholder, Complete. Scanning media would corrupt the image;
    /// exempting every `data` field by its name and alphabet would publish a blocked value from
    /// tool data; and a capped pass that stopped early would leave the rest of the field raw or
    /// hold its record back for good.
    #[test]
    fn media_stays_opaque_and_a_capped_field_is_replaced_whole() {
        let root = tempfile::tempdir().unwrap();
        let run = "agitblockedliteral";
        let mut projector = Projector {
            store: Store::open(&root.path().join("privacy"), None).unwrap(),
            dictionary: Dictionary::default(),
            policy: Snapshot::compile(
                &[super::super::policy::Literal {
                    value: run,
                    source: Source::GlobalUser,
                }],
                &[],
                false,
            )
            .unwrap(),
            key: None,
            complete: true,
            missing: None,
            hydration_growth: Default::default(),
        };
        let data = format!("iVBORw0KGgo{}{run}{}", "A".repeat(300), "B".repeat(299));
        let record = json!({"type":"user","message":{"role":"user","content":[
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":data}},
            {"type":"text","text":format!("see {run}")},
            {"type":"text","text":format!("data:font/ttf;base64,{data}")},
            {"type":"tool_use","id":"toolu_1","name":"upload","input":{"data":format!("{run}{}", "A".repeat(300))}}
        ]}});
        let protected = projector.transform(&record.to_string(), Mode::ProtectJsonl);
        let value: Value = serde_json::from_str(&protected.content).unwrap();
        assert_eq!(
            value["message"]["content"][0],
            record["message"]["content"][0]
        );
        assert_eq!(
            value["message"]["content"][2],
            record["message"]["content"][2]
        );
        for text in [
            &value["message"]["content"][1]["text"],
            &value["message"]["content"][3]["input"]["data"],
        ] {
            assert!(!text.as_str().unwrap().contains(run), "{text}");
        }

        let crowded = vec![run; MAX_FINDINGS + 1].join(" ");
        let capped = projector.transform(&crowded, Mode::ProtectText);
        assert!(capped.status == Status::Complete);
        assert!(whole_token(&capped.content), "{}", capped.content);

        use base64::Engine as _;
        let bytes: Vec<_> = (0u8..=255).cycle().take(1024).collect();
        let binary = base64::engine::general_purpose::STANDARD.encode(bytes);
        let literal = &binary[16..40];
        projector.policy = Snapshot::compile(
            &[super::super::policy::Literal {
                value: literal,
                source: Source::GlobalUser,
            }],
            &[],
            false,
        )
        .unwrap();
        let payload = json!({"source":{"type":"base64","media_type":"image/png","data":binary},"outside":literal,(literal):"mapping","ordinary":"visible","data":binary});
        for arguments in [
            payload.clone(),
            json!(serde_json::to_string_pretty(&payload).unwrap()),
        ] {
            let record = json!({"type":"response_item","payload":{"type":"function_call","call_id":"call","arguments":arguments}});
            let protected = projector.transform(&record.to_string(), Mode::ProtectJsonl);
            assert!(protected.status == Status::Complete);
            let projected: Value = serde_json::from_str(&protected.content).unwrap();
            let data = &projected["payload"]["arguments"];
            let data = data
                .as_str()
                .map(|text| serde_json::from_str::<Value>(text).unwrap())
                .unwrap_or_else(|| data.clone());
            assert_eq!(data["source"]["data"], binary);
            assert_eq!(data["ordinary"], "visible");
            assert_ne!(data["outside"], literal);
            assert_ne!(data["data"], binary);
            assert!(!data.as_object().unwrap().contains_key(literal));
            let restored = projector.transform(&protected.content, Mode::HydrateJsonl);
            assert!(restored.status == Status::Complete);
            assert_eq!(
                serde_json::from_str::<Value>(&restored.content).unwrap(),
                record
            );
        }
    }

    /// Text a record carries as base64 is checked as the text it is, at any length and whatever
    /// type it is declared as, in each carrier shape: a data URI, whole or inside longer text as a
    /// tool prints one, a base64 source, an MCP result's `data` and `blob` beside `mimeType`, and
    /// a file's `data` beside `mime`. A value protection replaces is replaced inside the decoded
    /// text, which is encoded again, and restoring gives back the record. A payload with nothing
    /// to replace keeps its bytes, line wrapping included, and text whose bytes aren't UTF-8 is
    /// not published. A rule that took any
    /// base64 for media, or that knew only some carriers, would send the value encoded.
    #[test]
    fn text_carried_as_base64_is_checked_as_text() {
        use base64::Engine as _;
        let engine = base64::engine::general_purpose::STANDARD;
        let root = tempfile::tempdir().unwrap();
        let run = "agitblockedliteral";
        let mut projector = Projector {
            store: Store::open(&root.path().join("privacy"), None).unwrap(),
            dictionary: Dictionary::default(),
            policy: Snapshot::compile(
                &[super::super::policy::Literal {
                    value: run,
                    source: Source::GlobalUser,
                }],
                &[],
                false,
            )
            .unwrap(),
            key: None,
            complete: true,
            missing: None,
            hydration_growth: Default::default(),
        };
        let file = engine.encode(format!("API_KEY={run}\n"));
        let clean = engine.encode("nothing to see here, ".repeat(20));
        let wrapped = clean
            .as_bytes()
            .chunks(76)
            .map(|line| std::str::from_utf8(line).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let unreadable = engine.encode([0xff, 0xfe, 0xfd, 0x80]);
        let record = json!({"type":"user","message":{"role":"user","content":[
            {"type":"text","text":format!("data:text/plain;base64,{file}")},
            {"type":"document","source":{"type":"base64","media_type":"text/plain","data":file}},
            {"type":"image","mimeType":"image/png","data":file},
            {"type":"resource","resource":{"uri":"file:///work/.env","mimeType":"text/plain","blob":file}},
            {"type":"file","mime":"text/plain","data":file},
            {"type":"text","text":format!("data:text/plain;base64,{wrapped}")},
            {"type":"file","mime":"text/plain","data":unreadable},
            {"type":"text","text":format!("the build printed <script src=\"data:text/javascript;base64,{file}\"></script> and exited")},
            {"type":"text","text":format!("before data:text/plain;base64,{unreadable} after")},
        ]}});
        let protected = projector.transform(&record.to_string(), Mode::ProtectJsonl);
        assert!(
            protected.status == Status::Complete,
            "{}",
            protected.content
        );
        let value: Value = serde_json::from_str(&protected.content).unwrap();
        let content = &value["message"]["content"];
        // Line breaks inside a payload are read through, so a payload published still wrapped
        // fails on what it holds rather than on its shape.
        let decoded = |encoded: &str| {
            let encoded: String = encoded
                .strip_prefix("data:text/plain;base64,")
                .unwrap_or(encoded)
                .split_ascii_whitespace()
                .collect();
            String::from_utf8(engine.decode(encoded).unwrap()).unwrap()
        };
        for carried in [
            &content[0]["text"],
            &content[1]["source"]["data"],
            &content[2]["data"],
            &content[3]["resource"]["blob"],
            &content[4]["data"],
        ] {
            let text = decoded(carried.as_str().unwrap());
            assert!(
                text.starts_with("API_KEY=") && !text.contains(run),
                "{carried}: {text}"
            );
        }
        assert_eq!(content[5], record["message"]["content"][5]);
        assert_eq!(content[6]["data"], UNREADABLE_TEXT);
        let printed = content[7]["text"].as_str().unwrap();
        let span = printed
            .strip_prefix("the build printed <script src=\"data:text/javascript;base64,")
            .and_then(|rest| rest.strip_suffix("\"></script> and exited"))
            .expect("the text around the payload stays");
        let text = decoded(span);
        assert!(
            text.starts_with("API_KEY=") && !text.contains(run),
            "{text}"
        );
        assert_eq!(
            content[8]["text"],
            format!("before {UNREADABLE_TEXT} after")
        );

        let restored = projector.transform(&protected.content, Mode::HydrateJsonl);
        let restored: Value = serde_json::from_str(&restored.content).unwrap();
        for index in [0, 1, 2, 3, 4, 5, 7] {
            assert_eq!(
                restored["message"]["content"][index], record["message"]["content"][index],
                "{index}"
            );
        }

        // Text protected and restored as text, rather than as a record, keeps the same rule.
        let printed = format!("the build printed data:text/javascript;base64,{file} and exited");
        let protected = projector.transform(&printed, Mode::ProtectText);
        let span = protected
            .content
            .strip_prefix("the build printed data:text/javascript;base64,")
            .and_then(|rest| rest.strip_suffix(" and exited"))
            .expect("the text around the payload stays");
        let text = decoded(span);
        assert!(
            text.starts_with("API_KEY=") && !text.contains(run),
            "{text}"
        );
        let restored = projector.transform(&protected.content, Mode::HydrateText);
        assert_eq!(restored.content, printed);

        // A payload a tool wrapped onto the next line goes on there, and the text after it stays.
        let wrapped = engine.encode(format!("prefix {run} suffix"));
        let (first, second) = wrapped.split_at(12);
        let record = json!({"type":"text","text":format!("tool output: data:text/plain;base64,{first}\n{second} end")});
        let protected = projector.transform(&record.to_string(), Mode::ProtectJsonl);
        let value: Value = serde_json::from_str(&protected.content).unwrap();
        let span = value["text"]
            .as_str()
            .unwrap()
            .strip_prefix("tool output: data:text/plain;base64,")
            .and_then(|rest| rest.strip_suffix(" end"))
            .expect("the text around the payload stays");
        let text = decoded(span);
        assert!(text.starts_with("prefix ") && !text.contains(run), "{text}");

        // Plain text after a wrapped payload doesn't end the payload at its first line.
        let tail = engine.encode(format!("prefix {run} tail"));
        let (first, second) = tail.split_at(12);
        let record = json!({"type":"text","text":format!("tool output: data:text/plain;base64,{first}\n{second}\nDone.")});
        let protected = projector.transform(&record.to_string(), Mode::ProtectJsonl);
        let value: Value = serde_json::from_str(&protected.content).unwrap();
        let span = value["text"]
            .as_str()
            .unwrap()
            .strip_prefix("tool output: data:text/plain;base64,")
            .and_then(|rest| rest.strip_suffix("\nDone."))
            .expect("the text around the payload stays");
        let text = decoded(span);
        assert!(text.starts_with("prefix ") && !text.contains(run), "{text}");

        // Bytes left out are a change on their own: the record is written without them. A payload
        // declared as text that doesn't decode is left out too, wrapped past what is tried or
        // cut short, since part of it may still decode to a value.
        let lines = "\nZ".repeat(WRAP_TRIES + 1);
        for record in [
            json!({"type":"file","mime":"text/plain","data":unreadable}),
            json!({"type":"text","text":format!("before data:text/plain;base64,{unreadable} after")}),
            json!({"type":"text","text":format!("tool output: data:text/plain;base64,{first}\n{second}{lines}")}),
            json!({"type":"text","text":format!("tool output: data:text/plain;base64,{tail}Z")}),
            json!({"type":"file","mime":"text/plain","data":format!("{tail}Z")}),
        ] {
            let protected = projector.transform(&record.to_string(), Mode::ProtectJsonl);
            assert!(
                protected.content.contains(UNREADABLE_TEXT)
                    && !protected.content.contains(&unreadable)
                    && !protected.content.contains(first),
                "{}",
                protected.content
            );
        }
    }

    /// Native linkage and runtime-opaque carriers survive every projection entry point, even
    /// with entropy replacement opted in. The same bytes in arguments or results remain
    /// replaceable, so a field-name allowlist cannot satisfy this contract.
    #[test]
    fn protocol_identifiers_survive_projection_without_exempting_tool_data() {
        let root = tempfile::tempdir().unwrap();
        let mut projector = Projector {
            store: Store::open(&root.path().join("privacy"), None).unwrap(),
            dictionary: Dictionary::default(),
            policy: Snapshot::compile(&[], &[], false)
                .unwrap()
                .with_entropy(crate::domain::privacy::detector::Entropy::Include),
            key: None,
            complete: true,
            missing: None,
            hydration_growth: Default::default(),
        };
        let id = "call_x7Qp9Ls2Vn4Rm8Tc6Yz3Ba1W";
        let fixtures = [
            (
                "codex",
                json!({"type":"session_meta","payload":{"id":id,"parent_thread_id":id,"source":{"subagent":{"thread_spawn":{"parent_thread_id":id}}},"note":id}}),
                "/payload/source/subagent/thread_spawn/parent_thread_id",
                "/payload/note",
            ),
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
                "codex",
                json!({"type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":id}],"encrypted_content":id}}),
                "/payload/encrypted_content",
                "/payload/summary",
            ),
            (
                "codex",
                json!({"type":"response_item","payload":{"type":"agent_message","id":id,"content":[{"type":"encrypted_content","encrypted_content":id},{"type":"text","text":id}]}}),
                "/payload/content/0/encrypted_content",
                "/payload/content/1/text",
            ),
            (
                "claude-code",
                json!({"type":"assistant","message":{"role":"assistant","id":id,"content":[{"type":"tool_use","id":id,"name":"Read","input":{"id":id, (id):"mapping"}}]}}),
                "/message/content/0/id",
                "/message/content/0/input",
            ),
            (
                "claude-code",
                json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":id,"signature":id}]}}),
                "/message/content/0/signature",
                "/message/content/0/thinking",
            ),
            (
                "claude-desktop",
                json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":id}]}}),
                "/message/content/0/tool_use_id",
                "/message/content/0/content",
            ),
            (
                "opencode",
                json!({"kind":"part","id":"prt_fixture","session_id":"ses_fixture","message_id":id,"data":{"type":"tool","callID":id,"state":{"input":{"id":id, (id):"mapping"}}}}),
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
                json!({"type":"message","id":"entry","parentId":id,"message":{"role":"assistant","content":[{"type":"toolCall","id":id,"name":"read","arguments":{"id":id, (id):"mapping"}}]}}),
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
                json!({"type":"function_call","sessionId":"session","callId":id,"name":"Read","arguments":{"id":id, (id):"mapping"}}),
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
                if original["payload"]["type"] == "agent_message" {
                    assert_eq!(record["payload"]["id"], id);
                }
                let body = record.pointer(data).unwrap().to_string();
                assert!(!body.contains(id), "{runtime}: {data}");
                assert!(body.contains("\u{2039}secret:"), "{runtime}: {data}");
                let restored = projector.transform(&record.to_string(), Mode::HydrateJsonl);
                assert!(restored.status == Status::Complete, "{runtime}");
                assert_eq!(
                    serde_json::from_str::<Value>(&restored.content).unwrap(),
                    original,
                    "{runtime}: payload keys and values must restore together"
                );
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
        .unwrap()
        .with_entropy(crate::domain::privacy::detector::Entropy::Include);
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
        // Neither a credential signature nor an explicit block rewrites a protocol identifier.
        let credential = json!({"type":"response_item","payload":{"type":"function_call","call_id":"AKIA2E7YQXK4NMZ5VJ3T","name":"read","arguments":"{}"}});
        let protected: Value = serde_json::from_str(
            &projector
                .transform(&credential.to_string(), Mode::ProtectJsonl)
                .content,
        )
        .unwrap();
        assert_eq!(protected, credential);
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
        let protected: Value = serde_json::from_str(
            &projector
                .transform(&blocked.to_string(), Mode::ProtectJsonl)
                .content,
        )
        .unwrap();
        assert_eq!(protected, blocked);

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
                .starts_with("\u{2039}secret:blocked:")
        );
    }

    /// A restoration from a dictionary that loaded incomplete is partial and carries the reason
    /// its loader knows through the whole JSONL outcome. One that dropped it would be reported
    /// as the generic `source_incomplete`, which suggests retrying, for a cause no retry changes.
    #[test]
    fn an_incomplete_restoration_carries_its_loaders_reason() {
        let root = tempfile::tempdir().unwrap();
        let mut projector = Projector {
            store: Store::open(&root.path().join("privacy"), None).unwrap(),
            dictionary: Dictionary::default(),
            policy: Snapshot::compile(&[], &[], false).unwrap(),
            key: None,
            complete: false,
            missing: Some(Reason::LegacyRules),
            hydration_growth: Default::default(),
        };
        let restored =
            projector.transform("{\"text\":\"plain\"}\nplain line\n", Mode::HydrateJsonl);
        assert!(restored.status == Status::Partial);
        assert_eq!(restored.reason, Some(Reason::LegacyRules));
    }

    fn blocking(
        value: &'static str,
        entropy: crate::domain::privacy::detector::Entropy,
    ) -> (tempfile::TempDir, Projector) {
        let root = tempfile::tempdir().unwrap();
        let projector = Projector {
            store: Store::open(&root.path().join("privacy"), None).unwrap(),
            dictionary: Dictionary::default(),
            policy: Snapshot::compile(
                &[super::super::policy::Literal {
                    value,
                    source: Source::RepositoryUser,
                }],
                &[],
                false,
            )
            .unwrap()
            .with_entropy(entropy),
            key: None,
            complete: true,
            missing: None,
            hydration_growth: Default::default(),
        };
        (root, projector)
    }

    const CALL: &str = "call_V9qB3nR7mK2xH8pL4sW6cF1a";

    /// A Codex call whose own call_id is a blocked value keeps that call_id, and its arguments
    /// keep every byte but the blocked occurrence, which becomes a placeholder where it stands:
    /// they stay JSON with their own spelling, so the call still reads and pairs with its result,
    /// and hydration gives back the record byte for byte. That holds with and without entropy
    /// replacement. Replacing the arguments whole leaves the call with an input no runtime can
    /// read back.
    #[test]
    fn a_blocked_call_id_keeps_the_call_and_its_encoded_arguments() {
        let arguments =
            format!("{{\"cmd\":\"printf '%s\\\\n' '{CALL}'\",\"workdir\":\"/workspace/demo\"}}");
        let call = json!({"type":"response_item","payload":{"type":"function_call","call_id":CALL,"name":"exec_command","arguments":arguments}});
        let output = json!({"type":"response_item","payload":{"type":"function_call_output","call_id":CALL,"output":format!("Private correlation value: {CALL}")}});
        let input = format!("{call}\n{output}\n");
        for entropy in [
            crate::domain::privacy::detector::Entropy::Exclude,
            crate::domain::privacy::detector::Entropy::Include,
        ] {
            let (_root, mut projector) = blocking(CALL, entropy);
            let protected = projector.transform(&input, Mode::ProtectJsonl);
            assert!(protected.status == Status::Complete, "{entropy:?}");
            let records: Vec<Value> = protected
                .content
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(records[0]["payload"]["call_id"], CALL, "{entropy:?}");
            assert_eq!(records[1]["payload"]["call_id"], CALL, "{entropy:?}");
            let encoded = records[0]["payload"]["arguments"].as_str().unwrap();
            let (before, after) = arguments.split_once(CALL).unwrap();
            let token = encoded
                .strip_prefix(before)
                .and_then(|rest| rest.strip_suffix(after))
                .unwrap_or_else(|| panic!("{entropy:?}: {encoded}"));
            assert!(whole_token(token), "{entropy:?}: {encoded}");
            let restored = projector.transform(&protected.content, Mode::HydrateJsonl);
            assert_eq!(restored.content, input, "{entropy:?}");
        }
    }

    /// A value written once plainly and once escaped (`\u0063` for its first letter) can't be
    /// replaced where it stands: the escaped occurrence is no literal run of its bytes. The
    /// arguments are then replaced whole, as before, so neither occurrence reaches the record, and
    /// hydration still gives them back byte for byte. Replacing only the plain occurrence leaves
    /// the value in the decoded arguments.
    #[test]
    fn a_value_also_written_escaped_falls_back_to_the_whole_arguments() {
        let arguments = format!(
            "{{\"plain\":\"{CALL}\",\"escaped\":\"\\u0063{}\"}}",
            &CALL[1..]
        );
        assert_eq!(
            serde_json::from_str::<Value>(&arguments).unwrap()["escaped"],
            CALL
        );
        let call = json!({"type":"response_item","payload":{"type":"function_call","call_id":"call_other","name":"exec_command","arguments":arguments}});
        let input = format!("{call}\n");
        let (_root, mut projector) =
            blocking(CALL, crate::domain::privacy::detector::Entropy::Exclude);
        let protected = projector.transform(&input, Mode::ProtectJsonl);
        let record: Value = serde_json::from_str(protected.content.trim_end()).unwrap();
        let encoded = record["payload"]["arguments"].as_str().unwrap();
        assert!(!protected.content.contains(&CALL[1..]), "{encoded}");
        assert!(
            serde_json::from_str::<Value>(encoded)
                .map_or(true, |decoded| !decoded.to_string().contains(&CALL[1..])),
            "{encoded}"
        );
        let restored = projector.transform(&protected.content, Mode::HydrateJsonl);
        assert_eq!(restored.content, input);
    }

    /// A Hermes record's tool calls, one JSON-encoded carrier, hold two calls: the first with a
    /// blocked value, the second with a value only its key (`password`) marks as a credential,
    /// and no backslash anywhere. Each call's values are checked with their keys, so neither value
    /// appears raw or decoded, and hydration restores the record byte for byte. Replacing values
    /// where they stand in this carrier finds the blocked value but leaves the second, which only
    /// the key around it reveals.
    #[test]
    fn a_key_marked_value_in_a_hermes_carrier_is_replaced() {
        const PASSWORD: &str = "Kq7mTz9vLp2wXr4n";
        let calls = json!([
            {"id":"call_1","function":{"name":"read","arguments":{"note":CALL}}},
            {"id":"call_2","function":{"name":"login","arguments":{"password":PASSWORD}}}
        ])
        .to_string();
        assert!(!calls.contains('\\'), "{calls}");
        let record =
            json!({"type":"hermes_message","data":{"role":"assistant","tool_calls":calls}});
        let input = format!("{record}\n");
        let (_root, mut projector) =
            blocking(CALL, crate::domain::privacy::detector::Entropy::Exclude);
        let protected = projector.transform(&input, Mode::ProtectJsonl);
        let carrier = serde_json::from_str::<Value>(protected.content.trim_end()).unwrap()["data"]
            ["tool_calls"]
            .as_str()
            .unwrap()
            .to_owned();
        for value in [CALL, PASSWORD] {
            assert!(!protected.content.contains(value), "{carrier}");
            if let Ok(decoded) = serde_json::from_str::<Value>(&carrier) {
                assert!(!decoded.to_string().contains(value), "{carrier}");
            }
        }
        let restored = projector.transform(&protected.content, Mode::HydrateJsonl);
        assert_eq!(restored.content, input);
    }

    /// The members of an encoded call are read by name, not in the order they sit in the text:
    /// `a` is read before `z` though it comes after. A value in both, with the later-read one
    /// holding an escape, is replaced where it stands without slicing backwards.
    #[test]
    fn values_in_members_out_of_name_order_are_replaced_where_they_stand() {
        let arguments = format!("{{\"z\":\"{CALL}\",\"a\":\"{CALL}\\n\"}}");
        let call = json!({"type":"response_item","payload":{"type":"function_call","call_id":"call_other","name":"exec_command","arguments":arguments}});
        let input = format!("{call}\n");
        let (_root, mut projector) =
            blocking(CALL, crate::domain::privacy::detector::Entropy::Exclude);
        let protected = projector.transform(&input, Mode::ProtectJsonl);
        assert!(protected.status == Status::Complete);
        let record: Value = serde_json::from_str(protected.content.trim_end()).unwrap();
        let encoded = record["payload"]["arguments"].as_str().unwrap();
        assert!(!protected.content.contains(CALL), "{encoded}");
        let decoded: Value = serde_json::from_str(encoded).unwrap();
        assert!(decoded["z"].as_str().is_some_and(whole_token), "{encoded}");
        assert!(decoded["a"].as_str().unwrap().ends_with('\n'), "{encoded}");
        let restored = projector.transform(&protected.content, Mode::HydrateJsonl);
        assert_eq!(restored.content, input);
    }

    /// An object that names a member twice reads as its last one only, so a check of the decoded
    /// arguments never sees the first. Here the first holds either the blocked value with an
    /// escaped first letter, which no raw match finds, or a value only its key (`password`)
    /// marks; the last is harmless. Such arguments are replaced whole instead, and the value is
    /// nowhere in them, raw or with its escape undone.
    #[test]
    fn a_member_named_twice_falls_back_to_the_whole_arguments() {
        const PASSWORD: &str = "Kq7mTz9vLp2wXr4n";
        for (name, first, hidden) in [
            ("shadow", format!("\\u0063{}", &CALL[1..]), &CALL[1..]),
            ("password", PASSWORD.to_owned(), PASSWORD),
        ] {
            let arguments =
                format!("{{\"{name}\":\"{first}\",\"{name}\":\"safe\",\"note\":\"{CALL}\\n\"}}");
            let call = json!({"type":"response_item","payload":{"type":"function_call","call_id":"call_other","name":"exec_command","arguments":arguments}});
            let input = format!("{call}\n");
            let (_root, mut projector) =
                blocking(CALL, crate::domain::privacy::detector::Entropy::Exclude);
            let protected = projector.transform(&input, Mode::ProtectJsonl);
            let record: Value = serde_json::from_str(protected.content.trim_end()).unwrap();
            let encoded = record["payload"]["arguments"].as_str().unwrap();
            assert!(!protected.content.contains(hidden), "{name}: {encoded}");
            let restored = projector.transform(&protected.content, Mode::HydrateJsonl);
            assert_eq!(restored.content, input, "{name}");
        }
    }
}
