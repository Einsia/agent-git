//! Which native records a published session carries.
//!
//! Publication carries the conversation only: the words the person typed or pasted, the agent's
//! replies and reasoning, and tool calls with their results. Everything a runtime records about
//! itself or puts into the model's context on its own is left out, whatever its type is called,
//! and so is any record this module does not know. A record is judged by where it came from, not
//! by the role it is filed under: a runtime that writes context in the user's role marks it, and
//! where no runtime marker tells the person's words apart, that runtime's user messages are left
//! out until one does.
//!
//! [`Classifier::published`] is the one definition: the record as it is published, or `None`. A
//! kept record can lose the blocks the runtime injected into it. [`Classifier::class`] is derived
//! from it, so the CLI that omits a record and the Hub that refuses or hides it apply the same
//! rule. A session's records are judged in their LOG order, by one classifier: an OpenCode part
//! takes its role from the message header before it. [`record_kind`] reads what a record
//! contributes to that judgement once, and [`Classifier::class_of`] folds the kinds in order;
//! [`Classifier::class`] is that fold over one record.
//!
//! Local history and resume keep the complete native transcript; only publication applies this.

use serde_json::{Map, Value};
use std::collections::HashMap;

/// What a native record is, for publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordClass {
    /// The conversation itself, published as it is.
    Conversation,
    /// Context or bookkeeping the runtime recorded or injected; never published.
    RuntimeContext,
    /// A copy of what was sent to the model; never published.
    RequestSnapshot,
}

impl RecordClass {
    /// The name a refusal or a report shows.
    pub fn label(self) -> &'static str {
        match self {
            RecordClass::Conversation => "conversation record",
            RecordClass::RuntimeContext => "runtime context record",
            RecordClass::RequestSnapshot => "request snapshot",
        }
    }
}

/// The registered runtimes this module knows. A source not listed here publishes nothing.
pub const RUNTIMES: &[&str] = &[
    "claude-code",
    "claude-desktop",
    "codex",
    "cursor",
    "opencode",
    "hermes",
    "openclaw",
    "workbuddy",
];

/// The runtimes whose user messages are published with only the request inside their
/// `<user_query>` wrapper; a user message without one is left out.
pub const USER_QUERY_ONLY: &[&str] = &["cursor", "workbuddy"];

/// Judges one session's records in their LOG order.
#[derive(Debug, Default)]
pub struct Classifier {
    /// OpenCode message ids and their roles, from the message headers seen so far.
    opencode_roles: HashMap<String, String>,
    /// The same for the ordered judgement of kinds: each message id's hash, and its role.
    message_roles: HashMap<u64, Host>,
    /// Whether the Hermes session header seen last says the owner's own console.
    hermes_local: bool,
}

/// The role of the OpenCode message a part belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Host {
    Assistant,
    User,
    /// Any other role, or a message whose header was not seen.
    Other,
}

impl Host {
    fn of(role: Option<&str>) -> Self {
        match role {
            Some("assistant") => Host::Assistant,
            Some("user") => Host::User,
            _ => Host::Other,
        }
    }
}

/// What one record contributes to the ordered judgement of a session, read from the record
/// alone. It holds no text and no id, only hashes of ids, flags and classes, so a reader can keep
/// one per record and judge a session's LOG again without reading its records again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordKind(Kind);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A class that does not depend on the records around it.
    Fixed(RecordClass),
    /// An OpenCode message header: its id's hash, its role, and its own class.
    OpencodeHeader {
        message: Option<u64>,
        host: Host,
        class: RecordClass,
    },
    /// A Hermes session header: whether the session is the owner's own console.
    HermesSession { local: bool, class: RecordClass },
    /// A Hermes row: its class in a session from the owner's own console, and in any other.
    HermesRow {
        local: RecordClass,
        otherwise: RecordClass,
    },
    /// An OpenCode part: its message id's hash, and its class under an assistant message, under
    /// a user message and under any other.
    OpencodePart {
        message: Option<u64>,
        under_assistant: RecordClass,
        under_user: RecordClass,
        otherwise: RecordClass,
    },
}

/// The kind of `content`, a native record of runtime `source`.
pub fn record_kind(source: &str, content: &Value) -> RecordKind {
    let fixed = |source: &str| {
        RecordKind(Kind::Fixed(class_from(
            Classifier::default().published(source, content),
            content,
        )))
    };
    if source == "hermes"
        && let Some(record) = content
            .as_object()
            .filter(|record| !is_agit_marker(record) && recovered_evidence(record).is_none())
    {
        return RecordKind(match hermes_local_session(record) {
            Some(local) => Kind::HermesSession {
                local,
                class: class_from(None, content),
            },
            None => Kind::HermesRow {
                local: class_from(hermes(record, true), content),
                otherwise: class_from(hermes(record, false), content),
            },
        });
    }
    let Some(record) = content
        .as_object()
        .filter(|record| source == "opencode" && !is_agit_marker(record))
        .filter(|record| recovered_evidence(record).is_none())
    else {
        return fixed(source);
    };
    let message = |key: &str| record.get(key).and_then(Value::as_str).map(id_hash);
    match record.get("kind").and_then(Value::as_str) {
        Some("message") => match record["data"]["role"].as_str() {
            Some(role) => RecordKind(Kind::OpencodeHeader {
                message: message("id"),
                host: Host::of(Some(role)),
                class: class_from(opencode_header(record), content),
            }),
            None => fixed(source),
        },
        Some("part") => RecordKind(Kind::OpencodePart {
            message: message("message_id"),
            under_assistant: class_from(opencode_part(record, Host::Assistant), content),
            under_user: class_from(opencode_part(record, Host::User), content),
            otherwise: class_from(opencode_part(record, Host::Other), content),
        }),
        _ => fixed(source),
    }
}

