//! Admission refusals reach the controller as typed RPC errors before its tunnel closes.
//!
//! A silent close is indistinguishable from a network failure, so a controller that is refused
//! for a permanent reason would redial forever. The refusal names its reason and says whether
//! the same lease can succeed later; only the diagnostic detail stays on this machine.

use crate::protocol::{ErrorCode, RpcError};
use std::time::Duration;

/// How long a permanent refusal answers the same controller scope without a new admission.
const PERMANENT_RETENTION: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, thiserror::Error)]
#[error("{detail}")]
pub(crate) struct Refusal {
    code: ErrorCode,
    reason: &'static str,
    message: &'static str,
    /// Present exactly when the same lease can be admitted once the condition clears.
    retry_after: Option<Duration>,
    /// A superseded lease is never admitted again, although its user can continue elsewhere.
    superseded: bool,
    detail: String,
}

impl Refusal {
    fn transient(reason: &'static str, message: &'static str, retry_after: Duration) -> Self {
        Self {
            code: ErrorCode::SessionBusy,
            reason,
            message,
            retry_after: Some(retry_after),
            superseded: false,
            detail: message.into(),
        }
    }

    fn permanent(code: ErrorCode, reason: &'static str, message: &'static str) -> Self {
        Self {
            code,
            reason,
            message,
            retry_after: None,
            superseded: false,
            detail: message.into(),
        }
    }

    fn because(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    pub fn capacity(detail: impl Into<String>) -> Self {
        Self::transient(
            "capacity",
            "The device is serving its maximum number of connections; retry shortly.",
            Duration::from_secs(2),
        )
        .because(detail)
    }

    pub fn restoring() -> Self {
        Self::transient(
            "transcript_restoring",
            "The conversation history is being prepared; retry shortly.",
            Duration::from_secs(1),
        )
    }

    pub fn ownership_changed() -> Self {
        let mut refusal = Self::transient(
            "ownership_changed",
            "Another controller owns this conversation; retry shortly.",
            Duration::from_secs(2),
        )
        .because("controller ownership changed");
        refusal.superseded = true;
        refusal
    }

    fn unavailable(detail: impl Into<String>) -> Self {
        Self::transient(
            "admission_unavailable",
            "The device could not admit this connection yet; retry shortly.",
            Duration::from_secs(2),
        )
        .because(detail)
    }

    pub fn conversation_unavailable(detail: impl Into<String>) -> Self {
        Self::permanent(
            ErrorCode::SessionNotFound,
            "conversation_unavailable",
            "This conversation is no longer available on the device.",
        )
        .because(detail)
    }

    pub fn conversation_changed(detail: impl Into<String>) -> Self {
        Self::permanent(
            ErrorCode::SessionNotFound,
            "conversation_changed",
            "This conversation changed on the device; reopen it from the conversation list.",
        )
        .because(detail)
    }

    pub fn project_unavailable(detail: impl Into<String>) -> Self {
        Self::permanent(
            ErrorCode::WorkspaceNotFound,
            "project_unavailable",
            "This folder is no longer bound on the device.",
        )
        .because(detail)
    }

    pub fn invalid_grant(detail: impl Into<String>) -> Self {
        Self::permanent(
            ErrorCode::MalformedFrame,
            "invalid_grant",
            "The connection grant names conflicting resources.",
        )
        .because(detail)
    }

    /// Unclassified failures stay retryable: a terminal error is a promise that retrying the
    /// same lease cannot help, and only a recognized cause can make it.
    pub fn classify(error: &anyhow::Error) -> Self {
        if let Some(refusal) = error.chain().find_map(|cause| cause.downcast_ref::<Self>()) {
            return refusal.clone();
        }
        if error
            .chain()
            .any(|cause| cause.is::<crate::adapter::codex_rollout::Restoring>())
        {
            return Self::restoring();
        }
        Self::unavailable(error.to_string())
    }

    /// A conversation that cannot be located is gone unless the failure is a passing I/O
    /// condition or a restoration still in progress.
    pub fn locating(error: anyhow::Error) -> anyhow::Error {
        let passing = error.chain().any(|cause| {
            cause.is::<crate::adapter::codex_rollout::Restoring>()
                || cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
                    !matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound
                            | std::io::ErrorKind::PermissionDenied
                            | std::io::ErrorKind::InvalidData
                            | std::io::ErrorKind::InvalidInput
                    )
                })
        });
        if passing {
            error
        } else {
            Self::conversation_unavailable(error.to_string()).into()
        }
    }

    pub fn reason(&self) -> &'static str {
        self.reason
    }

    pub fn retryable(&self) -> bool {
        self.retry_after.is_some()
    }

    pub fn code(&self) -> i32 {
        self.code.code()
    }

    /// How long the same controller lease may be answered from memory. A superseded lease is
    /// refused for as long as a permanent condition: its generation can never be admitted again.
    /// Capacity is not remembered, because a departing client frees it at once.
    pub fn retention(&self) -> Option<Duration> {
        match self.retry_after {
            _ if self.reason == "capacity" => None,
            Some(retry_after) if !self.superseded => Some(retry_after),
            _ => Some(PERMANENT_RETENTION),
        }
    }

    pub fn rpc_error(&self) -> RpcError {
        let mut error = RpcError::new(self.code, self.message);
        let mut data = serde_json::json!({
            "kind": "admission_refused",
            "reason": self.reason,
            "retryable": self.retryable(),
            "outcome": "not_sent",
        });
        if let Some(retry_after) = self.retry_after {
            data["retry_after_ms"] = serde_json::json!(retry_after.as_millis() as u64);
        }
        error.data = Some(data);
        error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tagged cause survives context layers, and a restoration is never reported as missing.
    #[test]
    fn classification_keeps_tagged_causes_and_restorations_retryable() {
        let tagged = anyhow::Error::from(Refusal::project_unavailable("unbound"))
            .context("admission failed");
        let refusal = Refusal::classify(&tagged);
        assert_eq!(refusal.reason(), "project_unavailable");
        assert!(!refusal.retryable());
        let restoring = Refusal::locating(crate::adapter::codex_rollout::Restoring.into());
        assert!(Refusal::classify(&restoring).retryable());
        let missing = Refusal::locating(
            anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::NotFound))
                .context("native conversation transcript is unavailable"),
        );
        assert_eq!(
            Refusal::classify(&missing).reason(),
            "conversation_unavailable"
        );
        let unknown = Refusal::classify(&anyhow::anyhow!("roster is locked"));
        assert!(unknown.retryable());
        assert_eq!(unknown.rpc_error().data.unwrap()["retry_after_ms"], 2000);
    }
}
