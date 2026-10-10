//! Identifier roles belong to native record schemas, never to arbitrary nested tool data.

use serde_json::Value;

pub(crate) fn identifier_shape(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 256
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-.:|".contains(&byte))
}

/// Verified native records can mask entropy-only findings without hiding credential signatures.
pub(crate) fn mask_entropy(value: &mut Value) {
    layout(value).map(value, &mut |text, _| {
        if identifier_shape(text) {
            let scan = crate::domain::privacy::detector::scan(text, 1024);
            if scan.complete
                && !scan.findings.is_empty()
                && scan
                    .findings
                    .iter()
                    .all(|hit| hit.rule == "high-entropy-value")
            {
                return "0".repeat(text.len());
            }
        }
        text.into()
    });
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
    pub fields: Vec<Field>,
    pub encoded: Vec<(String, Layout)>,
}

impl Layout {
    fn field(&mut self, pointer: impl Into<String>, kind: Kind) {
        self.fields.push(Field {
            pointer: pointer.into(),
            kind,
        });
    }

    fn codex_item(&mut self, item: &Value, path: &str) {
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
    }

    fn claude_message(&mut self, message: &Value) {
        self.field("/message/id", Kind::Message);
        if let Some(blocks) = message["content"].as_array() {
            for (index, block) in blocks.iter().enumerate() {
                let field = match (message["role"].as_str(), block["type"].as_str()) {
                    (Some("assistant"), Some("tool_use" | "server_tool_use")) => "id",
                    (Some("user"), Some("tool_result")) => "tool_use_id",
                    (Some("assistant"), Some(kind)) if kind.ends_with("_tool_result") => {
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
        } else if message["role"] == "assistant"
            && let Some(blocks) = message["content"].as_array()
        {
            for (index, block) in blocks.iter().enumerate() {
                if block["type"] == "toolCall" {
                    self.field(format!("{path}/content/{index}/id"), Kind::Call);
                }
            }
        }
    }

    fn hermes_calls(&mut self, calls: &Value, path: &str) {
        if let Some(calls) = calls.as_array() {
            for (index, call) in calls.iter().enumerate() {
                if call["function"].is_object() {
                    self.field(format!("{path}/{index}/id"), Kind::Call);
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
            for field in ["id", "session_id", "forked_from_id"] {
                ids.field(format!("/payload/{field}"), Kind::Identity);
            }
            ids.field("/payload/history_base/thread_id", Kind::Identity);
        }
        Some("response_item") => ids.codex_item(&record["payload"], "/payload"),
        Some("compacted") => {
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
            }
        }
        _ => {}
    }
    ids
}