/// A 64-bit hash of an id: the first 8 bytes of its SHA-256.
fn id_hash(id: &str) -> u64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(id.as_bytes());
    u64::from_be_bytes(
        digest[..8]
            .try_into()
            .expect("a SHA-256 digest has 8 bytes"),
    )
}

/// The class of a record that publication keeps as `published`.
fn class_from(published: Option<Value>, content: &Value) -> RecordClass {
    match published {
        Some(kept) if kept == *content => RecordClass::Conversation,
        _ if is_request_snapshot(content) => RecordClass::RequestSnapshot,
        _ => RecordClass::RuntimeContext,
    }
}

impl Classifier {
    /// How `content`, the next native record of runtime `source`, is classified. Only a record
    /// that [`Classifier::published`] keeps unchanged is [`RecordClass::Conversation`]; a record
    /// that would lose injected blocks is not, so the Hub refuses what the CLI would have trimmed.
    pub fn class(&mut self, source: &str, content: &Value) -> RecordClass {
        self.class_of(record_kind(source, content))
    }

    /// [`Classifier::class`] of the next record, from its [`RecordKind`].
    pub fn class_of(&mut self, kind: RecordKind) -> RecordClass {
        match kind.0 {
            Kind::Fixed(class) => class,
            Kind::HermesSession { local, class } => {
                self.hermes_local = local;
                class
            }
            Kind::HermesRow { local, otherwise } => {
                if self.hermes_local {
                    local
                } else {
                    otherwise
                }
            }
            Kind::OpencodeHeader {
                message,
                host,
                class,
            } => {
                if let Some(message) = message {
                    self.message_roles.insert(message, host);
                }
                class
            }
            Kind::OpencodePart {
                message,
                under_assistant,
                under_user,
                otherwise,
            } => match message
                .and_then(|message| self.message_roles.get(&message))
                .copied()
                .unwrap_or(Host::Other)
            {
                Host::Assistant => under_assistant,
                Host::User => under_user,
                Host::Other => otherwise,
            },
        }
    }

    /// The next record as it is published, or `None` when it is left out.
    pub fn published(&mut self, source: &str, content: &Value) -> Option<Value> {
        let record = content.as_object()?;
        if is_agit_marker(record) {
            return Some(content.clone());
        }
        if let Some(quote) = recovered_evidence(record) {
            return quote.then(|| content.clone());
        }
        match source {
            "claude-code" | "claude-desktop" => claude(content, record),
            "cursor" => cursor(record),
            "codex" => codex(record),
            "opencode" => self.opencode(record),
            "hermes" => {
                if let Some(local) = hermes_local_session(record) {
                    self.hermes_local = local;
                    return None;
                }
                hermes(record, self.hermes_local)
            }
            "openclaw" => openclaw(record),
            "workbuddy" => workbuddy(record),
            _ => None,
        }
    }
}

/// [`Classifier::class`] for a record judged on its own, outside its session's order.
pub fn record_class(source: &str, content: &Value) -> RecordClass {
    Classifier::default().class(source, content)
}

/// [`Classifier::published`] for a record judged on its own, outside its session's order.
pub fn published(source: &str, content: &Value) -> Option<Value> {
    Classifier::default().published(source, content)
}

/// A session's records in order, each as it is published or `None`. Claude-shaped records chain
/// by `parentUuid`; a kept record whose parent is left out is linked to its nearest kept ancestor,
/// so the published conversation still reads as one chain.
pub fn published_sequence<'a>(
    records: impl IntoIterator<Item = (&'a str, &'a Value)>,
) -> Vec<Option<Value>> {
    let mut classifier = Classifier::default();
    let mut dropped: HashMap<String, Value> = HashMap::new();
    let mut out = Vec::new();
    for (source, content) in records {
        let kept = classifier.published(source, content);
        let ancestor = |field: &str, dropped: &HashMap<String, Value>| {
            content
                .get(field)
                .and_then(Value::as_str)
                .and_then(|parent| dropped.get(parent))
                .cloned()
        };
        match kept {
            None => {
                if let Some(uuid) = content.get("uuid").and_then(Value::as_str) {
                    let parent = ancestor("parentUuid", &dropped)
                        .or_else(|| content.get("parentUuid").cloned())
                        .unwrap_or(Value::Null);
                    dropped.insert(uuid.to_owned(), parent);
                }
                out.push(None);
            }
            Some(mut kept) => {
                for field in ["parentUuid", "logicalParentUuid"] {
                    if let Some(parent) = ancestor(field, &dropped) {
                        kept[field] = parent;
                    }
                }
                out.push(Some(kept));
            }
        }
    }
    out
}

/// For a quote of an earlier record that resume made from an encrypted session's original, whether
/// it may be published: only when the record it quotes is a conversation record published as it
/// is, judged under the runtime the quote names. A quote whose record cannot be read is not.
/// `None` when `record` is no such quote.
fn recovered_evidence(record: &Map<String, Value>) -> Option<bool> {
    use crate::domain::transcript::recovery::{EVIDENCE_FIELD, EVIDENCE_PREFIX, GENERATED_ORIGIN};
    let content = record.get("message").map(|message| &message["content"]);
    let text = content.and_then(|content| {
        content.as_str().or_else(|| {
            content
                .as_array()
                .filter(|blocks| blocks.len() == 1)
                .and_then(|blocks| blocks[0]["text"].as_str())
        })
    });
    let quoted = text.and_then(|text| text.strip_prefix(EVIDENCE_PREFIX));
    if record.get("agit").and_then(Value::as_str) != Some(GENERATED_ORIGIN)
        && !record.contains_key(EVIDENCE_FIELD)
        && quoted.is_none()
    {
        return None;
    }
    let quote: Option<Value> = quoted.and_then(|quoted| serde_json::from_str(quoted).ok());
    Some(quote.is_some_and(|quote| {
        quote["runtime"].as_str().is_some_and(|runtime| {
            quote.get("record").is_some_and(|original| {
                record_class(runtime, original) == RecordClass::Conversation
            })
        })
    }))
}

