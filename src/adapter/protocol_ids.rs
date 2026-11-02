//! Identifier roles belong to native record schemas, never to arbitrary nested tool data.

use serde_json::Value;

/// Native structural names stay addressable; unknown members and tool payload keys are data.
#[cfg(feature = "secret-vault")]
pub(crate) const NATIVE_MEMBER_NAMES: &[&str] = &[
    "type",
    "kind",
    "id",
    "_session_id",
    "_source",
    "_object_hash",
    "session_id",
    "sessionId",
    "event_id",
    "uuid",
    "parentUuid",
    "parentId",
    "parent_id",
    "parentID",
    "object_hash",
    "commit_id",
    "tree_id",
    "blob_id",
    "ref_name",
    "schema_version",
    "layout_version",
    "projection_version",
    "created_at",
    "updated_at",
    "timestamp",
    "provenance",
    "message",
    "payload",
    "data",
    "role",
    "content",
    "name",
    "input",
    "arguments",
    "output",
    "result",
    "tool_calls",
    "function",
    "tool_call_id",
    "call_id",
    "callId",
    "tool_use_id",
    "toolCallId",
    "callID",
    "signature",
    "encrypted_content",
    "text",
    "thinking",
    "summary",
    "is_error",
    "isError",
    "cwd",
    "model",
    "usage",
    "stop_reason",
    "stop_sequence",
    "isSidechain",
    "requestId",
    "state",
    "item",
    "replacement_history",
    "turn_id",
    "item_id",
    "client_id",
    "history_base",
    "thread_id",
    "forked_from_id",
    "parent_session_id",
    "parent_thread_id",
    "thread_source",
    "source",
    "subagent",
    "thread_spawn",
    "agent_path",
    "firstKeptEntryId",
    "fromId",
    "targetId",
    "appendParentId",
    "project_id",
    "message_id",
    "messageID",
    "tail_start_id",
    "sessionID",
];

/// Only a schema-owned result container identifies content blocks. A tool's ordinary maps
/// remain data even when their fields resemble a native record or an attachment.
#[cfg(feature = "secret-vault")]
#[derive(Clone, Copy)]
pub(crate) enum KeyContext {
    Native,
    Data,
    Content,
    Source,
}

#[cfg(feature = "secret-vault")]
impl KeyContext {
    pub(crate) fn schema_fields(
        self,
        object: &serde_json::Map<String, Value>,
    ) -> &'static [&'static str] {
        match self {
            Self::Native => NATIVE_MEMBER_NAMES,
            Self::Data => &[],
            Self::Source => &[
                "type",
                "encoding",
                "media_type",
                "mimeType",
                "mime_type",
                "data",
                "text",
                "url",
                "uri",
                "blob",
                "format",
            ],
            Self::Content => match object.get("type").and_then(Value::as_str) {
                Some("tool_use" | "server_tool_use" | "toolCall") => {
                    &["type", "id", "name", "input", "arguments"]
                }
                Some("tool_result" | "toolResult") => &[
                    "type",
                    "tool_use_id",
                    "toolCallId",
                    "content",
                    "is_error",
                    "isError",
                    "structuredContent",
                ],
                Some("thinking" | "reasoning" | "redacted_thinking" | "encrypted_content") => &[
                    "type",
                    "thinking",
                    "text",
                    "signature",
                    "data",
                    "encrypted_content",
                ],
                Some("text" | "input_text" | "output_text")
                    if object.get("text").is_some_and(Value::is_string) =>
                {
                    &["type", "text", "annotations", "citations"]
                }
                Some(
                    "image" | "input_image" | "output_image" | "document" | "file" | "input_file"
                    | "audio" | "input_audio" | "output_audio" | "video" | "resource"
                    | "resource_link" | "blob" | "base64",
                ) => &[
                    "type",
                    "source",
                    "data",
                    "mimeType",
                    "mime_type",
                    "media_type",
                    "resource",
                    "url",
                    "uri",
                    "name",
                    "title",
                    "description",
                    "size",
                    "text",
                    "blob",
                    "file",
                    "file_data",
                    "file_id",
                    "image_url",
                    "video_url",
                    "file_url",
                    "audio",
                    "input_audio",
                    "annotations",
                ],
                _ => &[],
            },
        }
    }
}

