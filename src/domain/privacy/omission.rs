//! Shared publication notices keep policy decisions distinct from unreadable source content.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Omission {
    ToolCall,
    ToolOutput,
    ToolSource,
    File,
    Image,
    Document,
    Audio,
    Video,
    Attachment,
    AttachmentMetadata,
    NativeRecord,
    MessageContent,
    ReasoningRole,
    HistoricalSource,
}

impl Omission {
    #[cfg(feature = "secret-vault")]
    pub fn reason(self) -> &'static str {
        match self {
            Self::ToolCall => "tool call is omitted by policy",
            Self::ToolOutput => "tool output is omitted by policy",
            Self::ToolSource => "tool source is ambiguous or unknown",
            Self::File => "file content is excluded by policy",
            Self::Image | Self::Document | Self::Audio | Self::Video | Self::Attachment => {
                "attachment body is excluded from public content"
            }
            Self::AttachmentMetadata => "attachment metadata retained without body",
            Self::NativeRecord => "unsupported native record",
            Self::MessageContent => "unsupported message content",
            Self::ReasoningRole => "reasoning is not owned by an assistant message",
            Self::HistoricalSource => "recovered evidence source is unavailable",
        }
    }

    pub fn text(self) -> &'static str {
        match self {
            Self::ToolCall => "[Tool call omitted by policy]",
            Self::ToolOutput => "[Tool output omitted by policy]",
            Self::ToolSource => "[Tool content unavailable: source could not be verified]",
            Self::File => "[File content excluded by policy]",
            Self::Image => "[Image omitted by policy]",
            Self::Document => "[Document omitted by policy]",
            Self::Audio => "[Audio omitted by policy]",
            Self::Video => "[Video omitted by policy]",
            Self::Attachment => "[Attachment omitted by policy]",
            Self::AttachmentMetadata => "[Attachment body omitted; metadata retained]",
            Self::NativeRecord => "[Content unavailable: unsupported message format]",
            Self::MessageContent => "[Content unavailable: unsupported content block]",
            Self::ReasoningRole => {
                "[Reasoning unavailable: assistant source could not be verified]"
            }
            Self::HistoricalSource => "[Historical content unavailable: source record missing]",
        }
    }
}

pub(crate) fn is_notice(text: &str) -> bool {
    text == "[Content omitted by privacy policy]"
        || [
            Omission::ToolCall,
            Omission::ToolOutput,
            Omission::ToolSource,
            Omission::File,
            Omission::Image,
            Omission::Document,
            Omission::Audio,
            Omission::Video,
            Omission::Attachment,
            Omission::AttachmentMetadata,
            Omission::NativeRecord,
            Omission::MessageContent,
            Omission::ReasoningRole,
            Omission::HistoricalSource,
        ]
        .iter()
        .any(|reason| reason.text() == text)
        || (text.starts_with("[Attachment body omitted; metadata retained: ")
            && text.ends_with(']'))
}