fn is_request_snapshot(content: &Value) -> bool {
    content["type"]
        .as_str()
        .is_some_and(|kind| kind.starts_with("api-request") || kind == "prompt_snapshot")
}

/// agit's own merge, cherry-pick and revert markers, with or without the source they name, and its
/// merge summary. They carry no runtime context; a merge summary's text goes through the detector
/// like any other.
fn is_agit_marker(record: &Map<String, Value>) -> bool {
    let kind = record.get("type").and_then(Value::as_str);
    if kind == Some("system") {
        // Older markers may name no source; they carry no other field either.
        let sourced = record.len() == 3 && record.get("source").is_some_and(Value::is_string);
        return (sourced || record.len() == 2)
            && matches!(
                record.get("subtype").and_then(Value::as_str),
                Some(
                    "agit:__merge_start__"
                        | "agit:__merge_end__"
                        | "agit:__cherry_pick_start__"
                        | "agit:__cherry_pick_end__"
                        | "agit:__revert__"
                )
            );
    }
    kind == Some("user")
        && record.len() == 3
        && record.get("agit").and_then(Value::as_str) == Some("merge_summary")
        && record.get("message").is_some_and(|message| {
            message
                .as_object()
                .is_some_and(|message| message.len() == 2)
                && message["role"] == "user"
                && message["content"].is_string()
        })
}

// ---- Claude Code (and Claude Desktop's Code tab, which writes the same records) -----------

/// Claude Code: `user` and `assistant` records only. A user record the runtime generated
/// (`isMeta`, a system prompt source) is left out; a kept one loses its injected blocks.
fn claude(content: &Value, record: &Map<String, Value>) -> Option<Value> {
    let runtime_generated = record.get("isMeta") == Some(&Value::Bool(true))
        || crate::adapter::claude_code::has_system_prompt_source(content);
    match record.get("type").and_then(Value::as_str)? {
        "assistant" if !runtime_generated => Some(content.clone()),
        "user" if !runtime_generated => {
            let content = record.get("message")?.get("content")?;
            let kept = typed_content(content)?;
            let mut record = record.clone();
            record["message"]["content"] = kept;
            Some(Value::Object(record))
        }
        "queue-operation" => absorbed_follow_up(record),
        _ => None,
    }
}

/// Words the person typed while a tool ran, which Claude Code absorbs into the turn: no user
/// record repeats them. Only that removal of a queued prompt is kept, with its text through
/// [`typed_text`], since the runtime's own notices pass through the same queue; enqueued copies,
/// other removals, other modes and records with fields nobody knows are left out.
fn absorbed_follow_up(record: &Map<String, Value>) -> Option<Value> {
    const FIELDS: &[&str] = &[
        "type",
        "operation",
        "reason",
        "content",
        "mode",
        "timestamp",
        "sessionId",
        "uuid",
        "parentUuid",
    ];
    // A queued shell command or notification says so in its mode; a field nobody knows could
    // carry anything.
    if record.get("operation").and_then(Value::as_str) != Some("remove")
        || record.get("reason").and_then(Value::as_str) != Some("absorbed_mid_turn")
        || record
            .get("mode")
            .is_some_and(|mode| mode.as_str() != Some("prompt"))
        || record.keys().any(|key| !FIELDS.contains(&key.as_str()))
    {
        return None;
    }
    let words = typed_text(record.get("content")?.as_str()?)?;
    let mut kept = Map::new();
    for key in [
        "type",
        "operation",
        "reason",
        "mode",
        "timestamp",
        "sessionId",
        "uuid",
        "parentUuid",
    ] {
        if let Some(value) = record.get(key) {
            kept.insert(key.into(), value.clone());
        }
    }
    kept.insert("content".into(), Value::String(words));
    Some(Value::Object(kept))
}

/// The parts of a Claude-shaped user `content` the person typed or pasted, and tool results.
fn typed_content(content: &Value) -> Option<Value> {
    if let Some(text) = content.as_str() {
        return typed_text(text).map(Value::String);
    }
    let mut kept = Vec::new();
    for block in content.as_array()? {
        match block["type"].as_str() {
            Some("tool_result") => kept.push(tool_result(block)),
            Some("image") => kept.push(block.clone()),
            Some("text") => {
                if let Some(text) = block["text"].as_str().and_then(typed_text) {
                    let mut block = block.clone();
                    block["text"] = Value::String(text);
                    kept.push(block);
                }
            }
            _ => {}
        }
    }
    (!kept.is_empty()).then_some(Value::Array(kept))
}

/// A tool result without the elements the runtime appended to it. The output itself stays,
/// empty when nothing else was there, so the call keeps its result. Of a result made of parts,
/// text parts (`text`, and Codex's `input_text` and `output_text`) and images stay; any other
/// part, such as one nesting further parts or an `input_image` that can name a local file, is
/// left out.
fn tool_result(block: &Value) -> Value {
    let mut block = block.clone();
    if let Some(content) = block.get_mut("content") {
        *content = tool_output(content, true);
    }
    block
}