pub(crate) fn identifier_shape(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 256
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-.:|".contains(&byte))
}

/// Verified native records can mask entropy-only findings without hiding credential signatures.
///
/// Identity fields and runtime-opaque carriers (encrypted reasoning, thinking signatures) are
/// both masked under that rule; a carrier whose text also matches a credential rule keeps its
/// text, so the signature is still reported.
pub(crate) fn mask_entropy(value: &mut Value) {
    let layout = layout(value);
    layout.map(value, &mut |text, _| {
        if identifier_shape(text) && entropy_only(text) {
            return "0".repeat(text.len());
        }
        text.into()
    });
    for pointer in &layout.opaque {
        if let Some(carrier) = value.pointer_mut(pointer)
            && let Some(text) = carrier.as_str()
            && entropy_only(text)
        {
            *carrier = Value::String("0".repeat(text.len()));
        }
    }
}

fn entropy_only(text: &str) -> bool {
    let scan = crate::domain::privacy::detector::scan(text, 1024);
    scan.complete
        && !scan.findings.is_empty()
        && scan
            .findings
            .iter()
            .all(|hit| hit.rule == "high-entropy-value")
}

#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Identity,
    Call,
    CodexCall,
    ClaudeCall,
    Message,
    OpenCodeSession,
    OpenCodeMessage,
    OpenCodePart,
}

impl Kind {
    pub(crate) fn prefix(self) -> &'static str {
        match self {
            Self::Call | Self::CodexCall => "call_",
            Self::ClaudeCall => "toolu_",
            Self::Message | Self::OpenCodeMessage => "msg_",
            Self::OpenCodeSession => "ses_",
            Self::OpenCodePart => "prt_",
            Self::Identity => "",
        }
    }

    pub(crate) fn too_long(self, value: &str) -> bool {
        matches!(self, Self::CodexCall) && value.len() > 64
    }
}

pub(crate) struct Field {
    pub pointer: String,
    pub kind: Kind,
}

#[derive(Default)]
pub(crate) struct Layout {
    pub schema: bool,
    pub fields: Vec<Field>,
    pub encoded: Vec<(String, Layout)>,
    /// Structured payloads own their member names; native schema objects do not.
    pub data: Vec<String>,
    /// Tool results can mix ordinary maps with native text and attachment blocks.
    pub content: Vec<String>,
    /// Runtime-opaque carriers (reasoning signatures and encrypted reasoning). A runtime
    /// verifies them byte for byte on resume and no reader displays them, so privacy
    /// replacement never rewrites them.
    pub opaque: Vec<String>,
}

impl Layout {
    fn field(&mut self, pointer: impl Into<String>, kind: Kind) {
        self.schema = true;
        self.fields.push(Field {
            pointer: pointer.into(),
            kind,
        });
    }

    fn codex_item(&mut self, item: &Value, path: &str) {
        self.schema = true;
        for field in ["arguments", "input"] {
            self.data.push(format!("{path}/{field}"));
        }
        self.content.push(format!("{path}/output"));
        match item["type"].as_str() {
            Some(
                "function_call"
                | "custom_tool_call"
                | "local_shell_call"
                | "function_call_output"
                | "custom_tool_call_output"
                | "local_shell_call_output",
            ) => {
                self.field(format!("{path}/call_id"), Kind::CodexCall);
                self.field(format!("{path}/id"), Kind::Identity);
            }
            Some(
                "message"
                | "agent_message"
                | "reasoning"
                | "web_search_call"
                | "computer_call"
                | "image_generation_call"
                | "file_search_call"
                | "mcp_call"
                | "mcp_list_tools"
                | "mcp_approval_request",
            ) => {
                self.field(format!("{path}/id"), Kind::Identity);
            }
            Some("item_reference") => self.field(format!("{path}/id"), Kind::Identity),
            _ => {}
        }
        if item["encrypted_content"].is_string() {
            self.opaque.push(format!("{path}/encrypted_content"));
        }
        if item["type"] == "agent_message"
            && let Some(blocks) = item["content"].as_array()
        {
            for (index, block) in blocks.iter().enumerate() {
                if block["type"] == "encrypted_content" {
                    self.opaque
                        .push(format!("{path}/content/{index}/encrypted_content"));
                }
            }
        }
    }

