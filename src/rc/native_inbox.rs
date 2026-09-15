//! Decode legacy inbox requests without granting control of external sessions.

use anyhow::ensure;
use serde::Deserialize;

pub const MAX_MESSAGE: usize = 16 * 1024;

#[derive(Debug, Deserialize)]
pub struct Request {
    pub workspace_id: String,
    pub session_id: String,
    pub client_msg_id: String,
    pub message: String,
}

impl Request {
    pub fn validate(&self) -> crate::Result<()> {
        ensure!(
            valid_id(&self.session_id) && valid_id(&self.client_msg_id),
            "an exact session UUID and client message UUID are required"
        );
        ensure!(
            !self.message.trim().is_empty() && self.message.len() <= MAX_MESSAGE,
            "the message must be nonempty and fit the native inbox limit"
        );
        Ok(())
    }
}

pub fn valid_id(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|id| id.to_string() == value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_size_are_validated() {
        let mut value = Request {
            workspace_id: "workspace".into(),
            session_id: uuid::Uuid::new_v4().to_string(),
            client_msg_id: uuid::Uuid::new_v4().to_string(),
            message: "Synthetic input".into(),
        };
        assert!(value.validate().is_ok());
        value.session_id = "a session name".into();
        assert!(value.validate().is_err());
        value.session_id = uuid::Uuid::new_v4().to_string();
        value.message = "x".repeat(MAX_MESSAGE + 1);
        assert!(value.validate().is_err());
    }
}