/// Tool output follows one rule for Claude and Codex: its text, and the text parts of an array,
/// without injected elements; only Claude-shaped results retain images. Any other output, a
/// structured result or a scalar, is kept as it is; protection masks its credential fields and
/// keeps its references.
fn tool_output(output: &Value, keep_images: bool) -> Value {
    match output {
        Value::String(text) => Value::String(without_injected(text)),
        Value::Array(parts) => {
            let parts = parts
                .iter()
                .filter_map(|part| match part["text"].as_str() {
                    Some(text)
                        if matches!(
                            part["type"].as_str(),
                            Some("text" | "input_text" | "output_text")
                        ) =>
                    {
                        let text = without_injected(text);
                        (!text.trim().is_empty()).then(|| {
                            let mut part = part.clone();
                            part["text"] = Value::String(text);
                            part
                        })
                    }
                    _ if keep_images && part["type"] == "image" => Some(part.clone()),
                    _ => None,
                })
                .collect();
            Value::Array(parts)
        }
        other => other.clone(),
    }
}

/// Tags a runtime wraps around context it supplies on its own. A block runs to its closing tag,
/// or to the end of the text when it has none.
const INJECTED_ELEMENTS: &[&str] = &[
    "system-reminder",
    "in-app-browser-context",
    "user_instructions",
    "environment_context",
    "user-prompt-submit-hook",
    "session-start-hook",
];

/// Openings of a text the runtime wrote in the user's role: a command's output, a hook's, a
/// notification, an interruption notice.
const INJECTED_OPENINGS: &[&str] = &[
    "<task-notification>",
    "<command-name>",
    "<command-message>",
    "<command-args>",
    "<local-command-stdout>",
    "<local-command-stderr>",
    "<local-command-caveat>",
    "<user-prompt-submit-hook>",
    "<session-start-hook>",
    "<bash-input>",
    "<bash-stdout>",
    "<bash-stderr>",
    "<bash-notification>",
    "<teammate-message",
    "[Your previous response had no visible output",
    "[Request interrupted by user",
    "A session-scoped Stop hook is now active",
    "Caveat: The messages below were generated by the user while ",
    "Your tool call was malformed and could not be parsed",
    "Unknown slash command:",
];

/// `text` without the elements a runtime injected into it, or `None` when nothing the person
/// wrote remains. The same rule decides what the Hub shows as typed text.
pub fn typed_text(text: &str) -> Option<String> {
    let kept = without_injected(text);
    let trimmed = kept.trim();
    if trimmed.is_empty()
        || INJECTED_OPENINGS
            .iter()
            .any(|open| trimmed.starts_with(open))
    {
        return None;
    }
    Some(if kept.len() == text.len() {
        text.to_owned()
    } else {
        trimmed.to_owned()
    })
}

/// `text` without the injected elements in it, unchanged when none opens in it.
///
/// Each element runs from its opening tag to its first closing tag. The text is unambiguous when
/// no element's body holds a tag of any runtime element and what is left holds none either: then
/// everything outside the elements is kept, whether the runtime put them before the words, after
/// them or on both sides. Otherwise (a body quotes a tag, a stray tag is left, or an element is
/// never closed) only the text before the first element is kept, which is nothing when the text
/// opens with one.
///
/// Accepted residual: injected text that quotes a closing tag and then an opening tag of its
/// element (`<r>X </r> Y <r> Z </r>`, with `Y` injected) reads exactly as a reminder on each side
/// of the person's words, so `Y` is kept. No observed runtime shape quotes those tags.
fn without_injected(text: &str) -> String {
    let first = |text: &str| {
        INJECTED_ELEMENTS
            .iter()
            .filter_map(|tag| opening(text, tag).map(|at| (at, *tag)))
            .min_by_key(|(at, _)| *at)
    };
    let Some((first_at, _)) = first(text) else {
        return text.to_owned();
    };
    let mut rest = text;
    let mut kept = String::new();
    let mut ambiguous = false;
    while let Some((at, tag)) = first(rest) {
        kept.push_str(&rest[..at]);
        let element = &rest[at..];
        let close = format!("</{tag}>");
        let body = element
            .find('>')
            .and_then(|open_end| Some((open_end + 1, element.find(&close)?)))
            .filter(|(body_start, close_at)| body_start <= close_at);
        let Some((body_start, close_at)) = body else {
            ambiguous = true;
            break;
        };
        if holds_element_tag(&element[body_start..close_at]) {
            ambiguous = true;
            break;
        }
        rest = &element[close_at + close.len()..];
    }
    if !ambiguous {
        kept.push_str(rest);
        ambiguous = holds_element_tag(&kept);
    }
    if ambiguous {
        return text[..first_at].trim().to_owned();
    }
    kept.trim().to_owned()
}

/// Whether `text` holds an opening or closing tag of any runtime element.
fn holds_element_tag(text: &str) -> bool {
    INJECTED_ELEMENTS
        .iter()
        .any(|tag| opening(text, tag).is_some() || text.contains(&format!("</{tag}>")))
}

/// Where the first `<tag` element of `text` opens: one followed by `>`, whitespace or the end, so
/// a longer name that only begins the same way does not count.
fn opening(text: &str, tag: &str) -> Option<usize> {
    let open = format!("<{tag}");
    text.match_indices(&open).map(|(at, _)| at).find(|at| {
        text[at + open.len()..]
            .chars()
            .next()
            .is_none_or(|next| next == '>' || next.is_whitespace())
    })
}