    fn claude_message(&mut self, message: &Value) {
        self.field("/message/id", Kind::Message);
        if let Some(blocks) = message["content"].as_array() {
            for (index, block) in blocks.iter().enumerate() {
                match block["type"].as_str() {
                    Some("thinking") => self
                        .opaque
                        .push(format!("/message/content/{index}/signature")),
                    Some("redacted_thinking") => {
                        self.opaque.push(format!("/message/content/{index}/data"))
                    }
                    _ => {}
                }
                let field = match (message["role"].as_str(), block["type"].as_str()) {
                    (Some("assistant"), Some("tool_use" | "server_tool_use")) => {
                        self.data.push(format!("/message/content/{index}/input"));
                        "id"
                    }
                    (Some("user"), Some("tool_result")) => {
                        self.content
                            .push(format!("/message/content/{index}/content"));
                        self.data
                            .push(format!("/message/content/{index}/structuredContent"));
                        "tool_use_id"
                    }
                    (Some("assistant"), Some(kind)) if kind.ends_with("_tool_result") => {
                        self.content
                            .push(format!("/message/content/{index}/content"));
                        "tool_use_id"
                    }
                    _ => continue,
                };
                self.field(
                    format!("/message/content/{index}/{field}"),
                    Kind::ClaudeCall,
                );
            }
        }
    }

    fn openclaw_message(&mut self, message: &Value, path: &str) {
        if message["role"] == "toolResult" {
            self.field(format!("{path}/toolCallId"), Kind::Call);
            self.content.push(format!("{path}/content"));
        } else if message["role"] == "assistant"
            && let Some(blocks) = message["content"].as_array()
        {
            for (index, block) in blocks.iter().enumerate() {
                if block["type"] == "toolCall" {
                    self.field(format!("{path}/content/{index}/id"), Kind::Call);
                    self.data.push(format!("{path}/content/{index}/arguments"));
                }
            }
        }
    }

    fn hermes_calls(&mut self, calls: &Value, path: &str) {
        if let Some(calls) = calls.as_array() {
            for (index, call) in calls.iter().enumerate() {
                if call["function"].is_object() {
                    self.field(format!("{path}/{index}/id"), Kind::Call);
                    self.data.push(format!("{path}/{index}/function/arguments"));
                }
            }
        }
    }

    /// Encoded JSON is traversed only at schema-owned carriers. Arguments and outputs stay data.
    pub(crate) fn map(&self, value: &mut Value, f: &mut impl FnMut(&str, Kind) -> String) {
        for field in &self.fields {
            if let Some(value) = value.pointer_mut(&field.pointer)
                && let Some(text) = value.as_str()
            {
                let mapped = f(text, field.kind);
                if mapped != text {
                    *value = Value::String(mapped);
                }
            }
        }
        for (pointer, layout) in &self.encoded {
            if let Some(value) = value.pointer_mut(pointer)
                && let Some(text) = value.as_str()
                && let Ok(mut decoded) = serde_json::from_str::<Value>(text)
            {
                let before = decoded.clone();
                layout.map(&mut decoded, f);
                if decoded != before {
                    *value = Value::String(decoded.to_string());
                }
            }
        }
    }
}

