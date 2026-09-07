//! Shared local daemon control messages.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Status,
    Stop,
    ReloadSecrets,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Status(Status),
    Stopping,
    SecretsReloaded { generation: u64, rules: usize },
    Error { message: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub pid: u32,
    pub hub: String,
    pub online: bool,
    pub connection_id: Option<String>,
    pub uptime_secs: u64,
    pub agit_version: String,
    pub sessions: Vec<SessionLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLine {
    pub session_id: String,
    pub runtime: String,
    pub status: String,
    pub last_seq: u64,
}