// ---- Cursor and WorkBuddy -----------------------------------------------------------------

/// A Cursor or WorkBuddy user `content` with its tool results and the request inside its one
/// `<user_query>` block, or `None`. Text in any other shape (no block, two, or nested or
/// unbalanced tags) withholds the whole record. These runtimes put the context they supply
/// (`<user_info>`, rules, timestamps) around that block in the same message, so everything
/// outside it is left out. `text_blocks` are the block types the runtime's adapter reads as text:
/// a block of a type left off it is neither counted nor kept, so a request in it is never shown.
fn user_query_content(content: &Value, text_blocks: &[&str]) -> Option<Value> {
    const OPEN: &str = "<user_query>";
    const CLOSE: &str = "</user_query>";
    let texts: Vec<&str> = match content {
        Value::String(text) => vec![text.as_str()],
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| {
                block["type"]
                    .as_str()
                    .is_some_and(|kind| text_blocks.contains(&kind))
            })
            .filter_map(|block| block["text"].as_str())
            .filter(|text| !text.trim().is_empty())
            .collect(),
        _ => return None,
    };
    let request = if texts.is_empty() {
        None
    } else {
        let count = |tag: &str| {
            texts
                .iter()
                .map(|text| text.matches(tag).count())
                .sum::<usize>()
        };
        if count("<user_query") != 1 || count("</user_query") != 1 {
            return None;
        }
        let text = texts.iter().find(|text| text.contains(OPEN))?;
        let start = text.find(OPEN)? + OPEN.len();
        let end = text.find(CLOSE).filter(|end| *end >= start)?;
        let request = typed_text(&text[start..end])?;
        // The runtime's own wake-up notices use the same wrapper.
        if crate::adapter::cursor::is_synthetic_user_text(&request) {
            return None;
        }
        Some(format!("{OPEN}\n{}\n{CLOSE}", request.trim()))
    };
    let Value::Array(blocks) = content else {
        return request.map(Value::String);
    };
    let mut kept = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("tool_result") => kept.push(tool_result(block)),
            Some(kind)
                if text_blocks.contains(&kind)
                    && block["text"]
                        .as_str()
                        .is_some_and(|text| text.contains(OPEN)) =>
            {
                let mut block = block.clone();
                block["text"] = Value::String(request.clone()?);
                kept.push(block);
            }
            _ => {}
        }
    }
    (!kept.is_empty()).then_some(Value::Array(kept))
}

/// Cursor: assistant records, and user records with their request only.
fn cursor(record: &Map<String, Value>) -> Option<Value> {
    let role = match record.get("type").and_then(Value::as_str) {
        None => record.get("role").and_then(Value::as_str),
        Some("assistant") => Some("assistant"),
        Some(_) => None,
    };
    if record.get("isMeta") == Some(&Value::Bool(true)) {
        return None;
    }
    match role? {
        "assistant" => Some(Value::Object(record.clone())),
        "user" => {
            let kept = user_query_content(
                record.get("message")?.get("content")?,
                crate::adapter::cursor::TEXT_BLOCKS,
            )?;
            let mut record = Value::Object(record.clone());
            record["message"]["content"] = kept;
            Some(record)
        }
        _ => None,
    }
}

// ---- Codex ----------------------------------------------------------------------------------

/// Codex: assistant messages, reasoning, tool calls and their output, and the conversation's own
/// events. Developer and system messages, `session_meta` (base instructions), `turn_context`,
/// `world_state` and usage records are left out. A user-role message is kept only when its
/// provenance marks it as the person's input: Codex files AGENTS.md and the environment context
/// in the user's role too.
fn codex(record: &Map<String, Value>) -> Option<Value> {
    let payload = record.get("payload")?.as_object()?;
    let kind = payload.get("type").and_then(Value::as_str);
    let kept = match record.get("type").and_then(Value::as_str)? {
        "response_item" => match kind? {
            "message" => match payload.get("role").and_then(Value::as_str)? {
                "assistant" => true,
                "user" => codex_typed_by_person(payload),
                _ => false,
            },
            "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
                let mut record = Value::Object(record.clone());
                record["payload"]["output"] = tool_output(payload.get("output")?, false);
                return Some(record);
            }
            "reasoning" | "function_call" | "custom_tool_call" | "local_shell_call" => true,
            _ => false,
        },
        "event_msg" if kind == Some("user_message") => {
            // The person's submission, unless an older Codex filed its instructions or the
            // environment under this event with another kind.
            if payload
                .get("kind")
                .is_some_and(|kind| kind.as_str() != Some("plain"))
            {
                return None;
            }
            let message = typed_text(payload.get("message")?.as_str()?)?;
            let mut record = Value::Object(record.clone());
            record["payload"]["message"] = Value::String(message);
            return Some(record);
        }
        "event_msg" if kind == Some("token_count") => return codex_usage(record, payload),
        "turn_context" => return codex_model(record, payload),
        "event_msg" => matches!(
            kind?,
            "agent_message"
                | "agent_reasoning"
                | "task_complete"
                | "turn_aborted"
                | "patch_apply_end"
        ),
        "compacted" => {
            // The summary only: `replacement_history` replays the injected context.
            let message = payload
                .get("message")
                .filter(|message| message.is_string())?;
            let mut summary = record.clone();
            summary.insert("payload".into(), serde_json::json!({ "message": message }));
            return Some(Value::Object(summary));
        }
        _ => false,
    };
    kept.then(|| Value::Object(record.clone()))
}

