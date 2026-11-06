//! Shared local daemon control messages.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Status,
    Stop,
    StopIfIdle {
        instance_id: String,
        build_id: String,
        /// The build that takes over, so the daemon's log can say it was replaced rather than end
        /// as if it crashed. A daemon that predates the field ignores it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        replacing_with: Option<String>,
    },
    ReloadSecrets,
    /// A Claude Code terminal loaded this conversation; `pid` is its process, and `loaded` the
    /// branch its plugin saw it continue, once it has.
    TerminalOpened {
        thread_id: String,
        pid: u32,
        plugin: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        loaded: Option<super::lease::Loaded>,
    },
    /// The headless writer of this conversation finished a turn on record `leaf`.
    WriterLeaf {
        thread_id: String,
        leaf: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Status(Status),
    Stopping,
    Busy {
        blockers: Vec<String>,
    },
    InstanceChanged,
    SecretsReloaded {
        generation: u64,
        rules: usize,
    },
    /// The terminal's lease state, absent when the daemon does not run this conversation, and
    /// the line the terminal shows while its own input must wait.
    TerminalOpened {
        state: Option<crate::protocol::LeaseState>,
        notice: Option<String>,
    },
    Recorded,
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<super::build_identity::DaemonIdentity>,
    pub pid: u32,
    pub hub: String,
    pub online: bool,
    pub uptime_secs: u64,
    pub agit_version: String,
    pub sessions: Vec<SessionLine>,
    /// Each Hub's Cloud connection as this daemon sees it. A daemon that predates the field sends
    /// none, which a reader takes as "can't tell", never as offline.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cloud: Vec<CloudLine>,
}

/// One Hub's Cloud connection: whether this device is enrolled there with inbound on, and
/// whether its presence socket is connected. `error` is a fixed class, never a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudLine {
    pub hub: String,
    pub enrolled: bool,
    pub pending: bool,
    pub presence: CloudPresence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudPresence {
    #[default]
    Connecting,
    Connected,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLine {
    pub session_id: String,
    pub runtime: String,
    pub status: String,
    pub last_seq: u64,
}
