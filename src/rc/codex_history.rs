//! Native history mode belongs to the same open source as the records being projected.

use serde::Deserialize;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};

const HEADER_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum HistoryMode {
    #[default]
    Model,
    Legacy,
    Paginated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Header {
    Pending,
    Ready(HistoryMode),
}

impl Header {
    pub(crate) fn mode(self) -> HistoryMode {
        match self {
            Self::Pending => HistoryMode::Model,
            Self::Ready(mode) => mode,
        }
    }
}

#[derive(Deserialize)]
struct Metadata {
    #[serde(rename = "type")]
    kind: String,
    payload: Fields,
}

#[derive(Deserialize)]
struct Fields {
    id: String,
    #[serde(default)]
    originator: String,
    #[serde(default)]
    cli_version: String,
    #[serde(default)]
    history_mode: ModeField,
}

#[derive(Default)]
enum ModeField {
    #[default]
    Absent,
    Present(String),
}

impl<'de> Deserialize<'de> for ModeField {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::Present)
    }
}

pub(crate) fn read_header(source: &mut std::fs::File) -> Header {
    let Ok(position) = source.stream_position() else {
        return Header::Ready(HistoryMode::Model);
    };
    let header = if source.seek(SeekFrom::Start(0)).is_ok() {
        read_prefix(&mut *source)
    } else {
        Header::Ready(HistoryMode::Model)
    };
    let _ = source.seek(SeekFrom::Start(position));
    header
}

fn read_prefix(source: impl Read) -> Header {
    let mut bytes = Vec::new();
    if BufReader::new(source.take(HEADER_BYTES))
        .read_until(b'\n', &mut bytes)
        .is_err()
    {
        return Header::Ready(HistoryMode::Model);
    }
    if !bytes.ends_with(b"\n") {
        return if bytes.len() as u64 == HEADER_BYTES {
            Header::Ready(HistoryMode::Model)
        } else {
            Header::Pending
        };
    }
    let Ok(header) = serde_json::from_slice::<Metadata>(&bytes) else {
        return Header::Ready(HistoryMode::Model);
    };
    if header.kind != "session_meta" || header.payload.id.is_empty() {
        return Header::Ready(HistoryMode::Model);
    }
    let fields = header.payload;
    let mode = match &fields.history_mode {
        ModeField::Present(mode) if mode == "legacy" => HistoryMode::Legacy,
        ModeField::Present(mode) if mode == "paginated" => HistoryMode::Paginated,
        // Imported model-only histories cannot promise native user-message records.
        ModeField::Absent
            if !fields.originator.is_empty()
                && fields.originator != "agit"
                && !fields.cli_version.is_empty() =>
        {
            HistoryMode::Legacy
        }
        _ => HistoryMode::Model,
    };
    Header::Ready(mode)
}

pub(crate) fn events(record: &serde_json::Value, mode: HistoryMode) -> Vec<crate::adapter::Event> {
    use crate::adapter::{Event, EventKind};
    let text = canonical_user(record, mode).and_then(|user| match mode {
        HistoryMode::Legacy => user
            .get("message")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        HistoryMode::Paginated => user
            .get("content")
            .and_then(serde_json::Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter(|part| part["type"] == "text")
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            }),
        HistoryMode::Model => None,
    });
    if let Some(text) = text {
        return vec![Event::text(
            EventKind::UserPrompt,
            text,
            record
                .get("timestamp")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        )];
    }
    let mut events = crate::adapter::codex::parse_records([(0, record)]).events;
    if mode != HistoryMode::Model && record["type"] == "compacted" {
        // Retained model context is not newly spoken conversation in the native UI.
        events.retain(|event| event.kind == EventKind::CompactFiltered);
    }
    if mode != HistoryMode::Model
        && record["type"] == "response_item"
        && record["payload"]["type"] == "message"
        && record["payload"]["role"] == "user"
    {
        // Native UI records own user messages; model-only context summaries remain distinct.
        events.retain(|event| {
            !matches!(
                event.kind,
                EventKind::UserPrompt | EventKind::UserInterjection
            )
        });
    }
    events
}

fn canonical_user(record: &serde_json::Value, mode: HistoryMode) -> Option<&serde_json::Value> {
    if record["type"] != "event_msg" {
        return None;
    }
    let payload = record.get("payload")?;
    match mode {
        HistoryMode::Legacy if payload["type"] == "user_message" => Some(payload),
        HistoryMode::Paginated
            if payload["type"] == "item_completed" && payload["item"]["type"] == "UserMessage" =>
        {
            payload.get("item")
        }
        _ => None,
    }
}

/// Only a parsed canonical UUID may enter the correlation metadata after redaction.
pub(crate) fn prompt_identity(record: &serde_json::Value, mode: HistoryMode) -> Option<String> {
    let id = canonical_user(record, mode)?.get("client_id")?.as_str()?;
    let parsed = uuid::Uuid::parse_str(id).ok()?;
    (parsed.to_string() == id).then(|| id.to_owned())
}

pub(crate) fn prompt_identity_pointer(
    record: &serde_json::Value,
    mode: HistoryMode,
) -> Option<&'static str> {
    prompt_identity(record, mode)?;
    match mode {
        HistoryMode::Legacy => Some("/payload/client_id"),
        HistoryMode::Paginated => Some("/payload/item/client_id"),
        HistoryMode::Model => None,
    }
}

#[cfg(test)]
mod tests;