/// A turn's token counts as numbers only. The account's rate limits, and anything else the event
/// carries, are left out; an event with no counts publishes nothing.
fn codex_usage(record: &Map<String, Value>, payload: &Map<String, Value>) -> Option<Value> {
    const COUNTS: &[&str] = &[
        "input_tokens",
        "cached_input_tokens",
        "output_tokens",
        "reasoning_output_tokens",
        "total_tokens",
    ];
    let info = payload.get("info")?.as_object()?;
    let numbers = |usage: &Value, keys: &[&str]| -> Map<String, Value> {
        keys.iter()
            .filter_map(|key| {
                let value = usage.get(*key).filter(|value| value.is_u64())?;
                Some(((*key).to_owned(), value.clone()))
            })
            .collect()
    };
    let mut counted = Map::new();
    for key in ["total_token_usage", "last_token_usage"] {
        let usage = info.get(key).map(|usage| numbers(usage, COUNTS));
        if let Some(usage) = usage.filter(|usage| !usage.is_empty()) {
            counted.insert(key.into(), Value::Object(usage));
        }
    }
    counted.extend(numbers(
        &Value::Object(info.clone()),
        &["model_context_window"],
    ));
    if counted.is_empty() {
        return None;
    }
    Some(codex_stub(
        record,
        "event_msg",
        serde_json::json!({"type": "token_count", "info": counted}),
    ))
}

/// The model a turn ran on, and nothing else from its context: no working directory,
/// instructions, approval or sandbox policy. A value that is not a short model id publishes
/// nothing.
fn codex_model(record: &Map<String, Value>, payload: &Map<String, Value>) -> Option<Value> {
    let model = payload.get("model")?.as_str().filter(|model| {
        // A model the person serves from disk would name its path, a home directory included.
        !model.is_empty()
            && model.len() <= 100
            && !model.starts_with(['/', '~', '.'])
            && !model.contains("..")
            && !model.contains('\\')
            // A drive letter's path (C:/...) or a URL.
            && !model.contains(":/")
            && model
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._:/-".contains(&byte))
    })?;
    Some(codex_stub(
        record,
        "turn_context",
        serde_json::json!({ "model": model }),
    ))
}

/// A Codex record of `kind` holding only `payload`, and the record's own time for ordering.
fn codex_stub(record: &Map<String, Value>, kind: &str, payload: Value) -> Value {
    let mut stub = Map::new();
    if let Some(timestamp) = record.get("timestamp").filter(|time| time.is_string()) {
        stub.insert("timestamp".into(), timestamp.clone());
    }
    stub.insert("type".into(), Value::String(kind.into()));
    stub.insert("payload".into(), payload);
    Value::Object(stub)
}