pub(crate) fn layout(record: &Value) -> Layout {
    let mut ids = Layout::default();
    match record["type"].as_str() {
        Some("session_meta") if record["payload"].is_object() => {
            for field in ["id", "session_id", "forked_from_id", "parent_thread_id"] {
                ids.field(format!("/payload/{field}"), Kind::Identity);
            }
            ids.field("/payload/history_base/thread_id", Kind::Identity);
            ids.field(
                "/payload/source/subagent/thread_spawn/parent_thread_id",
                Kind::Identity,
            );
        }
        Some("response_item") => ids.codex_item(&record["payload"], "/payload"),
        Some("compacted") => {
            ids.schema = true;
            if let Some(items) = record["payload"]["replacement_history"].as_array() {
                for (index, item) in items.iter().enumerate() {
                    ids.codex_item(item, &format!("/payload/replacement_history/{index}"));
                }
            }
        }
        Some("turn_context" | "event_msg") if record["payload"].is_object() => {
            for field in ["turn_id", "item_id"] {
                ids.field(format!("/payload/{field}"), Kind::Identity);
            }
            if record["payload"]["type"] == "user_message" {
                ids.field("/payload/client_id", Kind::Identity);
            }
            if record["payload"]["type"] == "item_completed" {
                ids.field("/payload/item/id", Kind::Identity);
                if record["payload"]["item"]["type"] == "UserMessage" {
                    ids.field("/payload/item/client_id", Kind::Identity);
                }
            }
        }
        Some("user" | "assistant") if record["message"]["role"] == record["type"] => {
            for field in ["sessionId", "uuid", "parentUuid", "requestId"] {
                ids.field(format!("/{field}"), Kind::Identity);
            }
            ids.claude_message(&record["message"]);
        }
        Some("hermes_session") => {
            for field in ["id", "parent_session_id"] {
                ids.field(format!("/data/{field}"), Kind::Identity);
            }
        }
        Some("hermes_message") => {
            ids.field("/data/id", Kind::Identity);
            ids.field("/data/session_id", Kind::Identity);
            if record["data"]["role"] == "tool" {
                ids.field("/data/tool_call_id", Kind::Call);
                ids.content.push("/data/content".into());
            }
            if record["data"]["role"] == "assistant" {
                let calls = &record["data"]["tool_calls"];
                if let Some(text) = calls.as_str() {
                    let mut nested = Layout::default();
                    if let Ok(calls) = serde_json::from_str::<Value>(text) {
                        nested.hermes_calls(&calls, "");
                    }
                    ids.encoded.push(("/data/tool_calls".into(), nested));
                } else {
                    ids.hermes_calls(calls, "/data/tool_calls");
                }
            }
        }
        Some("function_call" | "function_call_result") if record["sessionId"].is_string() => {
            ids.field("/callId", Kind::Call);
            ids.field("/id", Kind::Identity);
            ids.field("/parentId", Kind::Identity);
            ids.data.push("/arguments".into());
            for field in ["result", "output"] {
                ids.content.push(format!("/{field}"));
            }
        }
        Some("system") if record["sessionId"].is_string() => {
            ids.field("/id", Kind::Identity);
            ids.field("/parentId", Kind::Identity);
        }
        Some("message" | "custom_message") => {
            ids.field("/id", Kind::Identity);
            ids.field("/parentId", Kind::Identity);
            let (message, path) = if record["type"] == "message" {
                (&record["message"], "/message")
            } else {
                (record, "")
            };
            ids.openclaw_message(message, path);
        }
        Some(
            "compaction"
            | "branch_summary"
            | "reset"
            | "thinking_level_change"
            | "model_change"
            | "custom"
            | "label"
            | "session_info",
        ) => {
            for field in ["id", "parentId", "firstKeptEntryId", "fromId"] {
                ids.field(format!("/{field}"), Kind::Identity);
            }
        }
        Some("leaf") => {
            for field in ["id", "parentId", "targetId", "appendParentId"] {
                ids.field(format!("/{field}"), Kind::Identity);
            }
        }
        _ => {}
    }
    match record["kind"].as_str() {
        Some("opencode.meta") => {
            ids.field("/id", Kind::OpenCodeSession);
            ids.field("/parent_id", Kind::OpenCodeSession);
            ids.field("/project_id", Kind::Identity);
        }
        Some(kind @ ("message" | "part")) if record["data"].is_object() => {
            ids.field(
                "/id",
                if kind == "message" {
                    Kind::OpenCodeMessage
                } else {
                    Kind::OpenCodePart
                },
            );
            ids.field("/session_id", Kind::OpenCodeSession);
            ids.field("/message_id", Kind::OpenCodeMessage);
            for field in ["id", "parentID", "messageID", "tail_start_id"] {
                let role = if field == "id" && kind == "part" {
                    Kind::OpenCodePart
                } else {
                    Kind::OpenCodeMessage
                };
                ids.field(format!("/data/{field}"), role);
            }
            ids.field("/data/sessionID", Kind::OpenCodeSession);
            if record["data"]["type"] == "tool" {
                ids.field("/data/callID", Kind::Call);
                for field in ["input", "metadata"] {
                    ids.data.push(format!("/data/state/{field}"));
                }
                ids.content.push("/data/state/output".into());
            }
        }
        _ => {}
    }
    ids
}