/// Whether every content item of a Codex user-role message is marked as the person's own input.
fn codex_typed_by_person(payload: &Map<String, Value>) -> bool {
    let Some(kinds) = payload
        .get("internal_chat_message_metadata_passthrough")
        .and_then(|metadata| metadata.get("content_item_kinds"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    let items = payload
        .get("content")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    !kinds.is_empty()
        && kinds.len() == items
        && kinds
            .iter()
            .all(|kind| kind.as_str().is_some_and(|kind| kind.starts_with("user.")))
}

// ---- OpenCode -------------------------------------------------------------------------------

impl Classifier {
    /// OpenCode: message headers (role and mode only), the assistant's text, reasoning, tool and
    /// compaction parts, and the text parts of the person's message that OpenCode leaves
    /// unmarked. OpenCode 1.19.4 marks each text part it adds to the person's turn `synthetic`.
    /// Accepted residual: a text part a plugin adds without a mark or any other field reads as
    /// typed and is published.
    fn opencode(&mut self, record: &Map<String, Value>) -> Option<Value> {
        match record.get("kind").and_then(Value::as_str)? {
            "message" => {
                let role = record.get("data")?.get("role")?.as_str()?;
                if let Some(id) = record.get("id").and_then(Value::as_str) {
                    self.opencode_roles.insert(id.to_owned(), role.to_owned());
                }
                opencode_header(record)
            }
            "part" => {
                let host = Host::of(
                    record
                        .get("message_id")
                        .and_then(Value::as_str)
                        .and_then(|id| self.opencode_roles.get(id))
                        .map(String::as_str),
                );
                opencode_part(record, host)
            }
            // The other storage shape: one row per message, its parts inside. Assistant rows
            // keep their timing and their text, reasoning and tool items; a compaction keeps its
            // summary. A user row holds what the person sent, and OpenCode writes what it adds as
            // rows of other types: it keeps its timing and its text through `typed_text`, never
            // its attached files, and a row with any other field is left out.
            "session_message" => {
                let data = record.get("data")?.as_object()?;
                let mut kept_data = Map::new();
                match record.get("type").and_then(Value::as_str)? {
                    "assistant" => {
                        if let Some(time) = data.get("time") {
                            kept_data.insert("time".into(), time.clone());
                        }
                        let items: Vec<Value> = data
                            .get("content")?
                            .as_array()?
                            .iter()
                            .filter(|item| {
                                matches!(item["type"].as_str(), Some("text" | "reasoning" | "tool"))
                            })
                            .cloned()
                            .collect();
                        kept_data.insert("content".into(), Value::Array(items));
                    }
                    "compaction" => {
                        kept_data.insert("summary".into(), data.get("summary")?.clone());
                    }
                    "user" => {
                        const FIELDS: &[&str] = &["time", "text", "files"];
                        if data.keys().any(|key| !FIELDS.contains(&key.as_str())) {
                            return None;
                        }
                        if let Some(time) = data.get("time") {
                            kept_data.insert("time".into(), time.clone());
                        }
                        let text = typed_text(data.get("text")?.as_str()?)?;
                        kept_data.insert("text".into(), Value::String(text));
                    }
                    _ => return None,
                }
                let mut kept = Map::new();
                for key in ["id", "kind", "session_id", "type", "seq", "time_created"] {
                    if let Some(value) = record.get(key) {
                        kept.insert(key.into(), value.clone());
                    }
                }
                kept.insert("data".into(), Value::Object(kept_data));
                Some(Value::Object(kept))
            }
            _ => None,
        }
    }
}

/// An OpenCode message header as it is published: its role and mode only.
fn opencode_header(record: &Map<String, Value>) -> Option<Value> {
    let data = record.get("data")?.as_object()?;
    data.get("role")?.as_str()?;
    let mut header = Map::new();
    for key in ["role", "mode"] {
        if let Some(value) = data.get(key) {
            header.insert(key.into(), value.clone());
        }
    }
    let mut kept = Map::new();
    for key in ["id", "kind", "session_id", "time_created"] {
        if let Some(value) = record.get(key) {
            kept.insert(key.into(), value.clone());
        }
    }
    kept.insert("data".into(), Value::Object(header));
    Some(Value::Object(kept))
}

/// An OpenCode part as it is published under a message of role `host`. The assistant's text and
/// every message's reasoning, tool and compaction parts are kept. Of the person's message only a
/// text part OpenCode leaves unmarked is kept, as [`opencode_typed_part`] reduces it: OpenCode
/// writes what it adds to the person's turn as text parts marked `synthetic`, and its attached
/// files are never published.
fn opencode_part(record: &Map<String, Value>, host: Host) -> Option<Value> {
    let data = record.get("data")?;
    match (host, data["type"].as_str()?) {
        (Host::Assistant, "text") => {
            (data["synthetic"] != true).then(|| Value::Object(record.clone()))
        }
        (_, "reasoning" | "tool" | "compaction") => Some(Value::Object(record.clone())),
        (Host::User, "text") => opencode_typed_part(record),
        _ => None,
    }
}

/// A text part of the person's message with its text through [`typed_text`], or `None` when the
/// part carries any field beyond its text, type, time and ids (a `synthetic` or `ignored` mark of
/// either value among them) or no typed words remain.
fn opencode_typed_part(record: &Map<String, Value>) -> Option<Value> {
    const DATA: &[&str] = &["type", "text", "time", "id", "messageID", "sessionID"];
    let data = record.get("data")?.as_object()?;
    if data.keys().any(|key| !DATA.contains(&key.as_str())) {
        return None;
    }
    let text = typed_text(data.get("text")?.as_str()?)?;
    let mut data = data.clone();
    data.insert("text".into(), Value::String(text));
    let mut kept = Map::new();
    for key in ["id", "kind", "session_id", "message_id", "time_created"] {
        if let Some(value) = record.get(key) {
            kept.insert(key.into(), value.clone());
        }
    }
    kept.insert("data".into(), Value::Object(data));
    Some(Value::Object(kept))
}

// ---- Hermes, OpenClaw, WorkBuddy ------------------------------------------------------------

/// Hermes: active assistant and tool messages, and the person's own messages where the session
/// is the owner's own console (`local`, as the session's header records it).
fn hermes(record: &Map<String, Value>, local: bool) -> Option<Value> {
    let data = record.get("data")?;
    if record.get("type").and_then(Value::as_str) != Some("hermes_message") || data["active"] == 0 {
        return None;
    }
    match data["role"].as_str()? {
        "assistant" | "tool" => Some(Value::Object(record.clone())),
        "user" => hermes_typed(data, local),
        _ => None,
    }
}

/// Whether a Hermes session header says the session is the owner's own console. Hermes records
/// where each session came from (`local` for its console; a platform's name for a gateway such as
/// Telegram or Discord, whose user rows can be other people's).
fn hermes_local_session(record: &Map<String, Value>) -> Option<bool> {
    (record.get("type").and_then(Value::as_str) == Some("hermes_session")).then(|| {
        record
            .get("data")
            .is_some_and(|data| data["source"] == "local")
    })
}

/// A Hermes user row holds what the person typed when the gateway took it in as their message: it
/// carries the platform's message id and nothing of what Hermes adds itself, except one thing.
/// When the person mentions a file (`@file:`), Hermes appends the file's text to the same row
/// after [`HERMES_ATTACHED`], so only the text before it is theirs. That is cut off only when
/// everything after the marker is file blocks of the one shape Hermes v0.22.5 writes; any other
/// attachment, or text the cut can't account for, withholds the whole row. Hermes writes its
/// compaction summary as a user row with `_compressed_summary` set and no platform id. A column
/// this rule doesn't know, or one it knows Hermes sets for its own additions, withholds the row
/// too. Only a session from the owner's own console publishes these rows; on any other platform
/// they can be other people's messages. What publishes is the row's time, order and typed text,
/// marked `session_source: local` in place of the platform's id.
const HERMES_ATTACHED: &str = "\n\n--- Attached Context ---\n\n";

/// The fields of a Hermes user row as it publishes.
const HERMES_PUBLISHED: &[&str] = &[
    "session_id",
    "role",
    "timestamp",
    "active",
    "display_order",
    "message_uid",
    "_compressed_summary",
    "session_source",
    "content",
];

fn hermes_typed(data: &Value, local: bool) -> Option<Value> {
    // Published once already: its own mark says where it came from.
    let republished = data["session_source"] == "local"
        && data
            .as_object()?
            .keys()
            .all(|key| HERMES_PUBLISHED.contains(&key.as_str()));
    // The columns of Hermes v0.22.5's messages table; any other one set withholds the row.
    const KNOWN: &[&str] = &[
        "id",
        "session_id",
        "role",
        "content",
        "effect_disposition",
        "timestamp",
        "token_count",
        "finish_reason",
        "platform_message_id",
        "observed",
        "_compressed_summary",
        "active",
        "compacted",
        "display_metadata",
        "display_identity",
        "display_order",
        "message_uid",
    ];
    const UNSET: &[&str] = &[
        "tool_call_id",
        "tool_calls",
        "tool_name",
        "reasoning",
        "reasoning_content",
        "reasoning_details",
        "codex_reasoning_items",
        "codex_message_items",
        "api_content",
        "display_kind",
        "absorbed_message_uids",
        "tool_call_uids",
        "tool_call_uid",
    ];
    let taken_in = local
        && data["platform_message_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty());
    let typed = data["_compressed_summary"] == 0
        && (taken_in || republished)
        && UNSET
            .iter()
            .all(|key| data.get(*key).is_none_or(Value::is_null))
        && data.as_object()?.iter().all(|(key, value)| {
            KNOWN.contains(&key.as_str())
                || UNSET.contains(&key.as_str())
                || (republished && key == "session_source")
                || value.is_null()
        });
    if !typed {
        return None;
    }
    let content = data["content"].as_str()?;
    let typed = match content.split_once(HERMES_ATTACHED) {
        None => content,
        Some((typed, attached)) => {
            hermes_file_blocks(attached)?;
            typed
        }
    };
    let text = typed_text(typed)?;
    let mut kept = Map::new();
    for key in HERMES_PUBLISHED {
        if let Some(value) = data.get(*key) {
            kept.insert((*key).into(), value.clone());
        }
    }
    // Where it came from stays with it, so publishing it again keeps it.
    kept.insert("session_source".into(), Value::String("local".into()));
    kept.insert("content".into(), Value::String(text));
    Some(serde_json::json!({"type":"hermes_message","data":kept}))
}

/// `Some` when `attached` is nothing but file blocks as Hermes v0.22.5 writes them after
/// [`HERMES_ATTACHED`]: a `📄 @file:<name> (<n> tokens)` line, then the file's text in one fence.
fn hermes_file_blocks(attached: &str) -> Option<()> {
    let mut rest = attached;
    loop {
        rest = rest.trim_start_matches('\n');
        if rest.is_empty() {
            return Some(());
        }
        let block = rest.strip_prefix("📄 @file:")?;
        let (header, body) = block.split_once('\n')?;
        if !header.ends_with(" tokens)") || header.contains("```") {
            return None;
        }
        let body = body.strip_prefix("```")?;
        let (_language, text) = body.split_once('\n')?;
        let end = text.find("\n```")?;
        rest = &text[end + 4..];
        if !(rest.is_empty() || rest.starts_with('\n')) {
            return None;
        }
    }
}

/// OpenClaw: assistant messages, tool results, and the person's own messages.
fn openclaw(record: &Map<String, Value>) -> Option<Value> {
    if record.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    match record.get("message")?["role"].as_str()? {
        "assistant" | "toolResult" => Some(Value::Object(record.clone())),
        "user" => openclaw_typed(record),
        _ => None,
    }
}

/// An OpenClaw user message holds what the person typed when it is the owner's own message as
/// OpenClaw took it in: a string, marked as the owner's, keyed as a user message, and nothing
/// else. OpenClaw keeps the context it supplies in events of their own and its compaction summary
/// in a `compaction` event. Any other field or shape withholds it; what publishes is the message
/// with its text through `typed_text`.
fn openclaw_typed(record: &Map<String, Value>) -> Option<Value> {
    const FIELDS: &[&str] = &[
        "role",
        "content",
        "timestamp",
        "__openclaw",
        "idempotencyKey",
    ];
    let message = record.get("message")?.as_object()?;
    let typed = message.keys().all(|key| FIELDS.contains(&key.as_str()))
        && message.get("__openclaw") == Some(&serde_json::json!({"senderIsOwner": true}))
        && message
            .get("idempotencyKey")
            .and_then(Value::as_str)
            .is_some_and(|key| key.ends_with(":user"));
    if !typed {
        return None;
    }
    let text = typed_text(message.get("content")?.as_str()?)?;
    let mut kept = Map::new();
    for key in ["type", "id", "parentId", "timestamp"] {
        if let Some(value) = record.get(key) {
            kept.insert(key.into(), value.clone());
        }
    }
    let mut published = message.clone();
    published.insert("content".into(), Value::String(text));
    kept.insert("message".into(), Value::Object(published));
    Some(Value::Object(kept))
}

/// WorkBuddy: assistant messages, reasoning, tool calls and their results, and user messages
/// with their request only.
fn workbuddy(record: &Map<String, Value>) -> Option<Value> {
    let kept = match record.get("type").and_then(Value::as_str)? {
        "message" => match record.get("role").and_then(Value::as_str)? {
            "assistant" => true,
            "user" => {
                let kept = user_query_content(
                    record.get("content")?,
                    crate::adapter::workbuddy::TEXT_BLOCKS,
                )?;
                let mut record = Value::Object(record.clone());
                record["content"] = kept;
                return Some(record);
            }
            _ => false,
        },
        "reasoning" | "function_call" | "function_call_result" => true,
        _ => false,
    };
    kept.then(|| Value::Object(record.clone()))
}

#[cfg(test)]
#[path = "publication_records_tests.rs"]
mod tests;
